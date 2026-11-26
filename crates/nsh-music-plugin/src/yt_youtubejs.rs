//! Embedded upstream `LuanRT/YouTube.js` (`youtubei.js@18.3.0`, MIT) engine running in-process
//! on a dedicated persistent `quickjs-jit` worker bridged to Rust `reqwest` + disk/memory cache.
//!
//! Replaces all handwritten InnerTube client definitions and `/youtubei/v1/player` protocol
//! assembly with official `YouTube.js` (`Innertube`, `Session`, `Actions`, `HTTPClient`,
//! `Player`, `Parser`, `Format`), paired with `yt-dlp/ejs` (`yt_ejs.rs`) dual deciphering.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rquickjs::function::Opt;
use rquickjs::{
    Array, ArrayBuffer, AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx,
    Error as JsError, Exception, Function, Object, Promise, TypedArray, Value,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc as tokio_mpsc;

use crate::crypto;
use crate::installer::atomic_write;
use crate::virtual_ytdlp::sha1_hex;
use crate::yt_format_selector::normalize_client_alias;

/// Embedded official `youtubei.js@18.3.0` (`youtubei.js/agnostic`) minified bundle (MIT).
pub const YOUTUBEI_MIN_JS: &str = include_str!("../assets/youtubei_js/youtubei.min.js");

const YTJS_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;
const YTJS_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const YTJS_JS_STACK_SIZE: usize = 8 * 1024 * 1024;
const YTJS_CACHE_SUBDIR: &str = "youtubejs_cache";
const YTJS_PLAYER_FETCH_CAP: Duration = Duration::from_secs(15);
const YTJS_API_FETCH_CAP: Duration = Duration::from_secs(8);

/// Safari desktop UA used when a plugin explicitly requests `web_safari` HLS playback.
pub(crate) const SAFARI_WEB_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15";

/// Canonical `YouTube.js` client cascade step descriptor (no hardcoded protocol versions/contexts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YtJsClientSpec {
    /// Logical key used by health cache, logging, and `yt-dlp` alias matching (e.g. `"web_music"`, `"tv"`, `"android_vr"`).
    pub key: &'static str,
    /// Upstream `YouTube.js` client identifier passed to `yt.getBasicInfo(videoId, { client })`
    /// (must be in `YouTube.js`'s `Constants.SUPPORTED_CLIENTS`).
    pub ytjs_client: &'static str,
    /// Whether this client supports browser cookie authentication.
    pub supports_cookies: bool,
    /// Optional UA override (only set for `web_safari` to trigger YouTube's Safari HLS path).
    pub user_agent_override: Option<&'static str>,
}

const ALL_YTJS_CLIENTS: &[YtJsClientSpec] = &[
    YtJsClientSpec {
        key: "web_music",
        ytjs_client: "YTMUSIC",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "web_safari",
        ytjs_client: "WEB",
        supports_cookies: true,
        user_agent_override: Some(SAFARI_WEB_UA),
    },
    YtJsClientSpec {
        key: "tv",
        ytjs_client: "TV",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "tv_simply",
        ytjs_client: "TV_SIMPLY",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "tv_embedded",
        ytjs_client: "TV_EMBEDDED",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "android_vr",
        ytjs_client: "ANDROID_VR",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "visionos",
        ytjs_client: "VISIONOS",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "ios",
        ytjs_client: "IOS",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "web_creator",
        ytjs_client: "WEB_CREATOR",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "web",
        ytjs_client: "WEB",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "mweb",
        ytjs_client: "MWEB",
        supports_cookies: true,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "web_embedded",
        ytjs_client: "WEB_EMBEDDED",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "android",
        ytjs_client: "ANDROID",
        supports_cookies: false,
        user_agent_override: None,
    },
    YtJsClientSpec {
        key: "android_music",
        ytjs_client: "YTMUSIC_ANDROID",
        supports_cookies: false,
        user_agent_override: None,
    },
];

/// Default client ordering when browser cookies are supplied: tries authenticated web/music/TV
/// clients first, then automatically falls back to anonymous mobile/VR clients without leaking cookies.
pub(crate) const DEFAULT_CLIENTS_WITH_COOKIE: &[&str] = &[
    "web_music",
    "web_safari",
    "tv",
    "android_vr",
    "visionos",
    "ios",
    "web_creator",
    "web",
    "mweb",
    "android",
];

/// Default client ordering when no cookies are supplied.
pub(crate) const DEFAULT_CLIENTS_ANONYMOUS: &[&str] = &[
    "web_music",
    "android_vr",
    "visionos",
    "ios",
    "tv",
    "android",
    "web",
    "mweb",
];

/// Resilient fallback clients appended after an explicitly requested `player_client=...` fails.
pub(crate) const RESILIENT_FALLBACK_CLIENTS: &[&str] = &[
    "web_music",
    "tv",
    "android_vr",
    "visionos",
    "ios",
    "web_creator",
    "web",
    "android",
];

/// Resolves a `yt-dlp` client name/alias to its [`YtJsClientSpec`].
pub(crate) fn lookup_ytjs_client(raw: &str) -> Option<&'static YtJsClientSpec> {
    let norm = normalize_client_alias(raw);
    let mapped = match norm.as_str() {
        "tv_samsung" | "tv_downgraded" => "tv",
        "android_sdkless" => "android",
        other => other,
    };
    ALL_YTJS_CLIENTS.iter().find(|c| c.key == mapped)
}

/// Expands `--extractor-args youtube:player_client=...` tokens (supporting `default`, `all`,
/// `-<client>` exclusions, and aliases) into an ordered deduplicated `YouTube.js` client list.
pub(crate) fn resolve_requested_ytjs_clients(
    requested: &[String],
    has_cookie: bool,
) -> Vec<&'static YtJsClientSpec> {
    let defaults = if has_cookie {
        DEFAULT_CLIENTS_WITH_COOKIE
    } else {
        DEFAULT_CLIENTS_ANONYMOUS
    };

    if requested.is_empty() {
        return defaults
            .iter()
            .filter_map(|k| lookup_ytjs_client(k))
            .collect();
    }

    let mut excluded: Vec<String> = Vec::new();
    let mut expanded: Vec<&'static YtJsClientSpec> = Vec::new();
    let mut saw_positive = false;

    for raw in requested {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(neg) = trimmed.strip_prefix('-') {
            let norm = normalize_client_alias(neg);
            if !norm.is_empty() {
                excluded.push(norm);
            }
            continue;
        }
        saw_positive = true;
        let norm = normalize_client_alias(trimmed);
        match norm.as_str() {
            "default" => {
                for &k in defaults {
                    if let Some(spec) = lookup_ytjs_client(k) {
                        expanded.push(spec);
                    }
                }
            }
            "all" => {
                for spec in ALL_YTJS_CLIENTS {
                    expanded.push(spec);
                }
            }
            _ => {
                if let Some(spec) = lookup_ytjs_client(&norm) {
                    expanded.push(spec);
                }
            }
        }
    }

    if !saw_positive {
        for &k in defaults {
            if let Some(spec) = lookup_ytjs_client(k) {
                expanded.push(spec);
            }
        }
    }

    let mut seen = Vec::new();
    let mut out = Vec::new();
    for spec in expanded {
        let is_excluded = excluded
            .iter()
            .any(|e| e == spec.key || lookup_ytjs_client(e).is_some_and(|s| s.key == spec.key));
        if is_excluded || seen.contains(&spec.key) {
            continue;
        }
        seen.push(spec.key);
        out.push(spec);
    }

    if out.is_empty() {
        for &k in defaults {
            if let Some(spec) = lookup_ytjs_client(k) {
                out.push(spec);
            }
        }
    }

    out
}

/// Request parameters sent into the persistent `YouTube.js` QuickJS engine for one client attempt.
#[derive(Debug, Clone)]
pub(crate) struct YtJsExtractRequest {
    pub video_id: String,
    pub ytjs_client: String,
    pub cookie: Option<String>,
    pub lang: Option<String>,
    pub visitor_data: Option<String>,
    pub po_token: Option<String>,
    pub user_agent_override: Option<String>,
    pub skip_hls: bool,
    pub loopback_origin: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub timeout: Duration,
}

/// Extracted format item returned from `YouTube.js` (`Format` + `format.decipher(player)`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct YtJsExtractedFormat {
    pub itag: u64,
    pub mime_type: String,
    pub url: Option<String>,
    pub raw_url: Option<String>,
    pub signature_cipher: Option<String>,
    pub cipher: Option<String>,
    pub deciphered_by_ytjs: bool,
    pub bitrate: u64,
    pub average_bitrate: Option<u64>,
    pub audio_sample_rate: Option<u64>,
    pub audio_channels: Option<u32>,
    pub content_length: Option<u64>,
    pub has_audio: bool,
    pub has_video: bool,
    pub is_muxed: bool,
    pub is_drc: bool,
    pub is_default_audio_track: bool,
    pub is_original_audio_track: bool,
}

/// Extraction outcome from a single `YouTube.js` client invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct YtJsExtractResponse {
    pub ok: bool,
    pub playability_status: String,
    pub playability_reason: String,
    pub error: Option<String>,
    pub visitor_data: String,
    pub user_agent: String,
    pub player_id: Option<String>,
    pub signature_timestamp: Option<u32>,
    pub title: String,
    pub duration_secs: Option<u64>,
    pub uploader: String,
    pub channel_id: String,
    pub thumbnail: String,
    pub hls_manifest_url: Option<String>,
    pub formats: Vec<YtJsExtractedFormat>,
}

