//! Embedded official `yt-dlp-ejs` 0.8.0 challenge solver running in-process on `quickjs-jit`.
//!
//! Fetches and caches YouTube's official `tv-player-ias.js` bundle (`_DEFAULT_PLAYER_JS_VARIANT = 'tv'`),
//! preprocesses it once via `yt.solver.lib.min.js` (`meriyah` + `astring`) and `yt.solver.core.min.js`
//! (`jsc`), persists the preprocessed player to `<cache_dir>/yt-ejs-player.json` when available,
//! and batch-solves `sig` (`signatureCipher`) and `n` throttle challenges with zero external runtimes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rquickjs::{CatchResultExt, CaughtError, Context, Function, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::installer::atomic_write;

/// Official `yt-dlp-ejs` 0.8.0 library bundle (`meriyah` + `astring`).
pub const YT_EJS_LIB_JS: &str = include_str!("../assets/yt_dlp_ejs/yt.solver.lib.min.js");
/// Official `yt-dlp-ejs` 0.8.0 core solver (`jsc`).
pub const YT_EJS_CORE_JS: &str = include_str!("../assets/yt_dlp_ejs/yt.solver.core.min.js");

const IFRAME_API_URL: &str = "https://www.youtube.com/iframe_api";
const PLAYER_FETCH_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const PLAYER_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const PLAYER_CACHE_FILE: &str = "yt-ejs-player.json";

const EJS_STACK_SIZE: usize = 16 * 1024 * 1024;
const EJS_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const EJS_JS_STACK_SIZE: usize = 8 * 1024 * 1024;
const EJS_COLD_DEADLINE_MS: u64 = 30_000;
const EJS_WARM_DEADLINE_MS: u64 = 8_000;

/// Fetched or cached YouTube `tv-player-ias.js` metadata and script payload.
#[derive(Debug, Clone)]
pub struct TvPlayerBundle {
    pub player_id: String,
    pub sts: u32,
    pub raw_player: Option<String>,
    pub preprocessed_player: Option<String>,
}

