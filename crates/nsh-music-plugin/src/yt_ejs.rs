//! Embedded official `yt-dlp-ejs` 0.8.0 challenge solver running in-process on `quickjs-jit`.
//!
//! Supports all upstream `yt-dlp` `_PLAYER_JS_VARIANT_MAP` variants (`tv`, `main`, `es5`, `es6`,
//! `tv_es6`, `tcc`, `tce`, `es6_tcc`, `es6_tce`, `phone`), defaulting to `"tv"` with automatic
//! fallback (`tv` -> `tv_es6` -> `es5`) and `/embed/{video_id}` discovery fallback when
//! `/iframe_api` fails. Caches preprocessed players in memory and on disk keyed by
//! `(player_id, variant)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rquickjs::{CatchResultExt, CaughtError, Context, Function, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::installer::atomic_write;

/// Official `yt-dlp-ejs` 0.8.0 library bundle (`meriyah` + `astring`).
pub const YT_EJS_LIB_JS: &str = include_str!("../assets/yt_dlp_ejs/yt.solver.lib.min.js");
/// Official `yt-dlp-ejs` 0.8.0 core solver (`jsc`).
pub const YT_EJS_CORE_JS: &str = include_str!("../assets/yt_dlp_ejs/yt.solver.core.min.js");

const IFRAME_API_URL: &str = "https://www.youtube.com/iframe_api";
const PLAYER_FETCH_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const PLAYER_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const PLAYER_CACHE_FILE: &str = "yt-ejs-player.json";
pub const DEFAULT_PLAYER_JS_VARIANT: &str = "tv";

const EJS_STACK_SIZE: usize = 16 * 1024 * 1024;
const EJS_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const EJS_JS_STACK_SIZE: usize = 8 * 1024 * 1024;
const EJS_COLD_DEADLINE_MS: u64 = 30_000;
const EJS_WARM_DEADLINE_MS: u64 = 8_000;

/// Full upstream `yt-dlp/yt-dlp/master` `_PLAYER_JS_VARIANT_MAP` (10 YouTube player variants).
pub const PLAYER_JS_VARIANT_MAP: &[(&str, &str)] = &[
    ("tv", "tv-player-ias.vflset/tv-player-ias.js"),
    ("main", "player_ias.vflset/en_US/base.js"),
    ("es5", "player_es5.vflset/en_US/base.js"),
    ("es6", "player_es6.vflset/en_US/base.js"),
    ("tv_es6", "tv-player-es6.vflset/tv-player-es6.js"),
    ("tcc", "player_ias_tcc.vflset/en_US/base.js"),
    ("tce", "player_ias_tce.vflset/en_US/base.js"),
    ("es6_tcc", "player_es6_tcc.vflset/en_US/base.js"),
    ("es6_tce", "player_es6_tce.vflset/en_US/base.js"),
    ("phone", "player-plasma-ias-phone-en_US.vflset/base.js"),
];

/// Resolves an upstream `player_js_variant` name into its relative YouTube `/s/player/<id>/` path.
pub fn player_js_variant_path(variant: &str) -> Option<&'static str> {
    let norm = variant.trim().to_ascii_lowercase();
    PLAYER_JS_VARIANT_MAP
        .iter()
        .find(|(k, _)| *k == norm)
        .map(|(_, path)| *path)
}

/// Fetched or cached YouTube player JS metadata and script payload.
#[derive(Debug, Clone)]
pub struct TvPlayerBundle {
    pub player_id: String,
    pub variant: String,
    pub sts: u32,
    pub raw_player: Option<String>,
    pub preprocessed_player: Option<String>,
}

impl TvPlayerBundle {
    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.player_id, self.variant)
    }
}

/// Solved `sig` and `n` challenge mappings returned by [`solve_challenges`].
#[derive(Debug, Default, Clone)]
pub struct EjsSolveResult {
    pub sigs: HashMap<String, String>,
    pub ns: HashMap<String, String>,
    pub cold: bool,
    pub elapsed_ms: u64,
}

fn default_variant_string() -> String {
    DEFAULT_PLAYER_JS_VARIANT.to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskPlayerCache {
    player_id: String,
    #[serde(default = "default_variant_string")]
    variant: String,
    sts: u32,
    preprocessed_player: String,
}

#[derive(Debug, Clone)]
struct MemPlayerEntry {
    player_id: String,
    variant: String,
    sts: u32,
    raw_player: Option<String>,
    preprocessed_player: Option<String>,
    fetched_at: Instant,
}

#[derive(Default)]
struct MemCacheState {
    latest_player_id: Option<(String, Instant)>,
    bundles: HashMap<(String, String), MemPlayerEntry>,
}

fn player_mem_cache() -> &'static Mutex<MemCacheState> {
    static CACHE: OnceLock<Mutex<MemCacheState>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(MemCacheState::default()))
}