struct ActiveBridgeContext {
    http_client: reqwest::Client,
    loopback_origin: Option<String>,
    cache_dir: Option<PathBuf>,
    deadline: Instant,
    last_player_ua: Option<String>,
}

struct YtJsJob {
    req: YtJsExtractRequest,
    http_client: reqwest::Client,
    reply: tokio::sync::oneshot::Sender<Result<YtJsExtractResponse, String>>,
}

struct YtJsWorkerHandle {
    tx: tokio_mpsc::UnboundedSender<YtJsJob>,
}

fn global_ytjs_worker() -> Result<&'static YtJsWorkerHandle, String> {
    static WORKER: OnceLock<Result<YtJsWorkerHandle, String>> = OnceLock::new();
    WORKER
        .get_or_init(YtJsWorkerHandle::spawn)
        .as_ref()
        .map_err(Clone::clone)
}

impl YtJsWorkerHandle {
    fn spawn() -> Result<Self, String> {
        let (tx, rx) = tokio_mpsc::unbounded_channel::<YtJsJob>();
        let (boot_tx, boot_rx) = std_mpsc::sync_channel::<Result<(), String>>(1);

        thread::Builder::new()
            .name("nsh-youtubejs".to_owned())
            .stack_size(YTJS_THREAD_STACK_SIZE)
            .spawn(move || {
                run_ytjs_worker(rx, boot_tx);
            })
            .map_err(|e| format!("failed to spawn nsh-youtubejs worker thread: {e}"))?;

        boot_rx
            .recv()
            .map_err(|_| "nsh-youtubejs worker exited during boot".to_owned())??;

        Ok(Self { tx })
    }
}

/// Runs one `YouTube.js` client extraction on the persistent `quickjs-jit` worker.
pub(crate) async fn extract_via_youtubejs(
    http_client: &reqwest::Client,
    req: YtJsExtractRequest,
) -> Result<YtJsExtractResponse, String> {
    let worker = global_ytjs_worker()?;
    let wait_budget = req.timeout.saturating_add(Duration::from_secs(2));
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    worker
        .tx
        .send(YtJsJob {
            req,
            http_client: http_client.clone(),
            reply: reply_tx,
        })
        .map_err(|_| "nsh-youtubejs worker channel closed".to_owned())?;

    match tokio::time::timeout(wait_budget, reply_rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => Err("nsh-youtubejs worker dropped reply".to_owned()),
        Err(_) => Err("YouTube.js extraction timed out".to_owned()),
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

fn rewrite_loopback_url(raw_url: &str, loopback_origin: Option<&str>) -> String {
    let Some(base) = loopback_origin.map(|s| s.trim_end_matches('/')) else {
        return raw_url.to_owned();
    };
    let Ok(parsed) = url::Url::parse(raw_url) else {
        return raw_url.to_owned();
    };
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "www.youtube.com" | "music.youtube.com" | "youtube.com" | "youtubei.googleapis.com"
    ) {
        let mut rewritten = format!("{base}{}", parsed.path());
        if let Some(q) = parsed.query() {
            rewritten.push('?');
            rewritten.push_str(q);
        }
        return rewritten;
    }
    raw_url.to_owned()
}

fn sanitize_cache_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len().min(96));
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        "default".to_owned()
    } else {
        out
    }
}

fn random_bytes(len: usize) -> Vec<u8> {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(seq);
        hasher.write_u128(now);
        let seed = format!("{:016x}:{seq}:{now}", hasher.finish());
        let digest = crypto::sha256_bytes(seed.as_bytes());
        let take = (len - out.len()).min(digest.len());
        out.extend_from_slice(&digest[..take]);
    }
    out
}

fn generate_uuid_v4() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn js_val_to_raw_bytes<'js>(val: Value<'js>) -> Option<Vec<u8>> {
    if val.is_undefined() || val.is_null() {
        return None;
    }
    if let Some(s) = val.as_string() {
        return s.to_string().ok().map(String::into_bytes);
    }
    if let Some(obj) = val.as_object() {
        if let Some(ta) = obj.as_typed_array::<u8>() {
            // SAFETY: QuickJS runtime is single-threaded here; slice is copied immediately.
            if let Some(slice) = unsafe { ta.as_bytes() } {
                return Some(slice.to_vec());
            }
        }
        if let Some(ab) = obj.as_array_buffer() {
            // SAFETY: QuickJS runtime is single-threaded here; slice is copied immediately.
            if let Some(slice) = unsafe { ab.as_bytes() } {
                return Some(slice.to_vec());
            }
        }
    }
    None
}

fn run_ytjs_worker(
    mut rx: tokio_mpsc::UnboundedReceiver<YtJsJob>,
    boot_tx: std_mpsc::SyncSender<Result<(), String>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = boot_tx.send(Err(format!("YouTube.js tokio init failed: {e}")));
            return;
        }
    };

    rt.block_on(async move {
        let qjs_rt = match AsyncRuntime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = boot_tx.send(Err(format!("YouTube.js QuickJS runtime init failed: {e}")));
                return;
            }
        };
        qjs_rt.set_memory_limit(YTJS_MEMORY_LIMIT).await;
        qjs_rt.set_max_stack_size(YTJS_JS_STACK_SIZE).await;

        let qjs_ctx = match AsyncContext::full(&qjs_rt).await {
            Ok(c) => c,
            Err(e) => {
                let _ = boot_tx.send(Err(format!("YouTube.js QuickJS context init failed: {e}")));
                return;
            }
        };

        let active_bridge: Arc<Mutex<Option<ActiveBridgeContext>>> = Arc::new(Mutex::new(None));
        let mem_cache: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let timer_cancels: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let boot_instant = Instant::now();

        let bridge_for_init = Arc::clone(&active_bridge);
        let cache_for_init = Arc::clone(&mem_cache);
        let timers_for_init = Arc::clone(&timer_cancels);

        let boot_res: Result<(), String> = qjs_ctx
            .async_with(async move |ctx| {
                install_ytjs_natives(
                    &ctx,
                    bridge_for_init,
                    cache_for_init,
                    timers_for_init,
                    boot_instant,
                )
                .catch(&ctx)
                .map_err(|e| {
                    format!(
                        "install YouTube.js natives failed: {}",
                        format_qjs_caught(e)
                    )
                })?;

                ctx.eval::<(), _>(YOUTUBEJS_POLYFILLS_JS)
                    .catch(&ctx)
                    .map_err(|e| {
                        format!("YouTube.js polyfills eval failed: {}", format_qjs_caught(e))
                    })?;

                ctx.eval::<(), _>(YOUTUBEI_MIN_JS)
                    .catch(&ctx)
                    .map_err(|e| {
                        format!(
                            "youtubei.min.js bundle eval failed: {}",
                            format_qjs_caught(e)
                        )
                    })?;

                ctx.eval::<(), _>(YOUTUBEJS_DRIVER_JS)
                    .catch(&ctx)
                    .map_err(|e| {
                        format!("YouTube.js driver init failed: {}", format_qjs_caught(e))
                    })?;

                Ok(())
            })
            .await;

        if let Err(err) = boot_res {
            let _ = boot_tx.send(Err(err));
            return;
        }
        let _ = boot_tx.send(Ok(()));

        while let Some(job) = rx.recv().await {
            let deadline = Instant::now() + job.req.timeout;
            if let Ok(mut guard) = active_bridge.lock() {
                *guard = Some(ActiveBridgeContext {
                    http_client: job.http_client,
                    loopback_origin: job.req.loopback_origin.clone(),
                    cache_dir: job.req.cache_dir.clone(),
                    deadline,
                    last_player_ua: None,
                });
            }

            let req_json = serde_json::json!({
                "videoId": job.req.video_id,
                "ytjsClient": job.req.ytjs_client,
                "cookie": job.req.cookie,
                "lang": job.req.lang,
                "visitorData": job.req.visitor_data,
                "poToken": job.req.po_token,
                "userAgentOverride": job.req.user_agent_override,
                "skipHls": job.req.skip_hls,
                "isLoopback": job.req.loopback_origin.is_some(),
            })
            .to_string();

            let bridge_ref = Arc::clone(&active_bridge);
            let run_timeout = job.req.timeout;
            let result: Result<YtJsExtractResponse, String> = qjs_ctx
                .async_with(async move |ctx| {
                    let globals = ctx.globals();
                    let extract_fn: Function<'_> = globals
                        .get("__nshYtJsExtract")
                        .catch(&ctx)
                        .map_err(|e| format_qjs_caught(e))?;
                    let promise: Promise<'_> = extract_fn
                        .call((req_json,))
                        .catch(&ctx)
                        .map_err(|e| format_qjs_caught(e))?;
                    let fut = promise.into_future::<String>();
                    let json_str = match tokio::time::timeout(run_timeout, fut).await {
                        Ok(res) => res.catch(&ctx).map_err(|e| format_qjs_caught(e))?,
                        Err(_) => return Err("YouTube.js extraction timed out".to_owned()),
                    };
                    let mut parsed: YtJsExtractResponse = serde_json::from_str(&json_str)
                        .map_err(|e| format!("YouTube.js invalid response JSON: {e}"))?;
                    if let Ok(guard) = bridge_ref.lock()
                        && let Some(ref active) = *guard
                        && let Some(ref captured_ua) = active.last_player_ua
                        && !captured_ua.is_empty()
                    {
                        parsed.user_agent = captured_ua.clone();
                    }
                    Ok(parsed)
                })
                .await;

            if let Ok(mut guard) = active_bridge.lock() {
                *guard = None;
            }
            let _ = job.reply.send(result);
        }

        let _ = tokio::time::timeout(Duration::from_millis(200), qjs_rt.idle()).await;
    });
}