/// Solved `sig` and `n` challenge mappings returned by [`solve_challenges`].
#[derive(Debug, Default, Clone)]
pub struct EjsSolveResult {
    pub sigs: HashMap<String, String>,
    pub ns: HashMap<String, String>,
    pub cold: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskPlayerCache {
    player_id: String,
    sts: u32,
    preprocessed_player: String,
}

#[derive(Debug, Clone)]
struct MemPlayerCache {
    player_id: String,
    sts: u32,
    raw_player: Option<String>,
    preprocessed_player: Option<String>,
    fetched_at: Instant,
}

fn player_mem_cache() -> &'static Mutex<Option<MemPlayerCache>> {
    static CACHE: OnceLock<Mutex<Option<MemPlayerCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Extracts the YouTube player hash ID (typically 8 hex chars) from `/iframe_api` JS.
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

/// Extracts `signatureTimestamp` (`sts`) from `tv-player-ias.js` (`signatureTimestamp[:=](\d+)`).
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

fn load_disk_cache(cache_dir: &Path, expected_player_id: &str) -> Option<DiskPlayerCache> {
    let raw = std::fs::read_to_string(disk_cache_path(cache_dir)).ok()?;
    let parsed: DiskPlayerCache = serde_json::from_str(&raw).ok()?;
    if parsed.player_id == expected_player_id && !parsed.preprocessed_player.is_empty() {
        Some(parsed)
    } else {
        None
    }
}

fn save_disk_cache(cache_dir: &Path, player_id: &str, sts: u32, preprocessed_player: &str) {
    let payload = DiskPlayerCache {
        player_id: player_id.to_owned(),
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
    sts: u32,
    preprocessed: &str,
    cache_dir: Option<&Path>,
) {
    if let Ok(mut guard) = player_mem_cache().lock() {
        *guard = Some(MemPlayerCache {
            player_id: player_id.to_owned(),
            sts,
            raw_player: None,
            preprocessed_player: Some(preprocessed.to_owned()),
            fetched_at: Instant::now(),
        });
    }
    if let Some(dir) = cache_dir {
        save_disk_cache(dir, player_id, sts, preprocessed);
    }
}

/// Fetches or reuses a cached YouTube `tv-player-ias.js` bundle (`player_id`, `sts`, and
/// either `preprocessed_player` or `raw_player`).
pub async fn fetch_or_cached_tv_player(
    client: &reqwest::Client,
    cache_dir: Option<&Path>,
    timeout: Duration,
) -> Result<TvPlayerBundle, String> {
    // 1. Fast path: in-memory cache within 30-min TTL.
    if let Ok(guard) = player_mem_cache().lock()
        && let Some(ref cached) = *guard
        && cached.fetched_at.elapsed() < PLAYER_CACHE_TTL
        && (cached.preprocessed_player.is_some() || cached.raw_player.is_some())
    {
        return Ok(TvPlayerBundle {
            player_id: cached.player_id.clone(),
            sts: cached.sts,
            raw_player: cached.raw_player.clone(),
            preprocessed_player: cached.preprocessed_player.clone(),
        });
    }

    // 2. Discover current `player_id` from `/iframe_api`.
    let api_timeout = timeout.min(Duration::from_secs(8));
    let iframe_resp = tokio::time::timeout(
        api_timeout,
        client
            .get(IFRAME_API_URL)
            .header("user-agent", PLAYER_FETCH_UA)
            .timeout(api_timeout)
            .send(),
    )
    .await
    .map_err(|_| "iframe_api request timed out".to_owned())?
    .map_err(|e| format!("iframe_api request failed: {e}"))?;

    if !iframe_resp.status().is_success() {
        return Err(format!("iframe_api HTTP {}", iframe_resp.status()));
    }
    let iframe_js = tokio::time::timeout(api_timeout, iframe_resp.text())
        .await
        .map_err(|_| "iframe_api body timed out".to_owned())?
        .map_err(|e| format!("iframe_api read failed: {e}"))?;

    let player_id = extract_player_id_from_iframe_api(&iframe_js)
        .ok_or_else(|| "could not extract player_id from youtube iframe_api".to_owned())?;

    // 3. Re-check memory cache by `player_id` (refresh TTL if unchanged).
    if let Ok(mut guard) = player_mem_cache().lock()
        && let Some(ref mut cached) = *guard
        && cached.player_id == player_id
        && (cached.preprocessed_player.is_some() || cached.raw_player.is_some())
    {
        cached.fetched_at = Instant::now();
        return Ok(TvPlayerBundle {
            player_id: cached.player_id.clone(),
            sts: cached.sts,
            raw_player: cached.raw_player.clone(),
            preprocessed_player: cached.preprocessed_player.clone(),
        });
    }

    // 4. Check disk cache (`<cache_dir>/yt-ejs-player.json`) for matching `player_id`.
    if let Some(dir) = cache_dir
        && let Some(disk) = load_disk_cache(dir, &player_id)
    {
        log::info!(
            target: "nsh_music_plugin",
            "yt-ejs loaded preprocessed player from disk cache player_id={} sts={} bytes={}",
            disk.player_id,
            disk.sts,
            disk.preprocessed_player.len()
        );
        if let Ok(mut guard) = player_mem_cache().lock() {
            *guard = Some(MemPlayerCache {
                player_id: disk.player_id.clone(),
                sts: disk.sts,
                raw_player: None,
                preprocessed_player: Some(disk.preprocessed_player.clone()),
                fetched_at: Instant::now(),
            });
        }
        return Ok(TvPlayerBundle {
            player_id: disk.player_id,
            sts: disk.sts,
            raw_player: None,
            preprocessed_player: Some(disk.preprocessed_player),
        });
    }

    // 5. Download `tv-player-ias.js` (official yt-dlp `_DEFAULT_PLAYER_JS_VARIANT = 'tv'`).
    let player_url =
        format!("https://www.youtube.com/s/player/{player_id}/tv-player-ias.vflset/tv-player-ias.js");
    let fetch_timeout = timeout.min(Duration::from_secs(15));
    let dl_start = Instant::now();
    let player_resp = tokio::time::timeout(
        fetch_timeout,
        client
            .get(&player_url)
            .header("user-agent", PLAYER_FETCH_UA)
            .timeout(fetch_timeout)
            .send(),
    )
    .await
    .map_err(|_| "tv-player-ias.js request timed out".to_owned())?
    .map_err(|e| format!("tv-player-ias.js request failed: {e}"))?;

    if !player_resp.status().is_success() {
        return Err(format!("tv-player-ias.js HTTP {}", player_resp.status()));
    }
    let raw_player = tokio::time::timeout(fetch_timeout, player_resp.text())
        .await
        .map_err(|_| "tv-player-ias.js body timed out".to_owned())?
        .map_err(|e| format!("tv-player-ias.js read failed: {e}"))?;

    let sts = extract_sts_from_player_js(&raw_player)
        .ok_or_else(|| format!("could not extract signatureTimestamp from player {player_id}"))?;

    log::info!(
        target: "nsh_music_plugin",
        "yt-ejs fetched tv-player-ias.js player_id={player_id} sts={sts} bytes={} in {}ms",
        raw_player.len(),
        dl_start.elapsed().as_millis()
    );

    if let Ok(mut guard) = player_mem_cache().lock() {
        *guard = Some(MemPlayerCache {
            player_id: player_id.clone(),
            sts,
            raw_player: Some(raw_player.clone()),
            preprocessed_player: None,
            fetched_at: Instant::now(),
        });
    }

    Ok(TvPlayerBundle {
        player_id,
        sts,
        raw_player: Some(raw_player),
        preprocessed_player: None,
    })
}

struct EjsJob {
    player_id: String,
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

struct YtEjsWorker {
    tx: mpsc::Sender<EjsJob>,
}

fn global_ejs_worker() -> Result<&'static YtEjsWorker, String> {
    static WORKER: OnceLock<Result<YtEjsWorker, String>> = OnceLock::new();
    WORKER
        .get_or_init(YtEjsWorker::spawn)
        .as_ref()
        .map_err(Clone::clone)
}

impl YtEjsWorker {
    fn spawn() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<EjsJob>();
        let (boot_tx, boot_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        thread::Builder::new()
            .name("nsh-yt-ejs".to_owned())
            .stack_size(EJS_STACK_SIZE)
            .spawn(move || {
                run_ejs_worker(rx, boot_tx);
            })
            .map_err(|e| format!("failed to spawn nsh-yt-ejs worker thread: {e}"))?;

        boot_rx
            .recv()
            .map_err(|_| "nsh-yt-ejs worker exited during boot".to_owned())??;

        Ok(Self { tx })
    }

    fn execute(&self, job: EjsJob) -> Result<WorkerSolveOutput, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let wait_timeout = Duration::from_millis(job.deadline_ms.saturating_add(2_000));
        let sent_job = EjsJob {
            reply: reply_tx,
            ..job
        };
        self.tx
            .send(sent_job)
            .map_err(|_| "nsh-yt-ejs worker channel closed".to_owned())?;
        reply_rx
            .recv_timeout(wait_timeout)
            .map_err(|_| "nsh-yt-ejs worker timed out waiting for result".to_owned())?
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
globalThis.__yt_ejs_solve = function(playerId, rawPlayer, preprocPlayer, requestsJson) {
    if (preprocPlayer && globalThis.__yt_ejs_cache.id !== playerId) {
        globalThis.__yt_ejs_cache = { id: playerId, code: preprocPlayer };
    }
    const requests = JSON.parse(requestsJson);
    if (globalThis.__yt_ejs_cache.id === playerId && globalThis.__yt_ejs_cache.code) {
        const res = jsc({
            type: "preprocessed",
            preprocessed_player: globalThis.__yt_ejs_cache.code,
            requests
        });
        return JSON.stringify({ cold: false, res });
    }
    if (!rawPlayer) {
        throw new Error("missing raw player script for " + playerId);
    }
    const res = jsc({
        type: "player",
        player: rawPlayer,
        requests,
        output_preprocessed: true
    });
    let outPreproc = null;
    if (res && typeof res.preprocessed_player === "string" && res.preprocessed_player.length > 0) {
        globalThis.__yt_ejs_cache = { id: playerId, code: res.preprocessed_player };
        outPreproc = res.preprocessed_player;
    }
    return JSON.stringify({
        cold: true,
        preprocessed_player: outPreproc,
        res: { type: res && res.type, responses: (res && res.responses) || [] }
    });
};
"#;

fn run_ejs_worker(rx: mpsc::Receiver<EjsJob>, boot_tx: mpsc::SyncSender<Result<(), String>>) {
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
    rt.set_interrupt_handler(Some(Box::new(move || {
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
            ctx.eval::<(), _>(YT_EJS_LIB_JS)
                .catch(&ctx)
                .map_err(|e| format!("yt.solver.lib.min.js boot failed: {}", format_qjs_caught(e)))?;
            ctx.eval::<(), _>("Object.assign(globalThis, lib);")
                .catch(&ctx)
                .map_err(|e| format!("Object.assign(globalThis, lib) failed: {}", format_qjs_caught(e)))?;
            ctx.eval::<(), _>(YT_EJS_CORE_JS)
                .catch(&ctx)
                .map_err(|e| format!("yt.solver.core.min.js boot failed: {}", format_qjs_caught(e)))?;
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
                    job.player_id.as_str(),
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

        if parsed_out.as_ref().is_ok_and(|o| o.cold) {
            rt.run_gc();
        }

        let _ = job.reply.send(parsed_out);
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

    let worker = global_ejs_worker()?;
    let (dummy_tx, _) = mpsc::sync_channel(1);
    let out = worker.execute(EjsJob {
        player_id: bundle.player_id.clone(),
        raw_player: bundle.raw_player.clone(),
        preprocessed_player: bundle.preprocessed_player.clone(),
        requests_json,
        deadline_ms,
        reply: dummy_tx,
    })?;

    if let Some(ref preproc) = out.preprocessed_player {
        update_cached_preprocessed(&bundle.player_id, bundle.sts, preproc, cache_dir);
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
        "yt-ejs solve player_id={} mode={} sigs={} ns={} elapsed={}ms",
        bundle.player_id,
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

        // Verify worker boots cleanly with embedded yt-dlp-ejs 0.8.0 lib + core.
        let _worker = global_ejs_worker().expect("embedded yt-dlp-ejs 0.8.0 should boot in QuickJS");

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
    #[ignore = "live network timing check against youtube.com tv-player-ias.js"]
    fn bench_live_tv_player_cold_and_warm() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();
        let cache_dir = std::env::temp_dir().join(format!(
            "nsh-yt-ejs-bench-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&cache_dir);

        let bundle = rt
            .block_on(fetch_or_cached_tv_player(
                &client,
                Some(&cache_dir),
                Duration::from_secs(20),
            ))
            .expect("fetch tv-player-ias.js");
        println!(
            "FETCHED player_id={} sts={} raw_len={:?}",
            bundle.player_id,
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

        // Fetch cached bundle (now has preprocessed_player) and run warm solve.
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