/// Extracts the YouTube player hash ID (typically 8 hex chars) from `/iframe_api` or `/embed/` HTML/JS.
/// Matches both `/s/player/<hex>/` and escaped `player\/<hex>\/`.
pub fn extract_player_id_from_iframe_api(js: &str) -> Option<String> {
    for needle in ["/s/player/", r"player\/"] {
        let mut search = js;
        while let Some(pos) = search.find(needle) {
            let rest = &search[pos + needle.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(rest.len());
            let candidate = &rest[..end];
            if (6..=16).contains(&candidate.len()) {
                return Some(candidate.to_owned());
            }
            search = rest;
        }
    }
    None
}

/// Extracts `signatureTimestamp` (`sts`) from player JS (`signatureTimestamp[:=](\d+)`).
pub fn extract_sts_from_player_js(js: &str) -> Option<u32> {
    let needle = "signatureTimestamp";
    let mut search = js;
    while let Some(pos) = search.find(needle) {
        let rest = &search[pos + needle.len()..];
        let after_ws = rest.trim_start();
        if let Some(after_sep) = after_ws
            .strip_prefix(':')
            .or_else(|| after_ws.strip_prefix('='))
        {
            let digits_start = after_sep.trim_start();
            let digit_len = digits_start
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(digits_start.len());
            if digit_len >= 4
                && let Ok(sts) = digits_start[..digit_len].parse::<u32>()
            {
                return Some(sts);
            }
        }
        search = rest;
    }
    None
}

fn disk_cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(PLAYER_CACHE_FILE)
}

fn load_disk_cache(
    cache_dir: &Path,
    expected_player_id: &str,
    expected_variant: &str,
) -> Option<DiskPlayerCache> {
    let raw = std::fs::read_to_string(disk_cache_path(cache_dir)).ok()?;
    let parsed: DiskPlayerCache = serde_json::from_str(&raw).ok()?;
    if parsed.player_id == expected_player_id
        && parsed.variant == expected_variant
        && !parsed.preprocessed_player.is_empty()
    {
        Some(parsed)
    } else {
        None
    }
}

fn save_disk_cache(
    cache_dir: &Path,
    player_id: &str,
    variant: &str,
    sts: u32,
    preprocessed_player: &str,
) {
    let payload = DiskPlayerCache {
        player_id: player_id.to_owned(),
        variant: variant.to_owned(),
        sts,
        preprocessed_player: preprocessed_player.to_owned(),
    };
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        let _ = std::fs::create_dir_all(cache_dir);
        if let Err(e) = atomic_write(&disk_cache_path(cache_dir), &bytes) {
            log::warn!(
                target: "nsh_music_plugin",
                "yt-ejs failed to persist preprocessed player cache: {e}"
            );
        }
    }
}

fn update_cached_preprocessed(
    player_id: &str,
    variant: &str,
    sts: u32,
    preprocessed: &str,
    cache_dir: Option<&Path>,
) {
    if let Ok(mut guard) = player_mem_cache().lock() {
        guard.bundles.insert(
            (player_id.to_owned(), variant.to_owned()),
            MemPlayerEntry {
                player_id: player_id.to_owned(),
                variant: variant.to_owned(),
                sts,
                raw_player: None,
                preprocessed_player: Some(preprocessed.to_owned()),
                fetched_at: Instant::now(),
            },
        );
    }
    if let Some(dir) = cache_dir {
        save_disk_cache(dir, player_id, variant, sts, preprocessed);
    }
}

async fn discover_player_id(
    client: &reqwest::Client,
    video_id: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    if let Ok(guard) = player_mem_cache().lock()
        && let Some((ref pid, at)) = guard.latest_player_id
        && at.elapsed() < PLAYER_CACHE_TTL
    {
        return Ok(pid.clone());
    }

    let api_timeout = timeout.min(Duration::from_secs(8));
    let primary_err = match tokio::time::timeout(
        api_timeout,
        client
            .get(IFRAME_API_URL)
            .header("user-agent", PLAYER_FETCH_UA)
            .timeout(api_timeout)
            .send(),
    )
    .await
    {
        Ok(Ok(resp)) if resp.status().is_success() => {
            if let Ok(Ok(iframe_js)) = tokio::time::timeout(api_timeout, resp.text()).await
                && let Some(pid) = extract_player_id_from_iframe_api(&iframe_js)
            {
                if let Ok(mut guard) = player_mem_cache().lock() {
                    guard.latest_player_id = Some((pid.clone(), Instant::now()));
                }
                return Ok(pid);
            }
            "could not extract player_id from youtube iframe_api".to_owned()
        }
        Ok(Ok(resp)) => format!("iframe_api HTTP {}", resp.status()),
        Ok(Err(e)) => format!("iframe_api request failed: {e}"),
        Err(_) => "iframe_api request timed out".to_owned(),
    };

    // Fallback: discover player_id from `https://www.youtube.com/embed/{video_id}`.
    if let Some(vid) = video_id.filter(|v| !v.is_empty()) {
        let embed_url = format!("https://www.youtube.com/embed/{vid}");
        log::warn!(
            target: "nsh_music_plugin",
            "yt-ejs iframe_api failed ({primary_err}); trying embed fallback for video={vid}"
        );
        if let Ok(Ok(resp)) = tokio::time::timeout(
            api_timeout,
            client
                .get(&embed_url)
                .header("user-agent", PLAYER_FETCH_UA)
                .timeout(api_timeout)
                .send(),
        )
        .await
            && resp.status().is_success()
            && let Ok(Ok(html)) = tokio::time::timeout(api_timeout, resp.text()).await
            && let Some(pid) = extract_player_id_from_iframe_api(&html)
        {
            if let Ok(mut guard) = player_mem_cache().lock() {
                guard.latest_player_id = Some((pid.clone(), Instant::now()));
            }
            return Ok(pid);
        }
    }

    Err(primary_err)
}