fn install_ytjs_natives<'js>(
    ctx: &Ctx<'js>,
    active_bridge: Arc<Mutex<Option<ActiveBridgeContext>>>,
    mem_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    timer_cancels: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>>,
    boot_instant: Instant,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    // 1. Native Rust SHA-1 lowercase hex digest for `Platform.shim.sha1Hash` & multi-scheme SAPISIDHASH.
    globals.set(
        "__ytjsSha1Hex",
        Function::new(ctx.clone(), |data: String| -> String {
            sha1_hex(data.as_bytes())
        })?,
    )?;

    // 2. Native UUIDv4 & random bytes for `Platform.shim.uuidv4` and `crypto.getRandomValues`.
    globals.set(
        "__ytjsUuidV4",
        Function::new(ctx.clone(), generate_uuid_v4)?,
    )?;
    globals.set(
        "__ytjsRandomBytes",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, len: usize| -> rquickjs::Result<TypedArray<'js, u8>> {
                let capped = len.min(65_536);
                TypedArray::<u8>::new(ctx, random_bytes(capped))
            },
        )?,
    )?;

    // 3. Monotonic timer ms for `performance.now()`.
    globals.set(
        "__ytjsPerfNow",
        Function::new(ctx.clone(), move || -> f64 {
            boot_instant.elapsed().as_secs_f64() * 1000.0
        })?,
    )?;

    // 4. Logging bridge for `console.*`.
    globals.set(
        "__ytjsLog",
        Function::new(ctx.clone(), |level: String, msg: String| {
            match level.as_str() {
                "error" | "warn" => {
                    log::debug!(target: "nsh_music_plugin", "[youtubei.js:{level}] {msg}")
                }
                _ => log::trace!(target: "nsh_music_plugin", "[youtubei.js:{level}] {msg}"),
            }
        })?,
    )?;

    // 5. Latin-1 faithful `btoa` / `atob` (used by `YouTube.js` `u8ToBase64` / `base64ToU8` protobuf utils).
    globals.set(
        "__ytjsBtoa",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, input: String| -> rquickjs::Result<String> {
            let mut bytes = Vec::with_capacity(input.len());
            for ch in input.chars() {
                let code = ch as u32;
                if code > 0xff {
                    return Err(Exception::throw_message(
                        &ctx,
                        "InvalidCharacterError: btoa string contains characters outside Latin1 range",
                    ));
                }
                bytes.push(code as u8);
            }
            Ok(crypto::base64_encode(&bytes))
        })?,
    )?;
    globals.set(
        "__ytjsAtob",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: String| -> rquickjs::Result<String> {
                let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                let bytes = crypto::base64_decode(&cleaned).map_err(|e| {
                    Exception::throw_message(&ctx, &format!("InvalidCharacterError: {e}"))
                })?;
                let out: String = bytes.into_iter().map(char::from).collect();
                Ok(out)
            },
        )?,
    )?;

    // 6. Fast native UTF-8 encode / decode for `TextEncoder` & `TextDecoder` (handles subarrays!).
    globals.set(
        "__ytjsUtf8Encode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: String| -> rquickjs::Result<TypedArray<'js, u8>> {
                TypedArray::<u8>::new(ctx, input.into_bytes())
            },
        )?,
    )?;
    globals.set(
        "__ytjsUtf8Decode",
        Function::new(ctx.clone(), |val: Value<'js>| -> rquickjs::Result<String> {
            let bytes = js_val_to_raw_bytes(val).unwrap_or_default();
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })?,
    )?;

    // 7. Native `url::Url` parser for `URL` polyfill.
    globals.set(
        "__ytjsParseUrl",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: String, base: Opt<String>| -> rquickjs::Result<Object<'js>> {
                let parsed = if let Some(b) = base.0.as_deref() {
                    let base_url = url::Url::parse(b).map_err(|e| {
                        Exception::throw_message(&ctx, &format!("TypeError: Invalid base URL: {e}"))
                    })?;
                    base_url.join(&input).map_err(|e| {
                        Exception::throw_message(&ctx, &format!("TypeError: Invalid URL: {e}"))
                    })?
                } else {
                    url::Url::parse(&input).map_err(|e| {
                        Exception::throw_message(&ctx, &format!("TypeError: Invalid URL: {e}"))
                    })?
                };
                let out = Object::new(ctx)?;
                let protocol = format!("{}:", parsed.scheme());
                let hostname = parsed.host_str().unwrap_or("").to_owned();
                let port = parsed.port().map(|p| p.to_string()).unwrap_or_default();
                let host = if port.is_empty() {
                    hostname.clone()
                } else {
                    format!("{hostname}:{port}")
                };
                let origin = parsed.origin().ascii_serialization();
                let search = parsed
                    .query()
                    .filter(|q| !q.is_empty())
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                let hash = parsed
                    .fragment()
                    .filter(|h| !h.is_empty())
                    .map(|h| format!("#{h}"))
                    .unwrap_or_default();
                out.set("href", parsed.as_str())?;
                out.set("origin", origin)?;
                out.set("protocol", protocol)?;
                out.set("username", parsed.username())?;
                out.set("password", parsed.password().unwrap_or(""))?;
                out.set("host", host)?;
                out.set("hostname", hostname)?;
                out.set("port", port)?;
                out.set("pathname", parsed.path())?;
                out.set("search", search)?;
                out.set("hash", hash)?;
                Ok(out)
            },
        )?,
    )?;

    // 8. Cancellable tokio timers for `setTimeout` / `clearTimeout`.
    let tc_set = Arc::clone(&timer_cancels);
    globals.set(
        "__ytjsTimer",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, timer_id: u64, ms: u64| -> rquickjs::Result<Promise<'js>> {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if let Ok(mut guard) = tc_set.lock() {
                    guard.insert(timer_id, tx);
                }
                let tc_done = Arc::clone(&tc_set);
                Promise::wrap_future(&ctx, async move {
                    let fired = tokio::select! {
                        _ = rx => false,
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => true,
                    };
                    if let Ok(mut guard) = tc_done.lock() {
                        guard.remove(&timer_id);
                    }
                    Ok::<bool, JsError>(fired)
                })
            },
        )?,
    )?;
    let tc_clear = Arc::clone(&timer_cancels);
    globals.set(
        "__ytjsClearTimer",
        Function::new(ctx.clone(), move |timer_id: u64| {
            if let Ok(mut guard) = tc_clear.lock()
                && let Some(tx) = guard.remove(&timer_id)
            {
                let _ = tx.send(());
            }
        })?,
    )?;

    // 9. Persistent disk + in-memory binary `Cache` backend (`<cache_dir>/youtubejs_cache/<key>.bin`).
    let b_get = Arc::clone(&active_bridge);
    let m_get = Arc::clone(&mem_cache);
    globals.set(
        "__ytjsCacheGet",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, persistent: bool| -> rquickjs::Result<Value<'js>> {
                let safe_key = sanitize_cache_key(&key);
                if let Ok(guard) = m_get.lock()
                    && let Some(bytes) = guard.get(&safe_key)
                {
                    let ab = ArrayBuffer::new(ctx.clone(), bytes.clone())?;
                    return Ok(ab.into_value());
                }
                if persistent
                    && let Ok(guard) = b_get.lock()
                    && let Some(ref active) = *guard
                    && let Some(ref dir) = active.cache_dir
                {
                    let path = dir.join(YTJS_CACHE_SUBDIR).join(format!("{safe_key}.bin"));
                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(mut mg) = m_get.lock() {
                            mg.insert(safe_key, bytes.clone());
                        }
                        let ab = ArrayBuffer::new(ctx.clone(), bytes)?;
                        return Ok(ab.into_value());
                    }
                }
                Ok(Value::new_undefined(ctx))
            },
        )?,
    )?;

    let b_set = Arc::clone(&active_bridge);
    let m_set = Arc::clone(&mem_cache);
    globals.set(
        "__ytjsCacheSet",
        Function::new(
            ctx.clone(),
            move |key: String, data: Value<'js>, persistent: bool| {
                let Some(bytes) = js_val_to_raw_bytes(data) else {
                    return;
                };
                let safe_key = sanitize_cache_key(&key);
                if let Ok(mut guard) = m_set.lock() {
                    guard.insert(safe_key.clone(), bytes.clone());
                }
                if persistent
                    && let Ok(guard) = b_set.lock()
                    && let Some(ref active) = *guard
                    && let Some(ref dir) = active.cache_dir
                {
                    let cache_root = dir.join(YTJS_CACHE_SUBDIR);
                    let _ = std::fs::create_dir_all(&cache_root);
                    let path = cache_root.join(format!("{safe_key}.bin"));
                    if let Err(e) = atomic_write(&path, &bytes) {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "youtubei.js disk cache write failed for `{safe_key}`: {e}"
                        );
                    }
                }
            },
        )?,
    )?;

    let b_rm = Arc::clone(&active_bridge);
    let m_rm = Arc::clone(&mem_cache);
    globals.set(
        "__ytjsCacheRemove",
        Function::new(ctx.clone(), move |key: String, persistent: bool| {
            let safe_key = sanitize_cache_key(&key);
            if let Ok(mut guard) = m_rm.lock() {
                guard.remove(&safe_key);
            }
            if persistent
                && let Ok(guard) = b_rm.lock()
                && let Some(ref active) = *guard
                && let Some(ref dir) = active.cache_dir
            {
                let path = dir.join(YTJS_CACHE_SUBDIR).join(format!("{safe_key}.bin"));
                let _ = std::fs::remove_file(path);
            }
        })?,
    )?;

    // 10. Native Rust `reqwest::Client` fetch bridge for `Platform.shim.fetch`.
    let b_fetch = Arc::clone(&active_bridge);
    globals.set(
        "__ytjsNativeFetch",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  raw_url: String,
                  method: String,
                  headers_arr: Value<'js>,
                  body_val: Value<'js>,
                  _redirect: String|
                  -> rquickjs::Result<Promise<'js>> {
                let mut req_headers: Vec<(String, String)> = Vec::new();
                if let Some(arr) = headers_arr.as_array() {
                    for i in 0..arr.len() {
                        if let Ok(pair) = arr.get::<Array<'js>>(i)
                            && let (Ok(k), Ok(v)) = (pair.get::<String>(0), pair.get::<String>(1))
                            && !k.is_empty()
                        {
                            req_headers.push((k, v));
                        }
                    }
                }
                let body_bytes = js_val_to_raw_bytes(body_val);
                let bridge = Arc::clone(&b_fetch);
                let ctx_inner = ctx.clone();

                Promise::wrap_future(&ctx, async move {
                    let (http_client, loopback_origin, deadline) = {
                        let guard = bridge.lock().map_err(|_| {
                            Exception::throw_message(&ctx_inner, "YouTube.js bridge lock poisoned")
                        })?;
                        let Some(ref active) = *guard else {
                            return Err(Exception::throw_message(
                                &ctx_inner,
                                "YouTube.js fetch called outside active request",
                            ));
                        };
                        (
                            active.http_client.clone(),
                            active.loopback_origin.clone(),
                            active.deadline,
                        )
                    };

                    // Capture exact User-Agent header sent by YouTube.js on `/youtubei/v1/player`.
                    if raw_url.contains("/youtubei/v1/player")
                        && let Some((_, ua)) = req_headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                        && !ua.is_empty()
                        && let Ok(mut guard) = bridge.lock()
                        && let Some(ref mut active) = *guard
                    {
                        active.last_player_ua = Some(ua.clone());
                    }

                    let target_url = rewrite_loopback_url(&raw_url, loopback_origin.as_deref());
                    let parsed_method = reqwest::Method::from_bytes(method.trim().as_bytes())
                        .unwrap_or(reqwest::Method::GET);

                    let is_player_script =
                        target_url.contains("/s/player/") || target_url.contains("/iframe_api");
                    let cap = if is_player_script {
                        YTJS_PLAYER_FETCH_CAP
                    } else {
                        YTJS_API_FETCH_CAP
                    };

                    let build_request = |timeout: Duration| {
                        let mut builder = http_client
                            .request(parsed_method.clone(), &target_url)
                            .timeout(timeout);
                        for (k, v) in &req_headers {
                            // Let reqwest manage Accept-Encoding/Host/Content-Length cleanly.
                            if k.eq_ignore_ascii_case("accept-encoding")
                                || k.eq_ignore_ascii_case("host")
                                || k.eq_ignore_ascii_case("content-length")
                            {
                                continue;
                            }
                            builder = builder.header(k.as_str(), v.as_str());
                        }
                        if let Some(ref bytes) = body_bytes {
                            builder = builder.body(bytes.clone());
                        }
                        builder
                    };

                    let rem = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|d| !d.is_zero())
                        .ok_or_else(|| {
                            Exception::throw_message(&ctx_inner, "YouTube.js fetch budget exceeded")
                        })?;
                    let attempt_timeout = rem.min(cap);

                    let mut send_res =
                        tokio::time::timeout(attempt_timeout, build_request(attempt_timeout).send())
                            .await;
                    if let Ok(Err(ref e)) = send_res
                        && !e.is_timeout()
                        && (e.is_connect() || e.is_request() || e.is_body())
                        && deadline
                            .checked_duration_since(Instant::now())
                            .is_some_and(|r| r >= Duration::from_secs(2))
                    {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "youtubei.js transient transport error on {} ({e}); retrying once after 150ms",
                            parsed_method
                        );
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        if let Some(retry_rem) = deadline
                            .checked_duration_since(Instant::now())
                            .filter(|d| !d.is_zero())
                        {
                            let retry_timeout = retry_rem.min(cap);
                            send_res = tokio::time::timeout(
                                retry_timeout,
                                build_request(retry_timeout).send(),
                            )
                            .await;
                        }
                    }

                    let resp = match send_res {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            return Err(Exception::throw_message(
                                &ctx_inner,
                                &format!("fetch error: {e}"),
                            ));
                        }
                        Err(_) => {
                            return Err(Exception::throw_message(
                                &ctx_inner,
                                "fetch request timed out",
                            ));
                        }
                    };

                    let status = resp.status();
                    let final_url = resp.url().to_string();
                    let redirected = final_url != target_url;
                    let mut resp_headers: Vec<(String, String)> = Vec::new();
                    for (k, v) in resp.headers() {
                        if let Ok(val) = v.to_str() {
                            resp_headers.push((k.as_str().to_owned(), val.to_owned()));
                        }
                    }

                    let body_rem = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|d| !d.is_zero())
                        .ok_or_else(|| {
                            Exception::throw_message(
                                &ctx_inner,
                                "YouTube.js fetch body budget exceeded",
                            )
                        })?;
                    let body = match tokio::time::timeout(body_rem.min(cap), resp.bytes()).await {
                        Ok(Ok(b)) => b.to_vec(),
                        Ok(Err(e)) => {
                            return Err(Exception::throw_message(
                                &ctx_inner,
                                &format!("fetch body read error: {e}"),
                            ));
                        }
                        Err(_) => {
                            return Err(Exception::throw_message(
                                &ctx_inner,
                                "fetch body read timed out",
                            ));
                        }
                    };

                    let out = Object::new(ctx_inner.clone())?;
                    out.set("status", i32::from(status.as_u16()))?;
                    out.set("statusText", status.canonical_reason().unwrap_or(""))?;
                    out.set("url", final_url)?;
                    out.set("redirected", redirected)?;
                    let hdr_arr = Array::new(ctx_inner.clone())?;
                    for (idx, (k, v)) in resp_headers.into_iter().enumerate() {
                        let pair = Array::new(ctx_inner.clone())?;
                        pair.set(0, k)?;
                        pair.set(1, v)?;
                        hdr_arr.set(idx, pair)?;
                    }
                    out.set("headers", hdr_arr)?;
                    let body_u8 = TypedArray::<u8>::new(ctx_inner.clone(), body)?;
                    out.set("body", body_u8)?;
                    Ok::<Object<'js>, JsError>(out)
                })
            },
        )?,
    )?;

    Ok(())
}