/// Fetches or reuses the default `"tv"` YouTube player bundle (`tv-player-ias.js`).
#[allow(dead_code)]
pub async fn fetch_or_cached_tv_player(
    client: &reqwest::Client,
    cache_dir: Option<&Path>,
    timeout: Duration,
) -> Result<TvPlayerBundle, String> {
    fetch_or_cached_tv_player_with_options(client, cache_dir, timeout, None, None, None).await
}

/// Fetches or reuses a cached YouTube player JS bundle with optional `variant` override
/// (any key in [`PLAYER_JS_VARIANT_MAP`]), `version_override` (`player_id`), and `video_id`
/// for `/embed/{video_id}` fallback discovery. Automatically falls back `tv` -> `tv_es6` -> `es5`
/// when downloading or extracting `sts` from `tv` fails.
pub async fn fetch_or_cached_tv_player_with_options(
    client: &reqwest::Client,
    cache_dir: Option<&Path>,
    timeout: Duration,
    variant: Option<&str>,
    version_override: Option<&str>,
    video_id: Option<&str>,
) -> Result<TvPlayerBundle, String> {
    let requested_variant = variant
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());

    let variant_chain: Vec<&'static str> = match requested_variant.as_deref() {
        Some(v) if v != DEFAULT_PLAYER_JS_VARIANT => {
            if let Some((canonical, _)) = PLAYER_JS_VARIANT_MAP.iter().find(|(k, _)| *k == v) {
                vec![canonical]
            } else {
                vec!["tv", "tv_es6", "es5"]
            }
        }
        _ => vec!["tv", "tv_es6", "es5"],
    };

    let player_id = if let Some(ov) = version_override.map(str::trim).filter(|s| !s.is_empty()) {
        ov.to_owned()
    } else {
        discover_player_id(client, video_id, timeout).await?
    };

    // 1. Check in-memory cache keyed by `(player_id, variant)`.
    if let Ok(mut guard) = player_mem_cache().lock() {
        for &var in &variant_chain {
            let key = (player_id.clone(), var.to_owned());
            if let Some(entry) = guard.bundles.get_mut(&key)
                && (entry.preprocessed_player.is_some() || entry.raw_player.is_some())
            {
                entry.fetched_at = Instant::now();
                return Ok(TvPlayerBundle {
                    player_id: entry.player_id.clone(),
                    variant: entry.variant.clone(),
                    sts: entry.sts,
                    raw_player: entry.raw_player.clone(),
                    preprocessed_player: entry.preprocessed_player.clone(),
                });
            }
        }
    }

    // 2. Check disk cache (`<cache_dir>/yt-ejs-player.json`) keyed by `(player_id, variant)`.
    if let Some(dir) = cache_dir {
        for &var in &variant_chain {
            if let Some(disk) = load_disk_cache(dir, &player_id, var) {
                log::info!(
                    target: "nsh_music_plugin",
                    "yt-ejs loaded preprocessed player from disk cache player_id={} variant={} sts={} bytes={}",
                    disk.player_id,
                    disk.variant,
                    disk.sts,
                    disk.preprocessed_player.len()
                );
                if let Ok(mut guard) = player_mem_cache().lock() {
                    guard.bundles.insert(
                        (disk.player_id.clone(), disk.variant.clone()),
                        MemPlayerEntry {
                            player_id: disk.player_id.clone(),
                            variant: disk.variant.clone(),
                            sts: disk.sts,
                            raw_player: None,
                            preprocessed_player: Some(disk.preprocessed_player.clone()),
                            fetched_at: Instant::now(),
                        },
                    );
                }
                return Ok(TvPlayerBundle {
                    player_id: disk.player_id,
                    variant: disk.variant,
                    sts: disk.sts,
                    raw_player: None,
                    preprocessed_player: Some(disk.preprocessed_player),
                });
            }
        }
    }

    // 3. Download player JS following `variant_chain` (`tv` -> `tv_es6` -> `es5`, or explicit variant).
    let fetch_timeout = timeout.min(Duration::from_secs(15));
    let mut last_err = String::new();
    for &var in &variant_chain {
        let Some(rel_path) = player_js_variant_path(var) else {
            continue;
        };
        let player_url = format!("https://www.youtube.com/s/player/{player_id}/{rel_path}");
        let dl_start = Instant::now();
        let resp = match tokio::time::timeout(
            fetch_timeout,
            client
                .get(&player_url)
                .header("user-agent", PLAYER_FETCH_UA)
                .timeout(fetch_timeout)
                .send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                last_err = format!("player {var} request failed: {e}");
                log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
                continue;
            }
            Err(_) => {
                last_err = format!("player {var} request timed out");
                log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
                continue;
            }
        };

        if !resp.status().is_success() {
            last_err = format!("player {var} HTTP {}", resp.status());
            log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
            continue;
        }
        let raw_player = match tokio::time::timeout(fetch_timeout, resp.text()).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                last_err = format!("player {var} read failed: {e}");
                log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
                continue;
            }
            Err(_) => {
                last_err = format!("player {var} body timed out");
                log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
                continue;
            }
        };

        let Some(sts) = extract_sts_from_player_js(&raw_player) else {
            last_err =
                format!("could not extract signatureTimestamp from player {player_id} ({var})");
            log::warn!(target: "nsh_music_plugin", "yt-ejs {last_err}");
            continue;
        };

        log::info!(
            target: "nsh_music_plugin",
            "yt-ejs fetched player_id={player_id} variant={var} sts={sts} bytes={} in {}ms",
            raw_player.len(),
            dl_start.elapsed().as_millis()
        );

        if let Ok(mut guard) = player_mem_cache().lock() {
            guard.bundles.insert(
                (player_id.clone(), var.to_owned()),
                MemPlayerEntry {
                    player_id: player_id.clone(),
                    variant: var.to_owned(),
                    sts,
                    raw_player: Some(raw_player.clone()),
                    preprocessed_player: None,
                    fetched_at: Instant::now(),
                },
            );
        }

        return Ok(TvPlayerBundle {
            player_id,
            variant: var.to_owned(),
            sts,
            raw_player: Some(raw_player),
            preprocessed_player: None,
        });
    }

    Err(if last_err.is_empty() {
        format!("failed to download player JS for {player_id}")
    } else {
        last_err
    })
}