const YOUTUBEJS_POLYFILLS_JS: &str = r#"
(function () {
  if (typeof globalThis.window === "undefined") globalThis.window = globalThis;
  if (typeof globalThis.self === "undefined") globalThis.self = globalThis;

  // console
  globalThis.console = {
    debug: (...a) => __ytjsLog("debug", a.map(String).join(" ")),
    log: (...a) => __ytjsLog("info", a.map(String).join(" ")),
    info: (...a) => __ytjsLog("info", a.map(String).join(" ")),
    warn: (...a) => __ytjsLog("warn", a.map(String).join(" ")),
    error: (...a) => __ytjsLog("error", a.map(String).join(" ")),
  };

  // performance
  globalThis.performance = {
    now: () => __ytjsPerfNow(),
    timeOrigin: Date.now(),
  };

  // Intl (required by YouTube.js Session.getSessionData default timezone)
  if (typeof globalThis.Intl === "undefined") {
    globalThis.Intl = {
      DateTimeFormat: function () {
        return {
          resolvedOptions: function () {
            return { timeZone: "UTC", locale: "en-US" };
          },
          format: function (d) {
            return new Date(d || Date.now()).toISOString();
          },
        };
      },
    };
  }

  // crypto
  globalThis.crypto = {
    randomUUID: () => __ytjsUuidV4(),
    getRandomValues: (arr) => {
      if (!arr || !ArrayBuffer.isView(arr)) {
        throw new TypeError("crypto.getRandomValues expects a TypedArray");
      }
      const bytes = __ytjsRandomBytes(arr.byteLength);
      new Uint8Array(arr.buffer, arr.byteOffset, arr.byteLength).set(bytes);
      return arr;
    },
  };

  // base64
  globalThis.btoa = (s) => __ytjsBtoa(String(s));
  globalThis.atob = (s) => __ytjsAtob(String(s));

  // queueMicrotask & timers
  if (typeof globalThis.queueMicrotask !== "function") {
    globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
  }
  let _nextTimer = 1;
  const _intervals = new Set();
  globalThis.setTimeout = (fn, ms, ...args) => {
    const id = _nextTimer++;
    const delay = Math.max(0, Number(ms) || 0);
    __ytjsTimer(id, delay).then((fired) => {
      if (fired && typeof fn === "function") fn(...args);
    });
    return id;
  };
  globalThis.clearTimeout = (id) => {
    if (typeof id === "number") __ytjsClearTimer(id);
  };
  globalThis.setInterval = (fn, ms, ...args) => {
    const id = _nextTimer++;
    _intervals.add(id);
    const delay = Math.max(1, Number(ms) || 0);
    const tick = () => {
      if (!_intervals.has(id)) return;
      __ytjsTimer(id, delay).then((fired) => {
        if (fired && _intervals.has(id) && typeof fn === "function") {
          fn(...args);
          tick();
        }
      });
    };
    tick();
    return id;
  };
  globalThis.clearInterval = (id) => {
    _intervals.delete(id);
    if (typeof id === "number") __ytjsClearTimer(id);
  };

  // TextEncoder / TextDecoder (handles TypedArray/DataView subarrays cleanly)
  function toContiguousU8(input) {
    if (!input) return new Uint8Array(0);
    if (input instanceof ArrayBuffer) return new Uint8Array(input);
    if (ArrayBuffer.isView(input)) {
      if (input.byteOffset === 0 && input.byteLength === input.buffer.byteLength && input instanceof Uint8Array) {
        return input;
      }
      return new Uint8Array(input.buffer.slice(input.byteOffset, input.byteOffset + input.byteLength));
    }
    return new Uint8Array(0);
  }

  class TextEncoder {
    constructor() {
      this.encoding = "utf-8";
    }
    encode(str = "") {
      return __ytjsUtf8Encode(String(str));
    }
    encodeInto(src, dest) {
      const encoded = this.encode(src);
      const written = Math.min(encoded.length, dest.length);
      dest.set(encoded.subarray(0, written));
      return { read: String(src).length, written };
    }
  }
  class TextDecoder {
    constructor(label = "utf-8", options = {}) {
      this.encoding = String(label || "utf-8").toLowerCase();
      this.fatal = !!options.fatal;
      this.ignoreBOM = !!options.ignoreBOM;
    }
    decode(input) {
      if (!input) return "";
      return __ytjsUtf8Decode(toContiguousU8(input));
    }
  }
  globalThis.TextEncoder = TextEncoder;
  globalThis.TextDecoder = TextDecoder;

  // URLSearchParams / URL
  function safeDecodeFormComponent(s) {
    const plus = s.replace(/\+/g, " ");
    try {
      return decodeURIComponent(plus);
    } catch {
      return plus;
    }
  }
  class URLSearchParams {
    constructor(init) {
      this._entries = [];
      if (init instanceof URLSearchParams) {
        for (const [k, v] of init._entries) this._entries.push([k, v]);
      } else if (typeof init === "string") {
        const raw = init.startsWith("?") ? init.slice(1) : init;
        if (raw) {
          for (const part of raw.split("&")) {
            if (!part) continue;
            const idx = part.indexOf("=");
            const k = safeDecodeFormComponent(idx >= 0 ? part.slice(0, idx) : part);
            const v = safeDecodeFormComponent(idx >= 0 ? part.slice(idx + 1) : "");
            this._entries.push([k, v]);
          }
        }
      } else if (Array.isArray(init)) {
        for (const pair of init) {
          if (Array.isArray(pair) && pair.length >= 2) {
            this._entries.push([String(pair[0]), String(pair[1])]);
          }
        }
      } else if (init && typeof init === "object") {
        for (const [k, v] of Object.entries(init)) {
          this._entries.push([String(k), String(v)]);
        }
      }
    }
    append(k, v) { this._entries.push([String(k), String(v)]); }
    set(k, v) {
      k = String(k);
      v = String(v);
      let replaced = false;
      const next = [];
      for (const pair of this._entries) {
        if (pair[0] === k) {
          if (!replaced) {
            next.push([k, v]);
            replaced = true;
          }
        } else {
          next.push(pair);
        }
      }
      if (!replaced) next.push([k, v]);
      this._entries = next;
    }
    get(k) {
      const hit = this._entries.find(([ek]) => ek === String(k));
      return hit ? hit[1] : null;
    }
    getAll(k) {
      return this._entries.filter(([ek]) => ek === String(k)).map(([, v]) => v);
    }
    has(k) { return this._entries.some(([ek]) => ek === String(k)); }
    delete(k) { this._entries = this._entries.filter(([ek]) => ek !== String(k)); }
    sort() { this._entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)); }
    keys() { return this._entries.map(([k]) => k)[Symbol.iterator](); }
    values() { return this._entries.map(([, v]) => v)[Symbol.iterator](); }
    entries() { return this._entries.map(([k, v]) => [k, v])[Symbol.iterator](); }
    forEach(cb, thisArg) { for (const [k, v] of this._entries) cb.call(thisArg, v, k, this); }
    get size() { return this._entries.length; }
    [Symbol.iterator]() { return this.entries(); }
    toString() {
      return this._entries
        .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
        .join("&");
    }
  }
  globalThis.URLSearchParams = URLSearchParams;

  class URL {
    constructor(input, base) {
      const parts = base !== undefined
        ? __ytjsParseUrl(String(input), String(base))
        : __ytjsParseUrl(String(input));
      this.protocol = parts.protocol;
      this.username = parts.username;
      this.password = parts.password;
      this.host = parts.host;
      this.hostname = parts.hostname;
      this.port = parts.port;
      this.origin = parts.origin;
      this.pathname = parts.pathname;
      this.hash = parts.hash;
      this.searchParams = new URLSearchParams(parts.search);
    }
    get search() {
      const q = this.searchParams.toString();
      return q ? "?" + q : "";
    }
    set search(val) {
      const s = String(val ?? "");
      this.searchParams = new URLSearchParams(s.startsWith("?") ? s : (s ? "?" + s : ""));
    }
    get href() {
      const auth = this.username
        ? (this.password ? `${this.username}:${this.password}@` : `${this.username}@`)
        : "";
      return `${this.protocol}//${auth}${this.host}${this.pathname}${this.search}${this.hash}`;
    }
    set href(val) {
      const parts = __ytjsParseUrl(String(val));
      this.protocol = parts.protocol;
      this.username = parts.username;
      this.password = parts.password;
      this.host = parts.host;
      this.hostname = parts.hostname;
      this.port = parts.port;
      this.origin = parts.origin;
      this.pathname = parts.pathname;
      this.hash = parts.hash;
      this.searchParams = new URLSearchParams(parts.search);
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
    static canParse(input, base) {
      try { new URL(input, base); return true; } catch { return false; }
    }
  }
  globalThis.URL = URL;

  // Event / CustomEvent / EventTarget / AbortSignal / AbortController
  class Event {
    constructor(type, init = {}) {
      this.type = String(type);
      this.bubbles = !!init.bubbles;
      this.cancelable = !!init.cancelable;
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this.timeStamp = performance.now();
    }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
    stopPropagation() {}
    stopImmediatePropagation() {}
  }
  class CustomEvent extends Event {
    constructor(type, init = {}) {
      super(type, init);
      this.detail = init && "detail" in init ? init.detail : null;
    }
  }
  class EventTarget {
    constructor() { this._etListeners = new Map(); }
    addEventListener(type, fn, opts) {
      if (typeof fn !== "function") return;
      const list = this._etListeners.get(type) || [];
      list.push({ fn, once: !!(opts && opts.once) });
      this._etListeners.set(type, list);
    }
    removeEventListener(type, fn) {
      const list = this._etListeners.get(type);
      if (!list) return;
      this._etListeners.set(type, list.filter((e) => e.fn !== fn));
    }
    dispatchEvent(event) {
      event.target = this;
      event.currentTarget = this;
      const list = (this._etListeners.get(event.type) || []).slice();
      for (const entry of list) {
        if (entry.once) this.removeEventListener(event.type, entry.fn);
        entry.fn.call(this, event);
      }
      return !event.defaultPrevented;
    }
  }
  class AbortSignal extends EventTarget {
    constructor() {
      super();
      this.aborted = false;
      this.reason = undefined;
      this.onabort = null;
    }
    throwIfAborted() {
      if (this.aborted) {
        const e = this.reason instanceof Error ? this.reason : new Error(String(this.reason ?? "The operation was aborted"));
        e.name = "AbortError";
        throw e;
      }
    }
    _abort(reason) {
      if (this.aborted) return;
      this.aborted = true;
      if (reason !== undefined) {
        this.reason = reason;
      } else {
        const e = new Error("The operation was aborted");
        e.name = "AbortError";
        this.reason = e;
      }
      const ev = new Event("abort");
      if (typeof this.onabort === "function") {
        try { this.onabort(ev); } catch {}
      }
      this.dispatchEvent(ev);
    }
    static abort(reason) {
      const s = new AbortSignal();
      s._abort(reason);
      return s;
    }
    static timeout(ms) {
      const s = new AbortSignal();
      setTimeout(() => {
        const e = new Error("The operation timed out");
        e.name = "TimeoutError";
        s._abort(e);
      }, ms);
      return s;
    }
  }
  class AbortController {
    constructor() { this.signal = new AbortSignal(); }
    abort(reason) { this.signal._abort(reason); }
  }
  globalThis.Event = Event;
  globalThis.CustomEvent = CustomEvent;
  globalThis.EventTarget = EventTarget;
  globalThis.AbortSignal = AbortSignal;
  globalThis.AbortController = AbortController;

  // Headers
  class Headers {
    constructor(init) {
      this._map = new Map();
      this._setCookies = [];
      if (init instanceof Headers) {
        for (const [k, v] of init._map.entries()) this._map.set(k, v);
        this._setCookies = init._setCookies.slice();
      } else if (Array.isArray(init)) {
        for (const pair of init) {
          if (Array.isArray(pair) && pair.length >= 2) this.append(pair[0], pair[1]);
        }
      } else if (init && typeof init === "object") {
        if (typeof init[Symbol.iterator] === "function") {
          for (const pair of init) {
            if (Array.isArray(pair) && pair.length >= 2) this.append(pair[0], pair[1]);
          }
        } else {
          for (const [k, v] of Object.entries(init)) {
            if (v !== undefined) this.set(k, v);
          }
        }
      }
    }
    append(name, value) {
      const k = String(name).toLowerCase();
      const v = String(value);
      if (k === "set-cookie") {
        this._setCookies.push(v);
      }
      if (this._map.has(k)) {
        this._map.set(k, this._map.get(k) + ", " + v);
      } else {
        this._map.set(k, v);
      }
    }
    set(name, value) {
      const k = String(name).toLowerCase();
      const v = String(value);
      if (k === "set-cookie") this._setCookies = [v];
      this._map.set(k, v);
    }
    get(name) {
      const k = String(name).toLowerCase();
      return this._map.has(k) ? this._map.get(k) : null;
    }
    has(name) { return this._map.has(String(name).toLowerCase()); }
    delete(name) {
      const k = String(name).toLowerCase();
      if (k === "set-cookie") this._setCookies = [];
      this._map.delete(k);
    }
    getSetCookie() { return this._setCookies.slice(); }
    entries() { return this._map.entries(); }
    keys() { return this._map.keys(); }
    values() { return this._map.values(); }
    forEach(cb, thisArg) {
      for (const [k, v] of this._map.entries()) cb.call(thisArg, v, k, this);
    }
    [Symbol.iterator]() { return this.entries(); }
  }
  globalThis.Headers = Headers;

  // ReadableStream / WritableStream / TransformStream
  class ReadableStream {
    constructor(underlyingSource = {}) {
      this._queue = [];
      this._closed = false;
      this._errored = false;
      this._error = null;
      this._waiters = [];
      this._source = underlyingSource;
      this.locked = false;
      const controller = {
        enqueue: (chunk) => {
          if (this._closed || this._errored) return;
          if (this._waiters.length > 0) {
            const w = this._waiters.shift();
            w.resolve({ value: chunk, done: false });
          } else {
            this._queue.push(chunk);
          }
        },
        close: () => {
          if (this._closed) return;
          this._closed = true;
          while (this._waiters.length > 0) {
            const w = this._waiters.shift();
            w.resolve({ value: undefined, done: true });
          }
        },
        error: (err) => {
          if (this._errored) return;
          this._errored = true;
          this._error = err;
          while (this._waiters.length > 0) {
            const w = this._waiters.shift();
            w.reject(err);
          }
        },
      };
      this._controller = controller;
      if (typeof underlyingSource.start === "function") {
        try {
          const r = underlyingSource.start(controller);
          if (r && typeof r.catch === "function") r.catch((e) => controller.error(e));
        } catch (e) {
          controller.error(e);
        }
      }
    }
    getReader() {
      if (this.locked) throw new TypeError("ReadableStream is locked");
      this.locked = true;
      const stream = this;
      return {
        async read() {
          if (stream._queue.length > 0) {
            return { value: stream._queue.shift(), done: false };
          }
          if (stream._errored) throw stream._error;
          if (stream._closed) return { value: undefined, done: true };
          if (typeof stream._source.pull === "function") {
            await stream._source.pull(stream._controller);
            if (stream._queue.length > 0) {
              return { value: stream._queue.shift(), done: false };
            }
            if (stream._errored) throw stream._error;
            if (stream._closed) return { value: undefined, done: true };
          }
          return new Promise((resolve, reject) => {
            stream._waiters.push({ resolve, reject });
          });
        },
        releaseLock() { stream.locked = false; },
        async cancel(reason) {
          stream._closed = true;
          stream.locked = false;
          if (typeof stream._source.cancel === "function") {
            await stream._source.cancel(reason);
          }
        },
      };
    }
    async cancel(reason) {
      this._closed = true;
      if (typeof this._source.cancel === "function") await this._source.cancel(reason);
    }
    [Symbol.asyncIterator]() {
      const reader = this.getReader();
      return {
        async next() {
          const res = await reader.read();
          if (res.done) reader.releaseLock();
          return res;
        },
        async return() {
          reader.releaseLock();
          return { value: undefined, done: true };
        },
      };
    }
  }
  class WritableStream {
    constructor(sink = {}) { this._sink = sink; this.locked = false; }
  }
  class TransformStream {
    constructor() {
      this.readable = new ReadableStream();
      this.writable = new WritableStream();
    }
  }
  globalThis.ReadableStream = ReadableStream;
  globalThis.WritableStream = WritableStream;
  globalThis.TransformStream = TransformStream;

  // Blob / File / FormData
  function concatPartsToU8(parts) {
    const chunks = [];
    let total = 0;
    const enc = new TextEncoder();
    for (const part of parts || []) {
      let u8;
      if (part instanceof Blob) u8 = part._bytes;
      else if (part instanceof ArrayBuffer) u8 = new Uint8Array(part);
      else if (ArrayBuffer.isView(part)) u8 = toContiguousU8(part);
      else u8 = enc.encode(String(part));
      chunks.push(u8);
      total += u8.length;
    }
    const merged = new Uint8Array(total);
    let offset = 0;
    for (const c of chunks) { merged.set(c, offset); offset += c.length; }
    return merged;
  }
  class Blob {
    constructor(parts = [], options = {}) {
      this._bytes = concatPartsToU8(parts);
      this.type = String(options.type || "").toLowerCase();
    }
    get size() { return this._bytes.byteLength; }
    async arrayBuffer() { return this._bytes.slice().buffer; }
    async bytes() { return this._bytes.slice(); }
    async text() { return new TextDecoder().decode(this._bytes); }
    stream() {
      const data = this._bytes.slice();
      return new ReadableStream({
        start(c) { if (data.byteLength > 0) c.enqueue(data); c.close(); },
      });
    }
    slice(start = 0, end = this.size, contentType = "") {
      return new Blob([this._bytes.slice(start, end)], { type: contentType });
    }
  }
  class File extends Blob {
    constructor(parts, name, options = {}) {
      super(parts, options);
      this.name = String(name || "");
      this.lastModified = Number(options.lastModified) || Date.now();
    }
  }
  class FormData {
    constructor() { this._entries = []; }
    append(name, value, filename) {
      const v = value instanceof Blob && filename ? new File([value], filename, { type: value.type }) : value;
      this._entries.push([String(name), v]);
    }
    set(name, value, filename) {
      const k = String(name);
      this._entries = this._entries.filter(([ek]) => ek !== k);
      this.append(k, value, filename);
    }
    get(name) {
      const hit = this._entries.find(([ek]) => ek === String(name));
      return hit ? hit[1] : null;
    }
    getAll(name) { return this._entries.filter(([ek]) => ek === String(name)).map(([, v]) => v); }
    has(name) { return this._entries.some(([ek]) => ek === String(name)); }
    delete(name) { this._entries = this._entries.filter(([ek]) => ek !== String(name)); }
    entries() { return this._entries[Symbol.iterator](); }
    keys() { return this._entries.map(([k]) => k)[Symbol.iterator](); }
    values() { return this._entries.map(([, v]) => v)[Symbol.iterator](); }
    forEach(cb, thisArg) { for (const [k, v] of this._entries) cb.call(thisArg, v, k, this); }
    [Symbol.iterator]() { return this.entries(); }
  }
  globalThis.Blob = Blob;
  globalThis.File = File;
  globalThis.FormData = FormData;

  // Request / Response
  class Request {
    constructor(input, init = {}) {
      if (input instanceof Request) {
        this.url = input.url;
        this.method = (init.method || input.method || "GET").toUpperCase();
        this.headers = new Headers(init.headers !== undefined ? init.headers : input.headers);
        this.body = init.body !== undefined ? init.body : input.body;
        this.redirect = init.redirect || input.redirect || "follow";
        this.signal = init.signal || input.signal || null;
        this.credentials = init.credentials || input.credentials || "same-origin";
      } else {
        this.url = input instanceof URL ? input.href : String(input);
        this.method = (init.method || "GET").toUpperCase();
        this.headers = new Headers(init.headers);
        this.body = init.body !== undefined ? init.body : null;
        this.redirect = init.redirect || "follow";
        this.signal = init.signal || null;
        this.credentials = init.credentials || "same-origin";
      }
    }
    clone() {
      return new Request(this.url, {
        method: this.method,
        headers: new Headers(this.headers),
        body: this.body,
        redirect: this.redirect,
        signal: this.signal,
        credentials: this.credentials,
      });
    }
  }
  class Response {
    constructor(body = null, init = {}) {
      this.status = init.status !== undefined ? Number(init.status) : 200;
      this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
      this.ok = this.status >= 200 && this.status < 300;
      this.headers = new Headers(init.headers);
      this.url = init.url || "";
      this.redirected = !!init.redirected;
      this.bodyUsed = false;
      if (body === null || body === undefined) {
        this._bytes = new Uint8Array(0);
      } else if (body instanceof Uint8Array) {
        this._bytes = body;
      } else if (body instanceof ArrayBuffer) {
        this._bytes = new Uint8Array(body);
      } else if (ArrayBuffer.isView(body)) {
        this._bytes = toContiguousU8(body);
      } else if (body instanceof Blob) {
        this._bytes = body._bytes;
      } else {
        this._bytes = new TextEncoder().encode(String(body));
      }
      const bytes = this._bytes;
      this.body = new ReadableStream({
        start(c) {
          if (bytes.byteLength > 0) c.enqueue(bytes);
          c.close();
        },
      });
    }
    async arrayBuffer() {
      this.bodyUsed = true;
      return this._bytes.slice().buffer;
    }
    async bytes() {
      this.bodyUsed = true;
      return this._bytes.slice();
    }
    async text() {
      this.bodyUsed = true;
      return new TextDecoder().decode(this._bytes);
    }
    async json() {
      const txt = await this.text();
      return JSON.parse(txt);
    }
    async blob() {
      this.bodyUsed = true;
      return new Blob([this._bytes], { type: this.headers.get("content-type") || "" });
    }
    clone() {
      return new Response(this._bytes.slice(), {
        status: this.status,
        statusText: this.statusText,
        headers: new Headers(this.headers),
        url: this.url,
        redirected: this.redirected,
      });
    }
  }
  globalThis.Request = Request;
  globalThis.Response = Response;

  // structuredClone
  if (typeof globalThis.structuredClone !== "function") {
    globalThis.structuredClone = function clone(val, seen = new WeakMap()) {
      if (val === null || typeof val !== "object") return val;
      if (seen.has(val)) return seen.get(val);
      if (val instanceof Date) return new Date(val.getTime());
      if (val instanceof RegExp) return new RegExp(val.source, val.flags);
      if (val instanceof Uint8Array) return new Uint8Array(val);
      if (val instanceof ArrayBuffer) return val.slice(0);
      if (val instanceof Map) {
        const out = new Map();
        seen.set(val, out);
        for (const [k, v] of val.entries()) out.set(clone(k, seen), clone(v, seen));
        return out;
      }
      if (val instanceof Set) {
        const out = new Set();
        seen.set(val, out);
        for (const v of val.values()) out.add(clone(v, seen));
        return out;
      }
      if (Array.isArray(val)) {
        const out = new Array(val.length);
        seen.set(val, out);
        for (let i = 0; i < val.length; i++) out[i] = clone(val[i], seen);
        return out;
      }
      const out = {};
      seen.set(val, out);
      for (const k of Object.keys(val)) out[k] = clone(val[k], seen);
      return out;
    };
  }
})();
"#;