struct EjsJob {
    cache_key: String,
    raw_player: Option<String>,
    preprocessed_player: Option<String>,
    requests_json: String,
    deadline_ms: u64,
    reply: mpsc::SyncSender<Result<WorkerSolveOutput, String>>,
}

struct WorkerSolveOutput {
    cold: bool,
    elapsed_ms: u64,
    preprocessed_player: Option<String>,
    responses: Vec<Value>,
}

#[derive(Clone)]
struct YtEjsWorker {
    id: u64,
    tx: mpsc::Sender<EjsJob>,
    abort_flag: Arc<AtomicBool>,
}

enum EjsExecError {
    ChannelClosed,
    DroppedReply,
    Poisoned(String),
    SolveError(String),
}

fn global_ejs_worker_slot() -> &'static Mutex<Option<YtEjsWorker>> {
    static WORKER: OnceLock<Mutex<Option<YtEjsWorker>>> = OnceLock::new();
    WORKER.get_or_init(|| Mutex::new(None))
}

fn reset_ejs_worker(failed_id: u64) {
    if let Ok(mut guard) = global_ejs_worker_slot().lock()
        && let Some(ref existing) = *guard
        && existing.id == failed_id
    {
        existing.abort_flag.store(true, Ordering::Relaxed);
        *guard = None;
    }
}

fn global_ejs_worker() -> Result<YtEjsWorker, String> {
    let mut guard = global_ejs_worker_slot()
        .lock()
        .map_err(|_| "nsh-yt-ejs worker mutex poisoned".to_owned())?;
    if let Some(ref existing) = *guard
        && !existing.abort_flag.load(Ordering::Relaxed)
    {
        return Ok(existing.clone());
    }
    let fresh = YtEjsWorker::spawn()?;
    *guard = Some(fresh.clone());
    Ok(fresh)
}