const YOUTUBEJS_DRIVER_JS: &str = r#"
(function () {
  const Yt = globalThis.__nshYouTubeJS;
  if (!Yt || !Yt.Innertube || !Yt.Platform) {
    throw new Error("youtubei.min.js did not expose __nshYouTubeJS.Innertube / Platform");
  }
  const { Innertube, Platform, Constants, Log } = Yt;
  if (Log && Log.setLevel && Log.Level) {
    Log.setLevel(Log.Level.ERROR);
  }

  class NshDiskCache {
    constructor(persistent = true, persistent_directory = "") {
      this._persistent = !!persistent;
      this._dir = persistent_directory || "";
    }
    get cache_dir() {
      return this._persistent ? this._dir : "";
    }
    async get(key) {
      return __ytjsCacheGet(String(key), this._persistent);
    }
    async set(key, value) {
      let buf = value;
      if (buf && ArrayBuffer.isView(buf)) {
        buf = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
      }
      __ytjsCacheSet(String(key), buf, this._persistent);
    }
    async remove(key) {
      __ytjsCacheRemove(String(key), this._persistent);
    }
  }

  function extractCookieNamed(cookieHeader, name) {
    for (const part of String(cookieHeader || "").split(";")) {
      const idx = part.indexOf("=");
      if (idx <= 0) continue;
      const k = part.slice(0, idx).trim();
      if (k === name) {
        const v = part.slice(idx + 1).trim();
        if (v) return v;
      }
    }
    return null;
  }

  async function buildMultiSchemeSapisidAuth(cookieHeader, origin) {
    const cleanOrigin = String(origin || "https://www.youtube.com").replace(/\/+$/, "");
    const ts = Math.floor(Date.now() / 1000);
    const schemes = [];
    const sapisid = extractCookieNamed(cookieHeader, "SAPISID") || extractCookieNamed(cookieHeader, "__Secure-3PAPISID");
    const s1p = extractCookieNamed(cookieHeader, "__Secure-1PAPISID");
    const s3p = extractCookieNamed(cookieHeader, "__Secure-3PAPISID");
    if (sapisid) {
      schemes.push(`SAPISIDHASH ${ts}_${await Platform.shim.sha1Hash(`${ts} ${sapisid} ${cleanOrigin}`)}`);
    }
    if (s1p) {
      schemes.push(`SAPISID1PHASH ${ts}_${await Platform.shim.sha1Hash(`${ts} ${s1p} ${cleanOrigin}`)}`);
    }
    if (s3p) {
      schemes.push(`SAPISID3PHASH ${ts}_${await Platform.shim.sha1Hash(`${ts} ${s3p} ${cleanOrigin}`)}`);
    }
    return schemes.length > 0 ? schemes.join(" ") : null;
  }

  let lastPlayerVisitorData = "";
  let lastPlayerRawPlayability = null;

  function extractTextNode(val) {
    if (!val) return "";
    if (typeof val === "string") return val.trim();
    if (typeof val.simpleText === "string") return val.simpleText.trim();
    if (Array.isArray(val.runs)) {
      return val.runs.map((r) => (r && typeof r.text === "string" ? r.text : "")).join("").trim();
    }
    if (typeof val.toString === "function" && val.toString !== Object.prototype.toString) {
      return String(val.toString()).trim();
    }
    return "";
  }

  async function nshYtjsFetch(input, init) {
    const isReq = input instanceof Request;
    const urlStr = typeof input === "string"
      ? input
      : (input instanceof URL ? input.href : (isReq ? input.url : String(input)));
    const method = ((init && init.method) || (isReq && input.method) || "GET").toUpperCase();
    const headers = new Headers(
      (init && init.headers !== undefined)
        ? init.headers
        : (isReq ? input.headers : undefined)
    );
    let body = (init && init.body !== undefined)
      ? init.body
      : (isReq ? input.body : null);
    const redirect = (init && init.redirect) || (isReq && input.redirect) || "follow";

    // Enrich authenticated InnerTube web requests with multi-scheme SAPISIDHASH + bootstrap flag,
    // and ensure TV client User-Agent header matches its context.client.userAgent.
    if (urlStr.includes("/youtubei/v1/")) {
      const cookieHdr = headers.get("cookie");
      if (cookieHdr) {
        headers.set("x-youtube-bootstrap-logged-in", "true");
        const originHdr = headers.get("origin") || "https://www.youtube.com";
        const auth = await buildMultiSchemeSapisidAuth(cookieHdr, originHdr);
        if (auth) {
          headers.set("authorization", auth);
          if (!headers.has("x-goog-authuser")) headers.set("x-goog-authuser", "0");
        }
      }
      if (typeof body === "string" && body.includes('"clientName":"TVHTML5"')) {
        if (Constants && Constants.CLIENTS && Constants.CLIENTS.TV && Constants.CLIENTS.TV.USER_AGENT) {
          headers.set("user-agent", Constants.CLIENTS.TV.USER_AGENT);
        }
      }
    }

    if (body instanceof URLSearchParams) {
      body = body.toString();
    } else if (body instanceof Blob) {
      body = body._bytes;
    } else if (body && ArrayBuffer.isView(body)) {
      body = new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength));
    }

    const headerPairs = [];
    for (const [k, v] of headers.entries()) {
      headerPairs.push([k, v]);
    }
    const rawResp = await __ytjsNativeFetch(urlStr, method, headerPairs, body, redirect);
    if (urlStr.includes("/youtubei/v1/player") && rawResp.status >= 200 && rawResp.status < 300 && rawResp.body) {
      try {
        const parsedPlayer = JSON.parse(new TextDecoder().decode(rawResp.body));
        if (parsedPlayer && parsedPlayer.responseContext && typeof parsedPlayer.responseContext.visitorData === "string" && parsedPlayer.responseContext.visitorData) {
          lastPlayerVisitorData = parsedPlayer.responseContext.visitorData;
        }
        if (parsedPlayer && parsedPlayer.playabilityStatus) {
          lastPlayerRawPlayability = parsedPlayer.playabilityStatus;
        }
      } catch {}
    }
    return new Response(rawResp.body, {
      status: rawResp.status,
      statusText: rawResp.statusText,
      headers: rawResp.headers,
      url: rawResp.url,
      redirected: rawResp.redirected,
    });
  }

  Platform.load({
    runtime: "node",
    server: true,
    Cache: NshDiskCache,
    sha1Hash: async (data) => __ytjsSha1Hex(String(data)),
    uuidv4: () => __ytjsUuidV4(),
    eval: async (data, env) => {
      const code = typeof data === "string" ? data : (data && data.output ? data.output : "");
      if (!code) return undefined;
      const fn = new Function("env", code);
      return fn(env || {});
    },
    fetch: nshYtjsFetch,
    Request,
    Response,
    Headers,
    FormData,
    File,
    ReadableStream,
    CustomEvent,
  });

  // Warm Innertube session cache (keyed by auth/UA/lang/visitor/loopback, 20 min TTL).
  const SESSION_TTL_MS = 20 * 60 * 1000;
  const sessionPool = new Map();

  async function getOrCreateInnertube(opts) {
    const isLoopback = !!opts.isLoopback;
    const cookieStr = opts.cookie ? String(opts.cookie).trim() : "";
    const cookieKey = cookieStr ? __ytjsSha1Hex(cookieStr) : "anon";
    const uaKey = opts.userAgentOverride || "";
    const langKey = opts.lang || "";
    const vdKey = opts.visitorData || "";
    const cacheKey = `${isLoopback ? "lb" : "live"}|${cookieKey}|${uaKey}|${langKey}|${vdKey}`;

    if (!isLoopback) {
      const hit = sessionPool.get(cacheKey);
      if (hit && (Date.now() - hit.createdAt) < SESSION_TTL_MS) {
        return hit.yt;
      }
    }

    const baseCfg = {
      lang: opts.lang || "en",
      location: "US",
      retrieve_innertube_config: false,
      cache: isLoopback ? undefined : new NshDiskCache(true),
      enable_session_cache: !isLoopback,
      generate_session_locally: isLoopback,
    };
    if (cookieStr) baseCfg.cookie = cookieStr;
    if (opts.userAgentOverride) baseCfg.user_agent = opts.userAgentOverride;
    if (opts.visitorData) baseCfg.visitor_data = opts.visitorData;

    let yt;
    if (isLoopback) {
      yt = await Innertube.create({ ...baseCfg, retrieve_player: false });
    } else {
      try {
        yt = await Innertube.create({ ...baseCfg, retrieve_player: true });
      } catch (_playerErr) {
        // If YouTube.js Player.create fails (e.g., new player AST), continue without player
        // so Rust's yt-dlp/ejs 0.8.0 solver can decipher sig/n as dual fallback.
        yt = await Innertube.create({ ...baseCfg, retrieve_player: false });
      }
    }

    if (!isLoopback) {
      if (sessionPool.size >= 8) {
        const oldestKey = sessionPool.keys().next().value;
        if (oldestKey) sessionPool.delete(oldestKey);
      }
      sessionPool.set(cacheKey, { yt, createdAt: Date.now() });
    }
    return yt;
  }

  function extractReasonFromPlayability(ps, rawPs) {
    let reason = ((ps && ps.reason) || (rawPs && rawPs.reason) || "").trim();
    const sub = extractTextNode(ps?.error_screen?.subreason)
      || extractTextNode(rawPs?.errorScreen?.playerErrorMessageRenderer?.subreason)
      || extractTextNode(ps?.error_screen?.reason)
      || extractTextNode(rawPs?.errorScreen?.playerErrorMessageRenderer?.reason);
    if (sub && !reason.toLowerCase().includes(sub.toLowerCase())) {
      reason = reason ? `${reason} (${sub})` : sub;
    }
    return reason;
  }

  function extractQueryParam(rawUrl, key) {
    try {
      return new URL(rawUrl).searchParams.get(key);
    } catch {
      return null;
    }
  }

  globalThis.__nshYtJsExtract = async function (reqJson) {
    const req = JSON.parse(reqJson);
    lastPlayerVisitorData = "";
    lastPlayerRawPlayability = null;
    try {
      const yt = await getOrCreateInnertube(req);
      const clientOpts = { client: req.ytjsClient };
      if (req.poToken) clientOpts.po_token = req.poToken;

      const info = await yt.getBasicInfo(req.videoId, clientOpts);
      const ps = info.playability_status || {};
      const status = String(ps.status || lastPlayerRawPlayability?.status || "");
      const reason = extractReasonFromPlayability(ps, lastPlayerRawPlayability);
      const visitorData = String(
        lastPlayerVisitorData
          || yt.session?.context?.client?.visitorData
          || req.visitorData
          || ""
      );
      const sessionUa = String(req.userAgentOverride || yt.session?.user_agent || "");

      if (status && status !== "OK") {
        return JSON.stringify({
          ok: false,
          playabilityStatus: status,
          playabilityReason: reason || `video ${status}`,
          visitorData,
          userAgent: sessionUa,
        });
      }

      const basic = info.basic_info || {};
      const thumbs = Array.isArray(basic.thumbnail) ? basic.thumbnail : [];
      const thumbnail = thumbs.length > 0 ? String(thumbs[thumbs.length - 1].url || "") : "";
      const sd = info.streaming_data || {};
      const player = yt.session?.player;

      const rawList = [];
      if (Array.isArray(sd.adaptive_formats)) {
        for (const f of sd.adaptive_formats) rawList.push({ f, fromMuxed: false });
      }
      if (Array.isArray(sd.formats)) {
        for (const f of sd.formats) rawList.push({ f, fromMuxed: true });
      }

      const outFormats = [];
      for (const { f, fromMuxed } of rawList) {
        if (!f) continue;
        const mime = String(f.mime_type || "");
        const mimeLower = mime.toLowerCase();
        const isAudioMime = mimeLower.startsWith("audio/");
        const hasAudio = !!f.has_audio || isAudioMime || fromMuxed || !!f.audio_channels;
        if (!hasAudio) continue;

        const hasRawCipher = !!(f.signature_cipher || f.cipher);
        const rawUrl = f.url ? String(f.url) : null;
        const rawN = rawUrl ? extractQueryParam(rawUrl, "n") : null;

        let finalUrl = rawUrl;
        let decipheredByYtjs = false;
        if (player && (hasRawCipher || rawN)) {
          try {
            const dec = await f.decipher(player);
            if (dec && typeof dec === "string" && dec.startsWith("http") && !dec.includes("enhanced_except_")) {
              const decN = extractQueryParam(dec, "n");
              const nWasTransformed = !rawN || (decN && decN !== rawN);
              if (nWasTransformed) {
                finalUrl = dec;
                decipheredByYtjs = true;
              }
            }
          } catch (_decErr) {
            // Fall through with decipheredByYtjs=false so Rust yt_ejs solves sig/n.
          }
        } else if (!hasRawCipher && !rawN && rawUrl) {
          decipheredByYtjs = true;
        }

        const audioTrack = f.audio_track || null;
        const isDefaultTrack = audioTrack ? (audioTrack.audio_is_default !== false) : true;
        const isOriginalTrack = typeof f.is_original === "boolean"
          ? f.is_original
          : (isDefaultTrack || (audioTrack && String(audioTrack.id || "").includes(".4")));

        outFormats.push({
          itag: Number(f.itag) || 0,
          mimeType: mime,
          url: finalUrl,
          rawUrl,
          signatureCipher: f.signature_cipher ? String(f.signature_cipher) : null,
          cipher: f.cipher ? String(f.cipher) : null,
          decipheredByYtjs,
          bitrate: Number(f.bitrate) || 0,
          averageBitrate: Number.isFinite(Number(f.average_bitrate)) && Number(f.average_bitrate) > 0
            ? Number(f.average_bitrate)
            : null,
          audioSampleRate: Number.isFinite(Number(f.audio_sample_rate)) && Number(f.audio_sample_rate) > 0
            ? Number(f.audio_sample_rate)
            : null,
          audioChannels: Number.isFinite(Number(f.audio_channels)) && Number(f.audio_channels) > 0
            ? Number(f.audio_channels)
            : null,
          contentLength: Number.isFinite(Number(f.content_length)) && Number(f.content_length) > 0
            ? Number(f.content_length)
            : null,
          hasAudio: true,
          hasVideo: !!f.has_video || !isAudioMime,
          isMuxed: !isAudioMime,
          isDrc: !!f.is_drc,
          isDefaultAudioTrack: isDefaultTrack,
          isOriginalAudioTrack: isOriginalTrack,
        });
      }

      const hlsUrl = (!req.skipHls && sd.hls_manifest_url) ? String(sd.hls_manifest_url) : null;
      return JSON.stringify({
        ok: true,
        playabilityStatus: status || "OK",
        playabilityReason: reason,
        visitorData,
        userAgent: sessionUa,
        playerId: player?.player_id ? String(player.player_id) : null,
        signatureTimestamp: Number.isFinite(Number(player?.signature_timestamp))
          ? Number(player.signature_timestamp)
          : null,
        title: String(basic.title || ""),
        durationSecs: Number.isFinite(Number(basic.duration)) && Number(basic.duration) > 0
          ? Math.floor(Number(basic.duration))
          : null,
        uploader: String(basic.author || ""),
        channelId: String(basic.channel_id || ""),
        thumbnail,
        hlsManifestUrl: hlsUrl,
        formats: outFormats,
      });
    } catch (err) {
      const info = err && err.info && typeof err.info === "object" ? err.info : null;
      const psStatus = String((info && info.status) || lastPlayerRawPlayability?.status || "");
      const psReason = extractReasonFromPlayability(info, lastPlayerRawPlayability);
      const msg = (err && err.message) ? String(err.message) : String(err);
      return JSON.stringify({
        ok: false,
        playabilityStatus: psStatus,
        playabilityReason: psReason,
        error: msg,
        visitorData: lastPlayerVisitorData || req.visitorData || "",
        userAgent: req.userAgentOverride || "",
      });
    }
  };
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtubejs_bundle_boots_and_resolves_client_aliases() {
        // Verify persistent QuickJS worker boots `youtubei.min.js` + polyfills + `Platform.load`.
        let _ = global_ytjs_worker().expect("YouTube.js QuickJS worker must boot cleanly");

        let with_cookie = resolve_requested_ytjs_clients(&[], true);
        let keys: Vec<&str> = with_cookie.iter().map(|c| c.key).collect();
        assert_eq!(
            &keys[..5],
            &["web_music", "web_safari", "tv", "android_vr", "visionos"]
        );

        let excluded = resolve_requested_ytjs_clients(
            &["default".to_owned(), "-ios".to_owned(), "-tv".to_owned()],
            false,
        );
        let ex_keys: Vec<&str> = excluded.iter().map(|c| c.key).collect();
        assert!(!ex_keys.contains(&"ios"));
        assert!(!ex_keys.contains(&"tv"));
        assert!(ex_keys.contains(&"web_music"));
        assert!(ex_keys.contains(&"android_vr"));

        assert_eq!(
            lookup_ytjs_client("web_remix").map(|c| c.ytjs_client),
            Some("YTMUSIC")
        );
        assert_eq!(
            lookup_ytjs_client("tv_samsung").map(|c| c.ytjs_client),
            Some("TV")
        );
        assert_eq!(
            lookup_ytjs_client("web_safari").map(|c| c.ytjs_client),
            Some("WEB")
        );
        assert_eq!(
            lookup_ytjs_client("web_safari").and_then(|c| c.user_agent_override),
            Some(SAFARI_WEB_UA)
        );
    }
}