impl YtEjsWorker {
    fn spawn() -> Result<Self, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<EjsJob>();
        let (boot_tx, boot_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let abort_flag = Arc::new(AtomicBool::new(false));
        let abort_worker = Arc::clone(&abort_flag);

        thread::Builder::new()
            .name(format!("nsh-yt-ejs-{id}"))
            .stack_size(EJS_STACK_SIZE)
            .spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_ejs_worker(rx, boot_tx, abort_worker);
                }));
                if res.is_err() {
                    log::error!(
                        target: "nsh_music_plugin",
                        "nsh-yt-ejs worker #{id} panicked; will transparently respawn on next solve"
                    );
                }
            })
            .map_err(|e| format!("failed to spawn nsh-yt-ejs worker thread: {e}"))?;

        boot_rx
            .recv()
            .map_err(|_| "nsh-yt-ejs worker exited during boot".to_owned())??;

        Ok(Self { id, tx, abort_flag })
    }

    fn execute(&self, job: EjsJob) -> Result<WorkerSolveOutput, EjsExecError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let wait_timeout = Duration::from_millis(job.deadline_ms.saturating_add(2_000));
        let sent_job = EjsJob {
            reply: reply_tx,
            ..job
        };
        if self.tx.send(sent_job).is_err() {
            return Err(EjsExecError::ChannelClosed);
        }
        match reply_rx.recv_timeout(wait_timeout) {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(msg)) => {
                if msg.to_ascii_lowercase().contains("interrupted") {
                    Err(EjsExecError::Poisoned(msg))
                } else {
                    Err(EjsExecError::SolveError(msg))
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(EjsExecError::DroppedReply),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(EjsExecError::Poisoned(
                "nsh-yt-ejs worker timed out waiting for result".to_owned(),
            )),
        }
    }
}

fn format_qjs_caught(err: CaughtError<'_>) -> String {
    match err {
        CaughtError::Exception(exc) => {
            let msg = exc.message().unwrap_or_else(|| exc.to_string());
            if let Some(stack) = exc.stack()
                && !stack.trim().is_empty()
            {
                return format!("{msg}\n{stack}");
            }
            msg
        }
        CaughtError::Value(v) => format!("{v:?}"),
        CaughtError::Error(e) => e.to_string(),
    }
}

const EJS_BRIDGE_JS: &str = r#"
globalThis.__yt_ejs_cache = { id: null, code: null };
globalThis.__yt_ejs_solve = function(cacheKey, rawPlayer, preprocPlayer, requestsJson) {
    if (preprocPlayer && globalThis.__yt_ejs_cache.id !== cacheKey) {
        globalThis.__yt_ejs_cache = { id: cacheKey, code: preprocPlayer };
    }
    const requests = JSON.parse(requestsJson);
    if (globalThis.__yt_ejs_cache.id === cacheKey && globalThis.__yt_ejs_cache.code) {
        const res = jsc({
            type: "preprocessed",
            preprocessed_player: globalThis.__yt_ejs_cache.code,
            requests
        });
        return JSON.stringify({ cold: false, res });
    }
    if (!rawPlayer) {
        throw new Error("missing raw player script for " + cacheKey);
    }
    const res = jsc({
        type: "player",
        player: rawPlayer,
        requests,
        output_preprocessed: true
    });
    let outPreproc = null;
    if (res && typeof res.preprocessed_player === "string" && res.preprocessed_player.length > 0) {
        globalThis.__yt_ejs_cache = { id: cacheKey, code: res.preprocessed_player };
        outPreproc = res.preprocessed_player;
    }
    return JSON.stringify({
        cold: true,
        preprocessed_player: outPreproc,
        res: { type: res && res.type, responses: (res && res.responses) || [] }
    });
};
"#;

fn run_ejs_worker(
    rx: mpsc::Receiver<EjsJob>,
    boot_tx: mpsc::SyncSender<Result<(), String>>,
    abort_flag: Arc<AtomicBool>,
) {
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            let _ = boot_tx.send(Err(format!("QuickJS Runtime::new failed: {e}")));
            return;
        }
    };
    rt.set_memory_limit(EJS_MEMORY_LIMIT);
    rt.set_max_stack_size(EJS_JS_STACK_SIZE);

    let boot_instant = Instant::now();
    let active_deadline_ms = Arc::new(AtomicU64::new(0));
    let dl_cb = Arc::clone(&active_deadline_ms);
    let abort_cb = Arc::clone(&abort_flag);
    rt.set_interrupt_handler(Some(Box::new(move || {
        if abort_cb.load(Ordering::Relaxed) {
            return true;
        }
        let dl = dl_cb.load(Ordering::Relaxed);
        dl != 0 && (boot_instant.elapsed().as_millis() as u64) >= dl
    })));

    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => {
            let _ = boot_tx.send(Err(format!("QuickJS Context::full failed: {e}")));
            return;
        }
    };

    let boot_res: Result<(), String> = ctx.with(|ctx| {
        let now_ms = boot_instant.elapsed().as_millis() as u64;
        active_deadline_ms.store(now_ms.saturating_add(15_000), Ordering::Relaxed);
        let res = (|| -> Result<(), String> {
            ctx.eval::<(), _>(YT_EJS_LIB_JS).catch(&ctx).map_err(|e| {
                format!("yt.solver.lib.min.js boot failed: {}", format_qjs_caught(e))
            })?;
            ctx.eval::<(), _>("Object.assign(globalThis, lib);")
                .catch(&ctx)
                .map_err(|e| {
                    format!(
                        "Object.assign(globalThis, lib) failed: {}",
                        format_qjs_caught(e)
                    )
                })?;
            ctx.eval::<(), _>(YT_EJS_CORE_JS).catch(&ctx).map_err(|e| {
                format!(
                    "yt.solver.core.min.js boot failed: {}",
                    format_qjs_caught(e)
                )
            })?;
            ctx.eval::<(), _>(EJS_BRIDGE_JS)
                .catch(&ctx)
                .map_err(|e| format!("EJS bridge boot failed: {}", format_qjs_caught(e)))?;
            Ok(())
        })();
        active_deadline_ms.store(0, Ordering::Relaxed);
        res
    });

    if let Err(err) = boot_res {
        let _ = boot_tx.send(Err(err));
        return;
    }
    let _ = boot_tx.send(Ok(()));

    while let Ok(job) = rx.recv() {
        let started = Instant::now();
        let now_ms = boot_instant.elapsed().as_millis() as u64;
        active_deadline_ms.store(
            now_ms.saturating_add(job.deadline_ms.max(1_000)),
            Ordering::Relaxed,
        );

        let eval_res: Result<String, String> = ctx.with(|ctx| {
            let solve_fn: Function<'_> = ctx
                .globals()
                .get("__yt_ejs_solve")
                .catch(&ctx)
                .map_err(|e| format!("missing __yt_ejs_solve: {}", format_qjs_caught(e)))?;
            let out: String = solve_fn
                .call((
                    job.cache_key.as_str(),
                    job.raw_player.as_deref(),
                    job.preprocessed_player.as_deref(),
                    job.requests_json.as_str(),
                ))
                .catch(&ctx)
                .map_err(|e| format!("EJS solve JS error: {}", format_qjs_caught(e)))?;
            Ok(out)
        });

        active_deadline_ms.store(0, Ordering::Relaxed);
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let parsed_out = eval_res.and_then(|json_str| {
            let val: Value = serde_json::from_str(&json_str)
                .map_err(|e| format!("invalid EJS JSON response: {e}"))?;
            let cold = val.get("cold").and_then(|v| v.as_bool()).unwrap_or(false);
            let preprocessed_player = val
                .get("preprocessed_player")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let responses = val
                .pointer("/res/responses")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(WorkerSolveOutput {
                cold,
                elapsed_ms,
                preprocessed_player,
                responses,
            })
        });

        let was_interrupted = parsed_out
            .as_ref()
            .is_err_and(|e| e.to_ascii_lowercase().contains("interrupted"));

        if parsed_out.as_ref().is_ok_and(|o| o.cold) {
            rt.run_gc();
        }

        let _ = job.reply.send(parsed_out);
        if was_interrupted || abort_flag.load(Ordering::Relaxed) {
            break;
        }
    }
}

/// Batch-solves unique `sig` and `n` challenges using `yt-dlp-ejs` 0.8.0 in QuickJS.
pub fn solve_challenges(
    bundle: &TvPlayerBundle,
    sig_challenges: &[String],
    n_challenges: &[String],
    cache_dir: Option<&Path>,
) -> Result<EjsSolveResult, String> {
    if sig_challenges.is_empty() && n_challenges.is_empty() {
        return Ok(EjsSolveResult::default());
    }

    let mut requests = Vec::new();
    let mut sig_idx: Option<usize> = None;
    let mut n_idx: Option<usize> = None;

    if !sig_challenges.is_empty() {
        sig_idx = Some(requests.len());
        requests.push(json!({
            "type": "sig",
            "challenges": sig_challenges
        }));
    }
    if !n_challenges.is_empty() {
        n_idx = Some(requests.len());
        requests.push(json!({
            "type": "n",
            "challenges": n_challenges
        }));
    }

    let requests_json = serde_json::to_string(&requests)
        .map_err(|e| format!("failed to serialize EJS requests: {e}"))?;

    let is_likely_cold = bundle.preprocessed_player.is_none();
    let deadline_ms = if is_likely_cold {
        EJS_COLD_DEADLINE_MS
    } else {
        EJS_WARM_DEADLINE_MS
    };

    let out = {
        let mut attempt = 0;
        loop {
            let worker = global_ejs_worker()?;
            let (dummy_tx, _) = mpsc::sync_channel(1);
            let job = EjsJob {
                cache_key: bundle.cache_key(),
                raw_player: bundle.raw_player.clone(),
                preprocessed_player: bundle.preprocessed_player.clone(),
                requests_json: requests_json.clone(),
                deadline_ms,
                reply: dummy_tx,
            };
            match worker.execute(job) {
                Ok(out) => break out,
                Err(EjsExecError::ChannelClosed | EjsExecError::DroppedReply) if attempt == 0 => {
                    reset_ejs_worker(worker.id);
                    attempt += 1;
                }
                Err(EjsExecError::ChannelClosed) => {
                    reset_ejs_worker(worker.id);
                    return Err("nsh-yt-ejs worker channel closed".to_owned());
                }
                Err(EjsExecError::DroppedReply) => {
                    reset_ejs_worker(worker.id);
                    return Err("nsh-yt-ejs worker dropped reply".to_owned());
                }
                Err(EjsExecError::Poisoned(msg)) => {
                    reset_ejs_worker(worker.id);
                    return Err(msg);
                }
                Err(EjsExecError::SolveError(msg)) => return Err(msg),
            }
        }
    };

    if let Some(ref preproc) = out.preprocessed_player {
        update_cached_preprocessed(
            &bundle.player_id,
            &bundle.variant,
            bundle.sts,
            preproc,
            cache_dir,
        );
    }

    let mut result = EjsSolveResult {
        sigs: HashMap::new(),
        ns: HashMap::new(),
        cold: out.cold,
        elapsed_ms: out.elapsed_ms,
    };

    if let Some(idx) = sig_idx {
        let resp = out
            .responses
            .get(idx)
            .ok_or_else(|| "missing EJS response for sig challenges".to_owned())?;
        extract_response_map(resp, "sig", &mut result.sigs)?;
    }
    if let Some(idx) = n_idx {
        let resp = out
            .responses
            .get(idx)
            .ok_or_else(|| "missing EJS response for n challenges".to_owned())?;
        extract_response_map(resp, "n", &mut result.ns)?;
    }

    log::info!(
        target: "nsh_music_plugin",
        "yt-ejs solve player_id={} variant={} mode={} sigs={} ns={} elapsed={}ms",
        bundle.player_id,
        bundle.variant,
        if result.cold { "cold" } else { "warm" },
        result.sigs.len(),
        result.ns.len(),
        result.elapsed_ms
    );

    Ok(result)
}

fn extract_response_map(
    resp: &Value,
    kind: &str,
    out: &mut HashMap<String, String>,
) -> Result<(), String> {
    let resp_type = resp.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if resp_type != "result" {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown EJS solver error");
        return Err(format!("EJS `{kind}` solver error: {err}"));
    }
    let Some(data) = resp.get("data").and_then(|v| v.as_object()) else {
        return Err(format!("EJS `{kind}` response missing data object"));
    };
    for (k, v) in data {
        if let Some(s) = v.as_str() {
            out.insert(k.clone(), s.to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_ejs_assets_boot_and_extract_helpers_work() {
        assert!(!YT_EJS_LIB_JS.is_empty());
        assert!(!YT_EJS_CORE_JS.is_empty());

        // Verify all 10 upstream player JS variants exist.
        for key in [
            "tv", "main", "es5", "es6", "tv_es6", "tcc", "tce", "es6_tcc", "es6_tce", "phone",
        ] {
            assert!(
                player_js_variant_path(key).is_some(),
                "missing variant `{key}`"
            );
        }
        assert_eq!(
            player_js_variant_path("tv"),
            Some("tv-player-ias.vflset/tv-player-ias.js")
        );
        assert_eq!(
            player_js_variant_path("tv_es6"),
            Some("tv-player-es6.vflset/tv-player-es6.js")
        );
        assert_eq!(
            player_js_variant_path("phone"),
            Some("player-plasma-ias-phone-en_US.vflset/base.js")
        );

        // Verify disk cache separates variants for the same player_id cleanly.
        let cache_dir =
            std::env::temp_dir().join(format!("nsh-yt-ejs-variant-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        save_disk_cache(&cache_dir, "deadbeef", "tv_es6", 20779, "/* preproc */");
        assert!(load_disk_cache(&cache_dir, "deadbeef", "tv_es6").is_some());
        assert!(load_disk_cache(&cache_dir, "deadbeef", "tv").is_none());
        let _ = std::fs::remove_dir_all(&cache_dir);

        // Verify worker boots cleanly with embedded yt-dlp-ejs 0.8.0 lib + core.
        let _worker =
            global_ejs_worker().expect("embedded yt-dlp-ejs 0.8.0 should boot in QuickJS");

        // Verify `extract_player_id_from_iframe_api` handles both unescaped and escaped JS strings.
        assert_eq!(
            extract_player_id_from_iframe_api(
                r#"var scriptUrl = 'https:\/\/www.youtube.com\/s\/player\/ab12cd34\/www-widgetapi.vflset\/www-widgetapi.js';"#
            )
            .as_deref(),
            Some("ab12cd34")
        );
        assert_eq!(
            extract_player_id_from_iframe_api(
                "https://www.youtube.com/s/player/9f8e7d6c/player_ias.vflset/en_US/base.js"
            )
            .as_deref(),
            Some("9f8e7d6c")
        );

        // Verify `extract_sts_from_player_js` matches both `:` and `=`.
        assert_eq!(
            extract_sts_from_player_js(r#"foo={signatureTimestamp:20185,bar:1}"#),
            Some(20185)
        );
        assert_eq!(
            extract_sts_from_player_js(r#"a.signatureTimestamp = 20341;"#),
            Some(20341)
        );
        assert_eq!(extract_sts_from_player_js("no timestamp here"), None);

        // Verify synthetic preprocessed player solves both `sig` and `n` through `jsc(...)`.
        let fake_bundle = TvPlayerBundle {
            player_id: "unit-test-preproc-001".to_owned(),
            variant: "tv".to_owned(),
            sts: 20185,
            raw_player: None,
            preprocessed_player: Some(
                r#"
                _result.sig = (s) => "SOLVED_SIG_" + s.split("").reverse().join("");
                _result.n = (n) => "SOLVED_N_" + n;
                "#
                .to_owned(),
            ),
        };
        let solved = solve_challenges(
            &fake_bundle,
            &["abc123".to_owned()],
            &["xyz789".to_owned()],
            None,
        )
        .expect("preprocessed solver should solve sig and n");
        assert!(!solved.cold);
        assert_eq!(
            solved.sigs.get("abc123").map(String::as_str),
            Some("SOLVED_SIG_321cba")
        );
        assert_eq!(
            solved.ns.get("xyz789").map(String::as_str),
            Some("SOLVED_N_xyz789")
        );
    }

    #[test]
    fn ejs_worker_interrupts_infinite_loop_and_self_heals_next_solve() {
        let initial_id = global_ejs_worker().expect("boot ejs worker").id;

        // Trigger a synchronous infinite loop inside a synthetic preprocessed player with a short deadline.
        let worker = global_ejs_worker().expect("ejs worker");
        let (dummy_tx, _) = mpsc::sync_channel(1);
        let loop_res = worker.execute(EjsJob {
            cache_key: "unit-test-inf-loop:tv".to_owned(),
            raw_player: None,
            preprocessed_player: Some(
                "_result.sig = (s) => { while (true) {} return s; };".to_owned(),
            ),
            requests_json: r#"[{"type":"sig","challenges":["loop1"]}]"#.to_owned(),
            deadline_ms: 1_000,
            reply: dummy_tx,
        });
        assert!(
            matches!(loop_res, Err(EjsExecError::Poisoned(_))),
            "infinite loop must be interrupted and marked poisoned"
        );
        reset_ejs_worker(worker.id);

        // The very next solve must transparently respawn a fresh healthy worker and succeed.
        let healed_id = global_ejs_worker().expect("respawn ejs worker").id;
        assert_ne!(
            healed_id, initial_id,
            "poisoned EJS worker must be replaced with a fresh instance"
        );
        let good_bundle = TvPlayerBundle {
            player_id: "unit-test-healed-001".to_owned(),
            variant: "tv".to_owned(),
            sts: 20185,
            raw_player: None,
            preprocessed_player: Some("_result.n = (n) => 'HEALED_' + n;".to_owned()),
        };
        let solved = solve_challenges(&good_bundle, &[], &["ok1".to_owned()], None)
            .expect("healed EJS worker must solve cleanly");
        assert_eq!(solved.ns.get("ok1").map(String::as_str), Some("HEALED_ok1"));
    }

    #[test]
    #[ignore = "live network timing check against youtube.com tv-player-ias.js"]
    fn bench_live_tv_player_cold_and_warm() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();
        let cache_dir =
            std::env::temp_dir().join(format!("nsh-yt-ejs-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        let bundle = rt
            .block_on(fetch_or_cached_tv_player(
                &client,
                Some(&cache_dir),
                Duration::from_secs(20),
            ))
            .expect("fetch tv-player-ias.js");
        println!(
            "FETCHED player_id={} variant={} sts={} raw_len={:?}",
            bundle.player_id,
            bundle.variant,
            bundle.sts,
            bundle.raw_player.as_ref().map(String::len)
        );

        let cold = solve_challenges(
            &bundle,
            &["ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef".to_owned()],
            &["testNChallenge01".to_owned()],
            Some(&cache_dir),
        )
        .expect("cold solve tv-player-ias.js");
        println!(
            "COLD solve: cold={} elapsed={}ms sigs={:?} ns={:?}",
            cold.cold, cold.elapsed_ms, cold.sigs, cold.ns
        );

        let cached_bundle = rt
            .block_on(fetch_or_cached_tv_player(
                &client,
                Some(&cache_dir),
                Duration::from_secs(10),
            ))
            .expect("cached bundle");
        assert!(cached_bundle.preprocessed_player.is_some());
        let warm = solve_challenges(
            &cached_bundle,
            &["ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef".to_owned()],
            &["testNChallenge02".to_owned()],
            Some(&cache_dir),
        )
        .expect("warm solve tv-player-ias.js");
        println!(
            "WARM solve: cold={} elapsed={}ms sigs={:?} ns={:?}",
            warm.cold, warm.elapsed_ms, warm.sigs, warm.ns
        );

        assert!(disk_cache_path(&cache_dir).is_file());
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
