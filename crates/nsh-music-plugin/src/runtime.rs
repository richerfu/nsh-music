use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nsh_music_audio::virtual_ffmpeg::VirtualFfmpegShim;
use nsh_music_core::model::*;
use nsh_music_core::source::{MusicSource, RequestKind, SignInPromptCallback, SourceCompat};
use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{JsxOptions, TransformOptions, Transformer, TypeScriptOptions};
use rquickjs::function::Args;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Error as JsError, Module, Object,
    Persistent, Value,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;

use crate::compat::{CompatReport, analyze_plugin_dir};
use crate::headers::HeaderRequestKind;
use crate::host::{
    HostContext, HostRuntimeOptions, OpenUrlHandler, WebSignInHandler, install_host_globals,
    js_to_json, json_to_js,
};
use crate::manifest::{ManifestError, PluginLocales, PluginManifest, effective_host_version};
use crate::virtual_ytdlp::{
    extract_youtube_video_id, is_ytdlp_command, resolve_youtube_stream_blocking,
};

/// Default per-call RPC timeout for normal plugin exports (matches `mygo-music`).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Extended timeout for stream resolution exports (`stream`, `getStreamUrl`).
pub const STREAM_CALL_TIMEOUT: Duration = Duration::from_secs(45);
/// Extended timeout for interactive sign-in exports (`signIn`, `getQrSignIn`, `pollQrSignIn`).
pub const SIGN_IN_CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Per-plugin QuickJS heap memory limit (256 MiB, matching `mygo-music`).
const QUICKJS_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Canonical 38 `mygo-music` exported function names supported by the plugin runtime.
pub const MYGO_PLUGIN_EXPORTS: &[&str] = &[
    "getTrack",
    "getStreamUrl",
    "getLyrics",
    "search",
    "searchSuggestions",
    "getHome",
    "getBrowse",
    "getPlaylist",
    "getAlbum",
    "getArtist",
    "getLikedTracks",
    "isLiked",
    "toggleLike",
    "likeEpisode",
    "getRecent",
    "onPlayed",
    "getUserPlaylists",
    "createPlaylist",
    "renamePlaylist",
    "deletePlaylist",
    "addToPlaylist",
    "removeFromPlaylist",
    "movePlaylistTrack",
    "savePlaylist",
    "followArtist",
    "saveAlbum",
    "getSavedAlbums",
    "getRadioStations",
    "getRadioTracks",
    "getPodcastShows",
    "getPodcastEpisodes",
    "subscribeShow",
    "getComments",
    "likeComment",
    "postComment",
    "getAuthStatus",
    "signIn",
    "signOut",
];

/// Additional lifecycle & remote-control exports recognized by the runtime.
pub const EXTRA_PLUGIN_EXPORTS: &[&str] = &[
    "getQrSignIn",
    "pollQrSignIn",
    "getConfigSchema",
    "onConfigChange",
    "remotePlay",
    "remotePause",
    "remoteResume",
    "remoteSeek",
    "remoteVolume",
    "remoteState",
    "remoteDevices",
    "remoteUse",
];

/// QR sign-in session initiated by a plugin's `getQrSignIn()` export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QrSignInSession {
    pub token: String,
    pub qr_url: String,
    pub qr_svg: String,
    pub message: String,
    pub expires_in: u64,
}

/// Polling state returned by a plugin's `pollQrSignIn(token)` export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QrSignInStatus {
    /// `"pending"`, `"scanned"`, `"confirmed"`, `"expired"`, or `"error"`.
    pub status: String,
    pub message: String,
    pub account: Option<Account>,
}

/// Errors produced while compiling, booting, or calling a plugin runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TypeScript transpilation failed for {path}: {message}")]
    Transpile { path: PathBuf, message: String },
    #[error("QuickJS runtime initialization failed: {0}")]
    JsInit(String),
    #[error("plugin worker thread failed: {0}")]
    Worker(String),
}

/// Strips TypeScript annotations using `oxc = 0.153.0` (`Parser` + `SemanticBuilder` + `Transformer` + `Codegen`).
/// Plain JavaScript files are returned cleanly as-is after optional validation.
pub fn transpile_ts_to_js(path: &Path, source: &str) -> Result<String, RuntimeError> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    if !source_type.is_typescript() {
        return Ok(source.to_owned());
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(err) = parsed.diagnostics.into_iter().next() {
        return Err(RuntimeError::Transpile {
            path: path.to_path_buf(),
            message: err.render_with_source_code(source.to_owned()),
        });
    }

    let mut program = parsed.program;
    // Evaluate TS enum members so Transformer compiles `enum` cleanly without panicking.
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();

    let options = TransformOptions {
        jsx: JsxOptions::disable(),
        typescript: TypeScriptOptions {
            only_remove_type_imports: false,
            ..Default::default()
        },
        ..TransformOptions::default()
    };

    let ret =
        Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if let Some(err) = ret.diagnostics.into_iter().next() {
        return Err(RuntimeError::Transpile {
            path: path.to_path_buf(),
            message: err.render_with_source_code(source.to_owned()),
        });
    }

    let out = Codegen::new().build(&program);
    Ok(out.code)
}

/// Module resolver strictly scoped inside `plugin_dir` (blocks path traversal).
struct PluginModuleResolver {
    plugin_root: PathBuf,
}

impl Resolver for PluginModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if !name.starts_with('.') {
            return Err(JsError::new_resolving_message(
                base,
                name,
                "external bare module specifiers are not supported in sandboxed plugins",
            ));
        }
        let base_path = Path::new(base);
        let base_dir = base_path.parent().unwrap_or_else(|| Path::new(""));
        let joined = normalize_rel_path(&base_dir.join(name)).ok_or_else(|| {
            JsError::new_resolving_message(base, name, "import escapes plugin directory")
        })?;

        let candidates = [
            joined.clone(),
            joined.with_extension("ts"),
            joined.with_extension("js"),
            joined.with_extension("mjs"),
            joined.join("index.ts"),
            joined.join("index.js"),
        ];

        for cand in candidates {
            let full = self.plugin_root.join(&cand);
            if full.is_file() {
                return Ok(cand.to_string_lossy().replace('\\', "/"));
            }
        }

        Err(JsError::new_resolving_message(
            base,
            name,
            "module file not found in plugin directory",
        ))
    }
}

/// Module loader reading relative JS/TS files inside `plugin_dir` and stripping TS via `oxc`.
struct PluginModuleLoader {
    plugin_root: PathBuf,
}

impl Loader for PluginModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let rel = normalize_rel_path(Path::new(name))
            .ok_or_else(|| JsError::new_loading_message(name, "invalid module path"))?;
        let full = self.plugin_root.join(&rel);
        let raw = fs::read_to_string(&full)
            .map_err(|e| JsError::new_loading_message(name, format!("failed to read file: {e}")))?;
        let js = transpile_ts_to_js(&full, &raw)
            .map_err(|e| JsError::new_loading_message(name, e.to_string()))?;
        Module::declare(ctx.clone(), name, js)
    }
}

fn normalize_rel_path(path: &Path) -> Option<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(p) => parts.push(p.to_os_string()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let mut out = PathBuf::new();
    for p in parts {
        out.push(p);
    }
    Some(out)
}

fn is_low_priority_method(method: &str) -> bool {
    matches!(
        method,
        "played" | "onPlayed" | "lyrics" | "getLyrics" | "comments" | "getComments" | "isLiked"
    )
}

enum RuntimeCommand {
    Call {
        method: String,
        args: Vec<serde_json::Value>,
        reply: std_mpsc::SyncSender<Result<serde_json::Value, SourceError>>,
        started_tx: std_mpsc::SyncSender<()>,
        started: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        cancel_notify: Arc<tokio::sync::Notify>,
    },
    Ping(std_mpsc::SyncSender<()>),
    Shutdown,
}

struct WorkerHandle {
    hi_tx: tokio_mpsc::UnboundedSender<RuntimeCommand>,
    lo_tx: tokio_mpsc::UnboundedSender<RuntimeCommand>,
    interrupted: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

/// An isolated QuickJS-backed `MusicSource` instance running in its own dedicated actor thread.
pub struct PluginSource {
    plugin_dir: PathBuf,
    entry_rel: String,
    transpiled_source: String,
    manifest: Arc<PluginManifest>,
    locales: Arc<PluginLocales>,
    host_ctx: Arc<HostContext>,
    compat_report: CompatReport,
    exports: Arc<HashSet<String>>,
    worker: Mutex<Option<WorkerHandle>>,
    call_timeout_override: Mutex<Option<Duration>>,
    granted: AtomicBool,
}

impl PluginSource {
    /// Loads, transpiles, and boots a plugin from `plugin_dir` on an isolated QuickJS worker thread.
    pub fn load(
        plugin_dir: &Path,
        data_dir: &Path,
        options: HostRuntimeOptions,
    ) -> Result<Self, RuntimeError> {
        let manifest = Arc::new(PluginManifest::from_dir(plugin_dir)?);
        manifest.check_min_app_version(&effective_host_version())?;
        let entry_path = manifest.resolve_entry_path(plugin_dir)?;
        let entry_source = fs::read_to_string(&entry_path).map_err(|source| RuntimeError::Io {
            path: entry_path.clone(),
            source,
        })?;
        let entry_rel = entry_path
            .strip_prefix(plugin_dir)
            .unwrap_or(&entry_path)
            .to_string_lossy()
            .replace('\\', "/");
        let transpiled = transpile_ts_to_js(&entry_path, &entry_source)?;
        let compat_report = analyze_plugin_dir(plugin_dir)?;
        let locales = Arc::new(PluginLocales::load_from_dir(plugin_dir));

        let host_ctx = Arc::new(HostContext::new(
            plugin_dir.to_path_buf(),
            data_dir.to_path_buf(),
            Arc::clone(&manifest),
            Arc::clone(&locales),
            options,
        ));
        let initial_granted = host_ctx.is_granted();

        let (worker, exports) = spawn_worker_thread(
            plugin_dir.to_path_buf(),
            entry_rel.clone(),
            transpiled.clone(),
            Arc::clone(&host_ctx),
            &manifest.id,
        )?;

        Ok(Self {
            plugin_dir: plugin_dir.to_path_buf(),
            entry_rel,
            transpiled_source: transpiled,
            manifest,
            locales,
            host_ctx,
            compat_report,
            exports: Arc::new(exports),
            worker: Mutex::new(Some(worker)),
            call_timeout_override: Mutex::new(None),
            granted: AtomicBool::new(initial_granted),
        })
    }

    /// Returns the parsed manifest for this plugin.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Returns the static compatibility report for this plugin.
    pub fn compat_report(&self) -> &CompatReport {
        &self.compat_report
    }

    /// Returns the sorted list of callable JS functions exported by this plugin.
    pub fn exported_functions(&self) -> Vec<String> {
        let mut names: Vec<String> = self.exports.iter().cloned().collect();
        names.sort();
        names
    }

    /// Sets the active locale for `host.i18n` and localized `SourceInfo`.
    pub fn set_locale(&self, locale: &str) {
        self.host_ctx.set_locale(locale);
    }

    /// Installs or clears the native WebView sign-in handler for `host.webSignIn`.
    pub fn set_web_sign_in_handler(&self, handler: Option<WebSignInHandler>) {
        self.host_ctx.set_web_sign_in_handler(handler);
    }

    /// Installs or clears the URL opener handler for `host.openURL`.
    pub fn set_open_url_handler(&self, handler: Option<OpenUrlHandler>) {
        self.host_ctx.set_open_url_handler(handler);
    }

    /// Overrides the default per-call timeout (primarily for fast unit tests).
    pub fn set_call_timeout_override(&self, timeout: Option<Duration>) {
        if let Ok(mut guard) = self.call_timeout_override.lock() {
            *guard = timeout;
        }
    }

    /// Updates permission grant state surfaced in `compatibility_info()` and host enforcement.
    pub fn set_granted(&self, granted: bool) {
        self.granted.store(granted, Ordering::SeqCst);
        self.host_ctx.set_granted(granted);
    }

    /// Returns whether mobile virtualization compatibility shims (`ffmpeg` / `yt-dlp`) are enabled.
    pub fn mobile_virtual_compat(&self) -> bool {
        self.host_ctx.mobile_virtual_compat_enabled()
    }

    /// Enables or disables mobile virtualization compatibility shims for this plugin.
    pub fn set_mobile_virtual_compat(&self, enabled: bool) {
        self.host_ctx.set_mobile_virtual_compat(enabled);
    }

    /// Returns the most recent process/runtime invocation error recorded for this plugin, if any.
    pub fn runtime_error(&self) -> Option<String> {
        self.host_ctx.last_runtime_error()
    }

    /// Records a runtime error message to be surfaced on this plugin's UI card.
    pub fn record_runtime_error(&self, err: impl Into<String>) {
        self.host_ctx.record_runtime_error(err);
    }

    /// Clears any recorded runtime error message for this plugin.
    pub fn clear_runtime_error(&self) {
        self.host_ctx.clear_runtime_error();
    }

    /// Stops the active QuickJS worker, terminates any spawned processes, and boots a fresh worker.
    pub fn restart(&self) -> Result<(), RuntimeError> {
        self.host_ctx.kill_all_processes();
        let prev = self.worker.lock().ok().and_then(|mut g| g.take());
        if let Some(w) = prev {
            w.interrupted.store(true, Ordering::SeqCst);
            let _ = w.hi_tx.send(RuntimeCommand::Shutdown);
            // Reap previous worker thread off the caller path so a wedged VM cannot block recovery.
            let _ = thread::Builder::new()
                .name(format!("nsh-plugin-reap-{}", self.manifest.id))
                .spawn(move || {
                    let _ = w.join.join();
                });
        }
        let (fresh, _) = spawn_worker_thread(
            self.plugin_dir.clone(),
            self.entry_rel.clone(),
            self.transpiled_source.clone(),
            Arc::clone(&self.host_ctx),
            &self.manifest.id,
        )?;
        if let Ok(mut guard) = self.worker.lock() {
            *guard = Some(fresh);
        }
        Ok(())
    }

    /// Invokes a specific exported JS function by name with JSON arguments using the default timeout.
    pub fn call_raw(
        &self,
        method: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, SourceError> {
        self.call_raw_with_timeout(method, args, DEFAULT_CALL_TIMEOUT)
    }

    /// Invokes a specific exported JS function with an explicit timeout. Interactive calls are
    /// prioritized ahead of queued background metadata calls (`lyrics`/`played`/`comments`/`isLiked`),
    /// and timeouts on queued or background calls never restart/kill active processes.
    pub fn call_raw_with_timeout(
        &self,
        method: &str,
        args: Vec<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, SourceError> {
        if !self.exports.contains(method) {
            return Err(SourceError::Unsupported);
        }
        let timeout_override = self.call_timeout_override.lock().ok().and_then(|g| *g);
        let effective_timeout = timeout_override.unwrap_or(timeout);
        let is_low_priority = is_low_priority_method(method);

        let (tx, hi_tx) = {
            let mut guard = self
                .worker
                .lock()
                .map_err(|_| SourceError::Message("plugin worker lock poisoned".to_owned()))?;
            if guard.is_none() {
                let (fresh, _) = spawn_worker_thread(
                    self.plugin_dir.clone(),
                    self.entry_rel.clone(),
                    self.transpiled_source.clone(),
                    Arc::clone(&self.host_ctx),
                    &self.manifest.id,
                )
                .map_err(|e| {
                    SourceError::Message(format!("failed to restart plugin runtime: {e}"))
                })?;
                *guard = Some(fresh);
            }
            let handle = guard.as_ref().unwrap();
            let call_tx = if is_low_priority {
                handle.lo_tx.clone()
            } else {
                handle.hi_tx.clone()
            };
            (call_tx, handle.hi_tx.clone())
        };

        let started = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        if tx
            .send(RuntimeCommand::Call {
                method: method.to_owned(),
                args,
                reply: reply_tx,
                started_tx,
                started: Arc::clone(&started),
                cancelled: Arc::clone(&cancelled),
                cancel_notify: Arc::clone(&cancel_notify),
            })
            .is_err()
        {
            let _ = self.restart();
            return Err(SourceError::Message(
                "the plugin stopped; try again".to_owned(),
            ));
        }

        // When no explicit `call_timeout_override` is set, wait separately for queue dispatch
        // so calls queued behind a slow active RPC get their full execution budget once started.
        if timeout_override.is_none() {
            let queue_wait = Duration::from_secs(30).max(effective_timeout);
            match started_rx.recv_timeout(queue_wait) {
                Ok(()) => {}
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    cancelled.store(true, Ordering::SeqCst);
                    cancel_notify.notify_one();
                    log::warn!(
                        target: "nsh_music_plugin",
                        "plugin `{}` timed out waiting in queue for `{method}` after {:?} (low_priority={is_low_priority}); preserving worker and active processes",
                        self.manifest.id,
                        queue_wait
                    );
                    return Err(SourceError::Message(format!(
                        "the plugin didn't answer {method} in time"
                    )));
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    if let Ok(res) = reply_rx.try_recv() {
                        return res;
                    }
                    let _ = self.restart();
                    return Err(SourceError::Message(
                        "plugin runtime stopped while awaiting response".to_owned(),
                    ));
                }
            }
        }

        match reply_rx.recv_timeout(effective_timeout) {
            Ok(res) => res,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                cancelled.store(true, Ordering::SeqCst);
                cancel_notify.notify_one();
                let was_started = started.load(Ordering::SeqCst);
                if !is_low_priority && was_started {
                    let (pong_tx, pong_rx) = std_mpsc::sync_channel(1);
                    let alive = hi_tx.send(RuntimeCommand::Ping(pong_tx)).is_ok()
                        && pong_rx.recv_timeout(Duration::from_millis(1500)).is_ok();
                    if alive {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "plugin `{}` timed out calling `{method}` after {:?}, but liveness ping succeeded; preserving worker and active processes",
                            self.manifest.id,
                            effective_timeout
                        );
                    } else {
                        log::warn!(
                            target: "nsh_music_plugin",
                            "plugin `{}` timed out calling `{method}` after {:?} and failed liveness ping; restarting worker",
                            self.manifest.id,
                            effective_timeout
                        );
                        let _ = self.restart();
                    }
                } else {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "plugin `{}` timed out calling `{method}` after {:?} (low_priority={is_low_priority}, started={was_started}); preserving worker and active processes",
                        self.manifest.id,
                        effective_timeout
                    );
                }
                Err(SourceError::Message(format!(
                    "the plugin didn't answer {method} in time"
                )))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.restart();
                Err(SourceError::Message(
                    "plugin runtime stopped while awaiting response".to_owned(),
                ))
            }
        }
    }

    fn call_any(
        &self,
        candidates: &[&str],
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, SourceError> {
        self.call_any_with_timeout(candidates, args, DEFAULT_CALL_TIMEOUT)
    }

    fn call_any_with_timeout(
        &self,
        candidates: &[&str],
        args: Vec<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, SourceError> {
        for name in candidates {
            if self.exports.contains(*name) {
                return self.call_raw_with_timeout(name, args, timeout);
            }
        }
        Err(SourceError::Unsupported)
    }

    /// Invokes a canonical `mygo-music` export with a single JSON options object `{ ... }`,
    /// falling back to legacy positional export aliases when the canonical export is absent.
    fn call_mygo_or_legacy(
        &self,
        canonical: &str,
        obj_arg: Option<serde_json::Value>,
        legacy: &[&str],
        legacy_args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, SourceError> {
        self.call_mygo_or_legacy_with_timeout(
            canonical,
            obj_arg,
            legacy,
            legacy_args,
            DEFAULT_CALL_TIMEOUT,
        )
    }

    fn call_mygo_or_legacy_with_timeout(
        &self,
        canonical: &str,
        obj_arg: Option<serde_json::Value>,
        legacy: &[&str],
        legacy_args: Vec<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, SourceError> {
        if self.exports.contains(canonical) {
            let args = obj_arg.into_iter().collect();
            return self.call_raw_with_timeout(canonical, args, timeout);
        }
        self.call_any_with_timeout(legacy, legacy_args, timeout)
    }

    fn has_any(&self, candidates: &[&str]) -> bool {
        candidates.iter().any(|name| self.exports.contains(*name))
    }

    /// Calls `track({ id })` / `getTrack(id)` to fetch detailed track metadata by ID.
    pub fn get_track(&self, id: &str) -> Result<Track, SourceError> {
        let val = self.call_mygo_or_legacy(
            "track",
            Some(serde_json::json!({ "id": id })),
            &["getTrack"],
            vec![serde_json::json!(id)],
        )?;
        let mut track: Track = parse_json_result(val)?;
        self.stamp_track(&mut track);
        Ok(track)
    }

    /// Calls `getQrSignIn` to initiate a QR-code login session (automatically generates SVG when `qrUrl` is present).
    pub fn get_qr_sign_in(&self) -> Result<QrSignInSession, SourceError> {
        let val = self.call_any_with_timeout(&["getQrSignIn"], vec![], SIGN_IN_CALL_TIMEOUT)?;
        let mut session: QrSignInSession = parse_json_result(val)?;
        if session.qr_svg.is_empty()
            && !session.qr_url.is_empty()
            && let Ok(code) = qrcode::QrCode::new(session.qr_url.as_bytes())
        {
            session.qr_svg = code
                .render::<qrcode::render::svg::Color<'_>>()
                .min_dimensions(200, 200)
                .build();
        }
        Ok(session)
    }

    /// Calls `pollQrSignIn(token)` to check QR login progress.
    pub fn poll_qr_sign_in(&self, token: &str) -> Result<QrSignInStatus, SourceError> {
        let val = self.call_any_with_timeout(
            &["pollQrSignIn"],
            vec![serde_json::json!(token)],
            SIGN_IN_CALL_TIMEOUT,
        )?;
        parse_json_result(val)
    }

    /// Calls `getConfigSchema()` if exported, falling back to manifest `settings`.
    pub fn get_config_schema(&self) -> Result<Vec<SettingDef>, SourceError> {
        if self.has_any(&["getConfigSchema"]) {
            let val = self.call_any(&["getConfigSchema"], vec![])?;
            return parse_json_result(val);
        }
        Ok(self
            .manifest
            .to_source_info(&self.locales, &self.host_ctx.locale())
            .settings)
    }

    /// Gracefully shuts down the QuickJS worker thread and kills any running spawned processes.
    pub fn shutdown(&self) {
        self.host_ctx.kill_all_processes();
        if let Ok(mut guard) = self.worker.lock()
            && let Some(w) = guard.take()
        {
            w.interrupted.store(true, Ordering::SeqCst);
            let _ = w.hi_tx.send(RuntimeCommand::Shutdown);
            let _ = w.join.join();
        }
    }

    fn stamp_track(&self, track: &mut Track) {
        if track.source.is_empty() {
            track.source = self.manifest.id.clone();
        }
    }

    fn stamp_tracks(&self, tracks: &mut [Track]) {
        for t in tracks {
            self.stamp_track(t);
        }
    }

    fn stamp_browse(&self, browse: &mut Browse) {
        for sec in &mut browse.sections {
            self.stamp_tracks(&mut sec.tracks);
            for pl in &mut sec.playlists {
                self.stamp_tracks(&mut pl.tracks);
            }
        }
    }

    fn try_youtube_stream_rescue(
        &self,
        id: &str,
        quality: Quality,
        orig_err: &SourceError,
    ) -> Option<Result<StreamInfo, String>> {
        if !self.host_ctx.mobile_virtual_compat_enabled() {
            return None;
        }
        if !matches!(
            orig_err,
            SourceError::Unavailable(_) | SourceError::Message(_)
        ) {
            return None;
        }
        let declares_ytdlp = self
            .manifest
            .all_process_commands()
            .iter()
            .any(|c| is_ytdlp_command(c));
        if !declares_ytdlp {
            return None;
        }
        let video_id = extract_youtube_video_id(id)?;
        if self
            .host_ctx
            .was_ytdlp_recently_exhausted(&video_id, Duration::from_secs(10))
        {
            log::info!(
                target: "nsh_music_plugin",
                "plugin `{}` skipping duplicate VirtualYtdlpShim rescue for video={video_id} (recently exhausted)",
                self.manifest.id
            );
            return None;
        }

        log::info!(
            target: "nsh_music_plugin",
            "plugin `{}` stream failed ({orig_err}); attempting VirtualYtdlpShim rescue for video={video_id}",
            self.manifest.id
        );

        let cookie = self.host_ctx.cookie_value();
        let resolved = match resolve_youtube_stream_blocking(
            self.host_ctx.http_client().clone(),
            &video_id,
            cookie.as_deref(),
            self.host_ctx.is_mobile(),
            Some(self.host_ctx.data_dir()),
        ) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };

        let ffmpeg_args = vec![
            "-nostdin".to_owned(),
            "-hide_banner".to_owned(),
            "-nostats".to_owned(),
            "-reconnect".to_owned(),
            "1".to_owned(),
            "-reconnect_streamed".to_owned(),
            "1".to_owned(),
            "-request_size".to_owned(),
            "10485760".to_owned(),
            "-user_agent".to_owned(),
            resolved.user_agent.clone(),
            "-i".to_owned(),
            resolved.url.clone(),
            "-vn".to_owned(),
            "-f".to_owned(),
            resolved.ext.clone(),
            "-listen".to_owned(),
            "1".to_owned(),
            "http://127.0.0.1:0/".to_owned(),
        ];

        let vhandle = match VirtualFfmpegShim::spawn(&ffmpeg_args) {
            Ok(h) => h,
            Err(e) => return Some(Err(format!("VirtualFfmpegShim spawn failed: {e}"))),
        };

        let mut ready = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if vhandle.error_output().contains("Input #0") {
                ready = true;
                break;
            }
            if !vhandle.is_running() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }

        if !ready {
            let err_out = vhandle.error_output();
            vhandle.kill();
            let detail = err_out.trim();
            return Some(Err(if detail.is_empty() {
                "VirtualFfmpegShim rescue relay did not become ready".to_owned()
            } else {
                format!("VirtualFfmpegShim rescue relay failed: {detail}")
            }));
        }

        let Some(addr) = vhandle.listen_addr() else {
            vhandle.kill();
            return Some(Err(
                "VirtualFfmpegShim rescue relay did not expose a listen address".to_owned(),
            ));
        };

        self.host_ctx.register_rescue_relay(vhandle);
        let media_type = if resolved.is_hls {
            "audio/aac".to_owned()
        } else {
            resolved.mime_type
        };
        Some(Ok(StreamInfo {
            url: format!("http://{addr}/"),
            media_type,
            quality: quality.as_str().to_owned(),
            bit_rate: resolved.bitrate as i64,
            ..StreamInfo::default()
        }))
    }
}

impl Drop for PluginSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl MusicSource for PluginSource {
    fn info(&self) -> SourceInfo {
        let mut info = self
            .manifest
            .to_source_info(&self.locales, &self.host_ctx.locale());
        if info.settings.is_empty()
            && self.has_any(&["getConfigSchema"])
            && let Ok(dynamic) = self.get_config_schema()
        {
            info.settings = dynamic;
        }
        info
    }

    fn can(&self, what: &str) -> bool {
        if self.exports.contains(what) {
            return true;
        }
        match what {
            "track" | "getTrack" => self.has_any(&["getTrack", "track"]),
            "stream" | "getStreamUrl" => self.has_any(&["getStreamUrl", "stream"]),
            "lyrics" | "getLyrics" => self.has_any(&["getLyrics", "lyrics"]),
            "search" => self.has_any(&["search"]),
            "suggest" | "searchSuggestions" => self.has_any(&["searchSuggestions", "suggest"]),
            "home" | "getHome" => self.has_any(&["getHome", "home"]),
            "browse" | "getBrowse" => self.has_any(&["getBrowse", "browse"]),
            "playlist" | "getPlaylist" => self.has_any(&["getPlaylist", "playlist"]),
            "playlists" | "getUserPlaylists" => self.has_any(&["getUserPlaylists", "playlists"]),
            "album" | "getAlbum" => self.has_any(&["getAlbum", "album"]),
            "artist" | "getArtist" => self.has_any(&["getArtist", "artist"]),
            "liked" | "getLikedTracks" => self.has_any(&["getLikedTracks", "liked"]),
            "like" | "toggleLike" => self.has_any(&["toggleLike", "like"]),
            "recent" | "getRecent" => self.has_any(&["getRecent", "recent"]),
            "played" | "onPlayed" => self.has_any(&["onPlayed", "played"]),
            "savedAlbums" | "getSavedAlbums" => self.has_any(&["getSavedAlbums", "savedAlbums"]),
            "stations" | "getRadioStations" => self.has_any(&["getRadioStations", "stations"]),
            "stationNext" | "getRadioTracks" => self.has_any(&["getRadioTracks", "stationNext"]),
            "shows" | "getPodcastShows" => self.has_any(&["getPodcastShows", "shows"]),
            "show" | "getPodcastEpisodes" => self.has_any(&["getPodcastEpisodes", "show"]),
            "comments" | "getComments" => self.has_any(&["getComments", "comments"]),
            "account" | "getAuthStatus" => self.has_any(&["getAuthStatus", "account"]),
            "qrSignIn" | "getQrSignIn" => self.has_any(&["getQrSignIn"]),
            _ => false,
        }
    }

    fn compatibility_info(&self) -> SourceCompat {
        self.compat_report
            .to_source_compat(self.granted.load(Ordering::SeqCst))
    }

    fn request_headers_for(&self, url: &str, kind: RequestKind) -> Vec<(String, String)> {
        self.host_ctx
            .resolve_request_headers(url, HeaderRequestKind::from(kind))
    }

    fn setting(&self, key: &str) -> String {
        self.host_ctx.get_config(key).unwrap_or_default()
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<(), SourceError> {
        self.host_ctx.set_config(key, value);
        if self.exports.contains("setSetting") {
            let _ = self.call_raw(
                "setSetting",
                vec![serde_json::json!({ "key": key, "value": value })],
            )?;
        } else if self.has_any(&["onConfigChange"]) {
            let _ = self.call_any(
                &["onConfigChange"],
                vec![
                    serde_json::json!(key),
                    serde_json::json!(value),
                    serde_json::json!(self.host_ctx.all_config()),
                ],
            )?;
        }
        Ok(())
    }

    fn set_sign_in_prompt(&self, cb: Option<SignInPromptCallback>) {
        self.host_ctx.set_sign_in_prompt(cb);
    }

    fn sign_in(&self) -> Result<Account, SourceError> {
        let val = self.call_any_with_timeout(&["signIn"], vec![], SIGN_IN_CALL_TIMEOUT)?;
        if !val.is_null()
            && let Ok(account) = parse_json_result::<Account>(val)
            && !account.name.trim().is_empty()
        {
            return Ok(account);
        }
        self.account()?.ok_or_else(|| {
            SourceError::SignIn("sign-in succeeded but account() returned null".into())
        })
    }

    fn sign_out(&self) -> Result<(), SourceError> {
        let _ = self.call_any(&["signOut"], vec![])?;
        Ok(())
    }

    fn account(&self) -> Result<Option<Account>, SourceError> {
        let val = self.call_mygo_or_legacy("account", None, &["getAuthStatus"], vec![])?;
        if val.is_null() {
            return Ok(None);
        }
        // Support `{ signedIn: false }` or `{ account: { ... } }` as well as direct `Account`.
        if let Some(obj) = val.as_object() {
            if obj.get("signedIn").and_then(|v| v.as_bool()) == Some(false) {
                return Ok(None);
            }
            if let Some(acc) = obj.get("account") {
                if acc.is_null() {
                    return Ok(None);
                }
                return Ok(Some(parse_json_result(acc.clone())?));
            }
        }
        Ok(Some(parse_json_result(val)?))
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        offset: usize,
    ) -> Result<SearchResult, SourceError> {
        let val = self.call_raw(
            "search",
            vec![serde_json::json!({
                "query": query,
                "kind": kind.as_str(),
                "offset": offset,
            })],
        )?;
        let mut res: SearchResult = parse_json_result(val)?;
        self.stamp_tracks(&mut res.tracks);
        for pl in &mut res.playlists {
            self.stamp_tracks(&mut pl.tracks);
        }
        Ok(res)
    }

    fn suggest(&self, query: &str) -> Result<Vec<String>, SourceError> {
        let val = self.call_mygo_or_legacy(
            "suggest",
            Some(serde_json::json!({ "query": query })),
            &["searchSuggestions"],
            vec![serde_json::json!(query)],
        )?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        parse_json_result(val)
    }

    fn home(&self) -> Result<Browse, SourceError> {
        let val = self.call_mygo_or_legacy("home", None, &["getHome"], vec![])?;
        let mut browse: Browse = parse_json_result(val)?;
        self.stamp_browse(&mut browse);
        Ok(browse)
    }

    fn browse(&self, page: &str) -> Result<Browse, SourceError> {
        let val = self.call_mygo_or_legacy(
            "browse",
            Some(serde_json::json!({ "page": page })),
            &["getBrowse"],
            vec![serde_json::json!(page)],
        )?;
        let mut browse: Browse = parse_json_result(val)?;
        self.stamp_browse(&mut browse);
        Ok(browse)
    }

    fn playlists(&self) -> Result<Vec<Playlist>, SourceError> {
        let val = self.call_mygo_or_legacy("playlists", None, &["getUserPlaylists"], vec![])?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        let mut list: Vec<Playlist> = parse_json_result(val)?;
        for pl in &mut list {
            self.stamp_tracks(&mut pl.tracks);
        }
        Ok(list)
    }

    fn playlist(&self, id: &str) -> Result<Playlist, SourceError> {
        let val = self.call_mygo_or_legacy(
            "playlist",
            Some(serde_json::json!({ "id": id })),
            &["getPlaylist"],
            vec![serde_json::json!(id)],
        )?;
        let mut pl: Playlist = parse_json_result(val)?;
        self.stamp_tracks(&mut pl.tracks);
        Ok(pl)
    }

    fn album(&self, id: &str) -> Result<AlbumPage, SourceError> {
        let val = self.call_mygo_or_legacy(
            "album",
            Some(serde_json::json!({ "id": id })),
            &["getAlbum"],
            vec![serde_json::json!(id)],
        )?;
        let mut page: AlbumPage = parse_json_result(val)?;
        self.stamp_tracks(&mut page.tracks);
        Ok(page)
    }

    fn artist(&self, id: &str) -> Result<ArtistPage, SourceError> {
        let val = self.call_mygo_or_legacy(
            "artist",
            Some(serde_json::json!({ "id": id })),
            &["getArtist"],
            vec![serde_json::json!(id)],
        )?;
        let mut page: ArtistPage = parse_json_result(val)?;
        self.stamp_tracks(&mut page.tracks);
        Ok(page)
    }

    fn liked(&self, offset: usize) -> Result<TrackPage, SourceError> {
        let val = self.call_mygo_or_legacy(
            "liked",
            Some(serde_json::json!({ "offset": offset })),
            &["getLikedTracks"],
            vec![serde_json::json!(offset)],
        )?;
        let mut page: TrackPage = if val.is_array() {
            TrackPage {
                tracks: parse_json_result(val)?,
                more: false,
            }
        } else {
            parse_json_result(val)?
        };
        self.stamp_tracks(&mut page.tracks);
        Ok(page)
    }

    fn recent(&self) -> Result<Vec<Track>, SourceError> {
        let val = self.call_mygo_or_legacy("recent", None, &["getRecent"], vec![])?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        let mut tracks: Vec<Track> = parse_json_result(val)?;
        self.stamp_tracks(&mut tracks);
        Ok(tracks)
    }

    fn played(&self, track: &Track) {
        if self.exports.contains("played") {
            let _ = self.call_raw("played", vec![serde_json::json!({ "id": track.id })]);
        } else if self.exports.contains("onPlayed")
            && let Ok(v) = serde_json::to_value(track)
        {
            let _ = self.call_raw("onPlayed", vec![v]);
        }
    }

    fn is_liked(&self, ids: &[String]) -> Result<HashMap<String, bool>, SourceError> {
        let val = self.call_raw("isLiked", vec![serde_json::json!({ "ids": ids })])?;
        if val.is_null() {
            return Ok(HashMap::new());
        }
        parse_json_result(val)
    }

    fn like(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_mygo_or_legacy(
            "like",
            Some(serde_json::json!({ "id": id, "on": on })),
            &["toggleLike"],
            vec![serde_json::json!(id), serde_json::json!(on)],
        )?;
        Ok(())
    }

    fn like_episode(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "likeEpisode",
            vec![serde_json::json!({ "id": id, "on": on })],
        )?;
        Ok(())
    }

    fn create_playlist(&self, name: &str) -> Result<Playlist, SourceError> {
        let val = self.call_raw("createPlaylist", vec![serde_json::json!({ "name": name })])?;
        let mut pl: Playlist = parse_json_result(val)?;
        self.stamp_tracks(&mut pl.tracks);
        Ok(pl)
    }

    fn rename_playlist(&self, id: &str, name: &str) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "renamePlaylist",
            vec![serde_json::json!({ "id": id, "name": name })],
        )?;
        Ok(())
    }

    fn delete_playlist(&self, id: &str) -> Result<(), SourceError> {
        let _ = self.call_raw("deletePlaylist", vec![serde_json::json!({ "id": id })])?;
        Ok(())
    }

    fn add_to_playlist(&self, id: &str, track_ids: &[String]) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "addToPlaylist",
            vec![serde_json::json!({ "id": id, "trackIds": track_ids })],
        )?;
        Ok(())
    }

    fn remove_from_playlist(&self, id: &str, track_ids: &[String]) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "removeFromPlaylist",
            vec![serde_json::json!({ "id": id, "trackIds": track_ids })],
        )?;
        Ok(())
    }

    fn move_playlist_track(&self, id: &str, from: usize, to: usize) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "movePlaylistTrack",
            vec![serde_json::json!({ "id": id, "from": from, "to": to })],
        )?;
        Ok(())
    }

    fn save_playlist(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "savePlaylist",
            vec![serde_json::json!({ "id": id, "on": on })],
        )?;
        Ok(())
    }

    fn follow_artist(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "followArtist",
            vec![serde_json::json!({ "id": id, "on": on })],
        )?;
        Ok(())
    }

    fn save_album(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw("saveAlbum", vec![serde_json::json!({ "id": id, "on": on })])?;
        Ok(())
    }

    fn saved_albums(&self, offset: usize) -> Result<AlbumList, SourceError> {
        let val = self.call_mygo_or_legacy(
            "savedAlbums",
            Some(serde_json::json!({ "offset": offset })),
            &["getSavedAlbums"],
            vec![serde_json::json!(offset)],
        )?;
        if val.is_array() {
            return Ok(AlbumList {
                albums: parse_json_result(val)?,
                more: false,
            });
        }
        parse_json_result(val)
    }

    fn shows(&self) -> Result<Vec<Show>, SourceError> {
        let val = self.call_mygo_or_legacy("shows", None, &["getPodcastShows"], vec![])?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        parse_json_result(val)
    }

    fn show(&self, id: &str, offset: usize) -> Result<ShowPage, SourceError> {
        let val = self.call_mygo_or_legacy(
            "show",
            Some(serde_json::json!({ "id": id, "offset": offset })),
            &["getPodcastEpisodes"],
            vec![serde_json::json!(id), serde_json::json!(offset)],
        )?;
        let mut page: ShowPage = if val.is_array() {
            ShowPage {
                show: Show {
                    id: id.to_owned(),
                    ..Show::default()
                },
                episodes: parse_json_result(val)?,
                more: false,
            }
        } else {
            parse_json_result(val)?
        };
        self.stamp_tracks(&mut page.episodes);
        Ok(page)
    }

    fn subscribe_show(&self, id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "subscribeShow",
            vec![serde_json::json!({ "id": id, "on": on })],
        )?;
        Ok(())
    }

    fn stations(&self) -> Result<Vec<Station>, SourceError> {
        let val = self.call_mygo_or_legacy("stations", None, &["getRadioStations"], vec![])?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        parse_json_result(val)
    }

    fn station_next(&self, id: &str) -> Result<Vec<Track>, SourceError> {
        let val = self.call_mygo_or_legacy(
            "stationNext",
            Some(serde_json::json!({ "id": id })),
            &["getRadioTracks"],
            vec![serde_json::json!(id)],
        )?;
        if val.is_null() {
            return Ok(Vec::new());
        }
        let mut tracks: Vec<Track> = parse_json_result(val)?;
        self.stamp_tracks(&mut tracks);
        Ok(tracks)
    }

    fn comments(&self, track_id: &str, offset: usize) -> Result<CommentPage, SourceError> {
        let val = self.call_mygo_or_legacy(
            "comments",
            Some(serde_json::json!({ "trackId": track_id, "offset": offset })),
            &["getComments"],
            vec![serde_json::json!(track_id), serde_json::json!(offset)],
        )?;
        if val.is_array() {
            let comments: Vec<Comment> = parse_json_result(val)?;
            let total = comments.len();
            return Ok(CommentPage {
                comments,
                total,
                more: false,
            });
        }
        parse_json_result(val)
    }

    fn like_comment(&self, track_id: &str, comment_id: &str, on: bool) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "likeComment",
            vec![serde_json::json!({
                "trackId": track_id,
                "commentId": comment_id,
                "on": on,
            })],
        )?;
        Ok(())
    }

    fn post_comment(&self, track_id: &str, text: &str) -> Result<Comment, SourceError> {
        let val = self.call_raw(
            "postComment",
            vec![serde_json::json!({ "trackId": track_id, "text": text })],
        )?;
        parse_json_result(val)
    }

    fn lyrics(&self, id: &str) -> Result<Lyrics, SourceError> {
        let val = self.call_mygo_or_legacy(
            "lyrics",
            Some(serde_json::json!({ "id": id })),
            &["getLyrics"],
            vec![serde_json::json!(id)],
        )?;
        if val.is_null() {
            return Ok(Lyrics::default());
        }
        if let Some(s) = val.as_str() {
            return Ok(Lyrics {
                lrc: s.to_owned(),
                ..Lyrics::default()
            });
        }
        parse_json_result(val)
    }

    fn stream(&self, id: &str, quality: Quality) -> Result<StreamInfo, SourceError> {
        let mut resolved: Result<StreamInfo, SourceError> = (|| {
            let val = self.call_mygo_or_legacy_with_timeout(
                "stream",
                Some(serde_json::json!({
                    "id": id,
                    "quality": quality.as_str(),
                })),
                &["getStreamUrl"],
                vec![serde_json::json!(id), serde_json::json!(quality.as_str())],
                STREAM_CALL_TIMEOUT,
            )?;
            let mut info: StreamInfo = if let Some(url) = val.as_str() {
                StreamInfo {
                    url: url.to_owned(),
                    quality: quality.as_str().to_owned(),
                    ..StreamInfo::default()
                }
            } else {
                parse_json_result(val)?
            };

            // Merge manifest Audio requestHeaders without overwriting headers returned by the plugin.
            if !info.url.is_empty() {
                for (k, v) in self.request_headers_for(&info.url, RequestKind::Audio) {
                    let exists = info.headers.keys().any(|ek| ek.eq_ignore_ascii_case(&k));
                    if !exists {
                        info.headers.insert(k, v);
                    }
                }
            }
            Ok(info)
        })();

        if let Err(orig_err) = resolved {
            resolved = match self.try_youtube_stream_rescue(id, quality, &orig_err) {
                Some(Ok(info)) => Ok(info),
                Some(Err(rescue_err)) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "plugin `{}` VirtualYtdlpShim rescue failed for id={id}: {rescue_err}",
                        self.manifest.id
                    );
                    Err(select_stream_rescue_error(orig_err, rescue_err))
                }
                None => Err(orig_err),
            };
        }

        match &resolved {
            Ok(info) => {
                self.host_ctx.clear_runtime_error();
                self.host_ctx.clear_ytdlp_exhausted();
                log::info!(
                    target: "nsh_music_plugin",
                    "plugin stream resolved: plugin_id={} track_id={} quality={} media_type={} url={}",
                    self.manifest.id,
                    id,
                    quality.as_str(),
                    info.media_type,
                    sanitize_stream_url_for_log(&info.url),
                );
            }
            Err(err) => {
                if !self.host_ctx.mobile_virtual_compat_enabled()
                    && self.host_ctx.last_runtime_error().is_none()
                {
                    self.host_ctx.record_runtime_error(err.to_string());
                }
                log::warn!(
                    target: "nsh_music_plugin",
                    "plugin stream failed: plugin_id={} track_id={} quality={} err={}",
                    self.manifest.id,
                    id,
                    quality.as_str(),
                    err,
                );
            }
        }
        resolved
    }

    fn remote_play(&self, ids: &[String], start: usize, position: i64) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "remotePlay",
            vec![serde_json::json!({
                "ids": ids,
                "start": start,
                "position": position,
            })],
        )?;
        Ok(())
    }

    fn remote_pause(&self) -> Result<(), SourceError> {
        let _ = self.call_any(&["remotePause"], vec![])?;
        Ok(())
    }

    fn remote_resume(&self) -> Result<(), SourceError> {
        let _ = self.call_any(&["remoteResume"], vec![])?;
        Ok(())
    }

    fn remote_seek(&self, position: i64) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "remoteSeek",
            vec![serde_json::json!({ "position": position })],
        )?;
        Ok(())
    }

    fn remote_volume(&self, percent: i32) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "remoteVolume",
            vec![serde_json::json!({ "percent": percent })],
        )?;
        Ok(())
    }

    fn remote_state(&self) -> Result<Option<RemoteState>, SourceError> {
        let val = self.call_any(&["remoteState"], vec![])?;
        if val.is_null() {
            return Ok(None);
        }
        Ok(Some(parse_json_result(val)?))
    }

    fn remote_devices(&self) -> Result<Vec<RemoteDevice>, SourceError> {
        let val = self.call_any(&["remoteDevices"], vec![])?;
        parse_json_result(val)
    }

    fn remote_use(&self, device_id: &str) -> Result<(), SourceError> {
        let _ = self.call_raw(
            "remoteUse",
            vec![serde_json::json!({ "deviceId": device_id })],
        )?;
        Ok(())
    }
}

fn strip_null_object_fields(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let cleaned = map
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_null_object_fields(v)))
                .collect();
            serde_json::Value::Object(cleaned)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(strip_null_object_fields).collect())
        }
        other => other,
    }
}

fn parse_json_result<T: for<'de> Deserialize<'de>>(
    val: serde_json::Value,
) -> Result<T, SourceError> {
    let sanitized = strip_null_object_fields(val);
    serde_json::from_value(sanitized)
        .map_err(|e| SourceError::Message(format!("invalid plugin return payload: {e}")))
}

fn spawn_worker_thread(
    plugin_root: PathBuf,
    entry_rel: String,
    transpiled_source: String,
    host_ctx: Arc<HostContext>,
    plugin_id: &str,
) -> Result<(WorkerHandle, HashSet<String>), RuntimeError> {
    let (hi_tx, hi_rx) = tokio_mpsc::unbounded_channel::<RuntimeCommand>();
    let (lo_tx, lo_rx) = tokio_mpsc::unbounded_channel::<RuntimeCommand>();
    let (boot_tx, boot_rx) = std_mpsc::sync_channel::<Result<HashSet<String>, RuntimeError>>(1);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_for_worker = Arc::clone(&interrupted);
    let thread_name = format!("nsh-plugin-{plugin_id}");

    let join = thread::Builder::new()
        .name(thread_name)
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            run_plugin_actor(
                plugin_root,
                entry_rel,
                transpiled_source,
                host_ctx,
                hi_rx,
                lo_rx,
                boot_tx,
                interrupted_for_worker,
            );
        })
        .map_err(|e| RuntimeError::Worker(format!("failed to spawn worker thread: {e}")))?;

    let exports = boot_rx.recv().map_err(|_| {
        RuntimeError::Worker("plugin worker exited during initialization".to_owned())
    })??;

    Ok((
        WorkerHandle {
            hi_tx,
            lo_tx,
            interrupted,
            join,
        },
        exports,
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_plugin_actor(
    plugin_root: PathBuf,
    entry_rel: String,
    transpiled_source: String,
    host_ctx: Arc<HostContext>,
    mut hi_rx: tokio_mpsc::UnboundedReceiver<RuntimeCommand>,
    mut lo_rx: tokio_mpsc::UnboundedReceiver<RuntimeCommand>,
    boot_tx: std_mpsc::SyncSender<Result<HashSet<String>, RuntimeError>>,
    interrupted: Arc<AtomicBool>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = boot_tx.send(Err(RuntimeError::Worker(e.to_string())));
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let qjs_rt = match AsyncRuntime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = boot_tx.send(Err(RuntimeError::JsInit(e.to_string())));
                return;
            }
        };
        qjs_rt.set_memory_limit(QUICKJS_MEMORY_LIMIT).await;
        qjs_rt.set_max_stack_size(2 * 1024 * 1024).await;
        let stop_flag = Arc::clone(&interrupted);
        qjs_rt
            .set_interrupt_handler(Some(Box::new(move || stop_flag.load(Ordering::Relaxed))))
            .await;
        qjs_rt
            .set_loader(
                PluginModuleResolver {
                    plugin_root: plugin_root.clone(),
                },
                PluginModuleLoader { plugin_root },
            )
            .await;

        let qjs_ctx = match AsyncContext::full(&qjs_rt).await {
            Ok(c) => c,
            Err(e) => {
                let _ = boot_tx.send(Err(RuntimeError::JsInit(e.to_string())));
                return;
            }
        };

        // Install host globals and evaluate the ESM entry module.
        let boot_result: Result<(Persistent<Object<'static>>, HashSet<String>), RuntimeError> =
            qjs_ctx
                .async_with(async |ctx| {
                    install_host_globals(&ctx, Arc::clone(&host_ctx))
                        .catch(&ctx)
                        .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;

                    let module =
                        Module::declare(ctx.clone(), entry_rel.as_str(), transpiled_source)
                            .catch(&ctx)
                            .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;
                    let (evaluated, eval_promise) = module
                        .eval()
                        .catch(&ctx)
                        .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;

                    eval_promise
                        .into_future::<()>()
                        .await
                        .catch(&ctx)
                        .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;

                    let ns = evaluated
                        .namespace()
                        .catch(&ctx)
                        .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;

                    // Collect callable functions from both named ESM exports and `export default { ... }`.
                    let dispatch = Object::new(ctx.clone())
                        .catch(&ctx)
                        .map_err(|e| RuntimeError::JsInit(format_caught_error(e)))?;
                    let mut export_names = HashSet::new();

                    if let Ok(default_val) = ns.get::<_, Value<'_>>("default")
                        && let Some(default_obj) = default_val.into_object()
                    {
                        for prop in default_obj.props::<String, Value<'_>>() {
                            if let Ok((k, v)) = prop
                                && v.is_function()
                            {
                                let _ = dispatch.set(k.as_str(), v);
                                export_names.insert(k);
                            }
                        }
                    }

                    for prop in ns.props::<String, Value<'_>>() {
                        if let Ok((k, v)) = prop
                            && k != "default"
                            && v.is_function()
                        {
                            let _ = dispatch.set(k.as_str(), v);
                            export_names.insert(k);
                        }
                    }

                    let persistent_dispatch = Persistent::save(&ctx, dispatch);
                    Ok((persistent_dispatch, export_names))
                })
                .await;

        let (dispatch_obj, export_names) = match boot_result {
            Ok(pair) => pair,
            Err(err) => {
                let _ = boot_tx.send(Err(err));
                return;
            }
        };

        let _ = boot_tx.send(Ok(export_names));

        // Process RPC invocations sequentially on the isolated QuickJS VM, prioritizing
        // interactive/stream calls ahead of queued background metadata calls.
        loop {
            let cmd = tokio::select! {
                biased;
                maybe_hi = hi_rx.recv() => match maybe_hi {
                    Some(c) => c,
                    None => break,
                },
                maybe_lo = lo_rx.recv() => match maybe_lo {
                    Some(c) => c,
                    None => break,
                },
            };
            match cmd {
                RuntimeCommand::Shutdown => break,
                RuntimeCommand::Ping(pong) => {
                    let _ = pong.send(());
                }
                RuntimeCommand::Call {
                    method,
                    args,
                    reply,
                    started,
                    started_tx,
                    cancelled,
                    cancel_notify,
                } => {
                    if cancelled.load(Ordering::SeqCst) {
                        continue;
                    }
                    started.store(true, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    if cancelled.load(Ordering::SeqCst) {
                        continue;
                    }
                    let dispatch_ref = dispatch_obj.clone();
                    let result = qjs_ctx
                        .async_with(async |ctx| {
                            invoke_plugin_function(
                                &ctx,
                                dispatch_ref,
                                &method,
                                &args,
                                &cancelled,
                                &cancel_notify,
                            )
                            .await
                        })
                        .await;
                    let _ = reply.send(result);
                }
            }
        }

        // Release Persistent handles before dropping the AsyncContext / AsyncRuntime.
        drop(dispatch_obj);
        host_ctx.kill_all_processes();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(250), qjs_rt.idle()).await;
    });
}

async fn invoke_plugin_function<'js>(
    ctx: &Ctx<'js>,
    dispatch_persistent: Persistent<Object<'static>>,
    method: &str,
    args: &[serde_json::Value],
    cancelled: &AtomicBool,
    cancel_notify: &tokio::sync::Notify,
) -> Result<serde_json::Value, SourceError> {
    let dispatch = dispatch_persistent
        .restore(ctx)
        .map_err(|e| SourceError::Message(format!("failed to restore plugin exports: {e}")))?;

    let func_val: Value<'js> = dispatch
        .get(method)
        .catch(ctx)
        .map_err(|e| caught_to_source_error(ctx, e))?;

    let Some(func) = func_val.into_function() else {
        return Err(SourceError::Unsupported);
    };

    let mut js_args = Args::new(ctx.clone(), args.len());
    js_args
        .this(dispatch)
        .map_err(|e| SourceError::Message(e.to_string()))?;
    for arg in args {
        let js_val = json_to_js(ctx, arg)
            .catch(ctx)
            .map_err(|e| caught_to_source_error(ctx, e))?;
        js_args
            .push_arg(js_val)
            .map_err(|e| SourceError::Message(e.to_string()))?;
    }

    let raw_ret: Value<'js> = func
        .call_arg(js_args)
        .catch(ctx)
        .map_err(|e| caught_to_source_error(ctx, e))?;

    let resolved: Value<'js> = if let Some(promise) = raw_ret.clone().into_promise() {
        let fut = promise.into_future::<Value<'js>>();
        tokio::pin!(fut);
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Err(SourceError::Message(format!(
                    "the plugin didn't answer {method} in time"
                )));
            }
            tokio::select! {
                biased;
                res = &mut fut => {
                    break res.catch(ctx).map_err(|e| caught_to_source_error(ctx, e))?;
                }
                _ = cancel_notify.notified() => {
                    if cancelled.load(Ordering::SeqCst) {
                        return Err(SourceError::Message(format!(
                            "the plugin didn't answer {method} in time"
                        )));
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    } else {
        raw_ret
    };

    js_to_json(ctx, resolved)
        .catch(ctx)
        .map_err(|e| caught_to_source_error(ctx, e))
}

fn format_caught_error(err: CaughtError<'_>) -> String {
    match err {
        CaughtError::Exception(exc) => {
            let msg = exc.message().unwrap_or_default();
            let stack = exc.stack().unwrap_or_default();
            if stack.is_empty() {
                msg
            } else if msg.is_empty() {
                stack
            } else {
                format!("{msg}\n{stack}")
            }
        }
        CaughtError::Value(v) => format!("{v:?}"),
        CaughtError::Error(e) => e.to_string(),
    }
}

fn caught_to_source_error<'js>(_ctx: &Ctx<'js>, err: CaughtError<'js>) -> SourceError {
    match err {
        CaughtError::Exception(exc) => {
            let obj = exc.as_object();
            let code = obj.get::<_, Option<String>>("code").ok().flatten();
            let msg = exc
                .message()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| exc.to_string());
            if let Some(c) = code
                && matches!(
                    c.as_str(),
                    "signIn" | "setup" | "unavailable" | "unsupported"
                )
            {
                return SourceError::from_code(&c, msg);
            }
            SourceError::Message(msg)
        }
        CaughtError::Value(val) => {
            if let Some(obj) = val.as_object() {
                let code = obj.get::<_, Option<String>>("code").ok().flatten();
                let msg = obj
                    .get::<_, Option<String>>("message")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if let Some(c) = code {
                    return SourceError::from_code(&c, msg);
                }
                if !msg.is_empty() {
                    return SourceError::Message(msg);
                }
            }
            if let Some(s) = val.as_string().and_then(|s| s.to_string().ok()) {
                return SourceError::Message(s);
            }
            SourceError::Message(format!("{val:?}"))
        }
        CaughtError::Error(e) => SourceError::Message(e.to_string()),
    }
}

/// Preserves YouTube GeoIP diagnostics from `VirtualYtdlpShim` rescue failures when the
/// original plugin error was `SourceError::Unavailable`.
fn select_stream_rescue_error(orig_err: SourceError, rescue_err: String) -> SourceError {
    if matches!(orig_err, SourceError::Unavailable(_)) && rescue_err.contains("YouTube geo=") {
        SourceError::Unavailable(rescue_err)
    } else {
        orig_err
    }
}

/// Returns only `scheme://host[:port]` for safe logging without query strings, path tokens, or cookies.
fn sanitize_stream_url_for_log(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty>".to_owned();
    }
    match url::Url::parse(trimmed) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("<no-host>");
            match u.port() {
                Some(port) => format!("{}://{host}:{port}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        }
        Err(_) => "<invalid-url>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpiles_typescript_with_oxc() {
        let ts = r#"
            interface TrackPayload {
                id: string;
                title: string;
            }
            enum Kind {
                Song = "song",
            }
            export async function getTrack(id: string): Promise<TrackPayload> {
                const kind: Kind = Kind.Song;
                return { id, title: `Track ${id} (${kind})` };
            }
        "#;
        let js = transpile_ts_to_js(Path::new("plugin.ts"), ts).expect("oxc strips TS");
        assert!(!js.contains("interface TrackPayload"));
        assert!(!js.contains(": string"));
        assert!(js.contains("export async function getTrack"));
    }

    #[test]
    fn sign_in_falls_back_to_account_when_null_or_empty_name() {
        let root =
            std::env::temp_dir().join(format!("nsh-plugin-signin-fallback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"id":"signin-fallback","name":"SignIn Fallback","version":"0.1.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            let mode = 0;
            export async function signIn() {
                mode += 1;
                if (mode === 1) return null;
                if (mode === 2) return { ok: true };
                return { id: "direct", name: "Direct User" };
            }
            export async function account() {
                if (mode === 1) return { id: "u1", name: "Fallback Null" };
                if (mode === 2) return { id: "u2", name: "Fallback EmptyName" };
                return null;
            }
            "#,
        )
        .unwrap();

        let source = PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
            .expect("load fixture plugin");

        let a1 = source
            .sign_in()
            .expect("null signIn falls back to account()");
        assert_eq!(a1.id, "u1");
        assert_eq!(a1.name, "Fallback Null");

        let a2 = source
            .sign_in()
            .expect("empty-name signIn falls back to account()");
        assert_eq!(a2.id, "u2");
        assert_eq!(a2.name, "Fallback EmptyName");

        let a3 = source.sign_in().expect("direct Account signIn");
        assert_eq!(a3.id, "direct");
        assert_eq!(a3.name, "Direct User");

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn high_priority_preempts_queued_low_priority_calls() {
        let root =
            std::env::temp_dir().join(format!("nsh-plugin-priority-lanes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"id":"priority-test","name":"Priority Test","version":"0.1.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            const order = [];
            const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
            export async function search() {
                await sleep(180);
                order.push("blocker");
                return { tracks: [] };
            }
            export async function lyrics() {
                order.push("lyrics");
                return { lrc: "ok" };
            }
            export async function played() {
                order.push("played");
                return null;
            }
            export async function stream() {
                order.push("stream");
                return { url: "https://cdn.example.test/ok.m4a", type: "audio/mp4" };
            }
            export async function getOrder() {
                return order.slice();
            }
            "#,
        )
        .unwrap();

        let source = Arc::new(
            PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
                .expect("load priority plugin"),
        );

        // 1. Occupy the worker with a 180ms high-priority blocker call.
        let s_blocker = Arc::clone(&source);
        let t_blocker = thread::spawn(move || s_blocker.call_raw("search", vec![]));
        thread::sleep(Duration::from_millis(35));

        // 2. Queue two low-priority calls (`lyrics`, `played`) FIRST, then a high-priority `stream`.
        let s_lo1 = Arc::clone(&source);
        let t_lo1 = thread::spawn(move || s_lo1.call_raw("lyrics", vec![]));
        let s_lo2 = Arc::clone(&source);
        let t_lo2 = thread::spawn(move || s_lo2.call_raw("played", vec![]));
        thread::sleep(Duration::from_millis(20));

        let s_hi = Arc::clone(&source);
        let t_hi = thread::spawn(move || s_hi.call_raw("stream", vec![]));

        assert!(t_blocker.join().unwrap().is_ok());
        assert!(t_hi.join().unwrap().is_ok());
        assert!(t_lo1.join().unwrap().is_ok());
        assert!(t_lo2.join().unwrap().is_ok());

        let order_val = source.call_raw("getOrder", vec![]).expect("read order");
        let order: Vec<String> = serde_json::from_value(order_val).unwrap();
        assert_eq!(order[0], "blocker");
        assert_eq!(
            order[1], "stream",
            "high-priority `stream` must preempt queued low-priority calls, got: {order:?}"
        );
        assert!(order[2..].contains(&"lyrics".to_owned()));
        assert!(order[2..].contains(&"played".to_owned()));

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn queued_and_low_priority_timeouts_do_not_restart_or_kill_processes() {
        let root = std::env::temp_dir().join(format!(
            "nsh-plugin-nondestructive-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
                "id":"timeout-safe",
                "name":"Timeout Safe",
                "version":"0.1.0",
                "main":"index.js",
                "permissions":{"process":["ffmpeg"]}
            }"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
            let generation = 0;
            export async function bumpGen() {
                generation += 1;
                return generation;
            }
            export async function getGen() {
                return generation;
            }
            export async function lyrics() {
                await sleep(300);
                return { lrc: "late" };
            }
            export async function track() {
                return { id: "t1", title: "Queued" };
            }
            "#,
        )
        .unwrap();

        let source = Arc::new(
            PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
                .expect("load timeout-safe plugin"),
        );

        // Seed in-VM state so we can prove the VM did NOT restart.
        assert_eq!(
            source.call_raw("bumpGen", vec![]).unwrap(),
            serde_json::json!(1)
        );

        // Register 4 rescue relays -> HostContext keeps the newest 3 active and kills the oldest.
        for _ in 0..4 {
            let relay = VirtualFfmpegShim::spawn(&[
                "-i".to_owned(),
                "http://127.0.0.1:9/unused".to_owned(),
                "-listen".to_owned(),
                "1".to_owned(),
                "http://127.0.0.1:0/".to_owned(),
            ])
            .expect("spawn virtual relay");
            source.host_ctx.register_rescue_relay(relay);
        }
        assert_eq!(
            source.host_ctx.active_process_count(),
            3,
            "register_rescue_relay must cap active rescue relays at 3"
        );

        // 1. Start a slow low-priority `lyrics` call with a 140ms timeout (times out while started).
        let s_lo = Arc::clone(&source);
        let t_lo = thread::spawn(move || {
            s_lo.call_raw_with_timeout("lyrics", vec![], Duration::from_millis(140))
        });

        // Wait until `lyrics` is executing, then enqueue a high-priority `track` call with an
        // explicit 40ms `call_timeout_override` (which caps total wait) so it strictly times out
        // BEFORE `lyrics` finishes or cancels (`cancelled = true`, `started = false`).
        thread::sleep(Duration::from_millis(30));
        source.set_call_timeout_override(Some(Duration::from_millis(40)));
        let queued_res = source.call_raw_with_timeout("track", vec![], Duration::from_millis(40));
        source.set_call_timeout_override(None);
        assert!(
            queued_res.is_err(),
            "queued high-priority call should time out"
        );

        let lo_res = t_lo.join().unwrap();
        assert!(
            lo_res.is_err(),
            "slow low-priority lyrics call should time out"
        );

        // Neither timeout may restart the worker or kill the 3 active rescue relays!
        assert_eq!(
            source.host_ctx.active_process_count(),
            3,
            "queued and low-priority timeouts must NOT kill active processes"
        );

        // Wait for the in-flight 300ms `lyrics` promise to finish, then confirm the SAME VM instance is alive.
        thread::sleep(Duration::from_millis(320));
        assert_eq!(
            source.call_raw("getGen", vec![]).unwrap(),
            serde_json::json!(1),
            "VM state must remain intact (worker must NOT have restarted)"
        );
        assert_eq!(source.host_ctx.active_process_count(), 3);

        source.shutdown();
        assert_eq!(
            source.host_ctx.active_process_count(),
            0,
            "shutdown must kill all tracked rescue relays"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn select_stream_rescue_error_surfaces_youtube_geo_on_unavailable() {
        let surfaced = select_stream_rescue_error(
            SourceError::Unavailable("This video is not available".to_owned()),
            "This video is not available (YouTube geo=CN; switch proxy/VPN region)".to_owned(),
        );
        assert_eq!(
            surfaced,
            SourceError::Unavailable(
                "This video is not available (YouTube geo=CN; switch proxy/VPN region)".to_owned()
            )
        );

        let preserved_no_geo = select_stream_rescue_error(
            SourceError::Unavailable("Original unavailable".to_owned()),
            "network timeout".to_owned(),
        );
        assert_eq!(
            preserved_no_geo,
            SourceError::Unavailable("Original unavailable".to_owned())
        );

        let preserved_non_unavailable = select_stream_rescue_error(
            SourceError::Message("Original message".to_owned()),
            "This video is not available (YouTube geo=US)".to_owned(),
        );
        assert_eq!(
            preserved_non_unavailable,
            SourceError::Message("Original message".to_owned())
        );
    }

    #[test]
    fn slow_async_interactive_timeout_preserves_vm_and_running_processes() {
        let root = std::env::temp_dir().join(format!(
            "nsh-plugin-slow-async-ping-ok-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
                "id":"slow-async-safe",
                "name":"Slow Async Safe",
                "version":"0.1.0",
                "main":"index.js",
                "permissions":{"process":["ffmpeg"]}
            }"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            let generation = 0;
            export async function bumpGen() {
                generation += 1;
                return generation;
            }
            export async function getGen() {
                return generation;
            }
            export async function search() {
                await host.sleep(600);
                return { tracks: [] };
            }
            "#,
        )
        .unwrap();

        let source = PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
            .expect("load slow-async plugin");

        assert_eq!(
            source.call_raw("bumpGen", vec![]).unwrap(),
            serde_json::json!(1)
        );
        let relay = VirtualFfmpegShim::spawn(&[
            "-i".to_owned(),
            "http://127.0.0.1:9/unused".to_owned(),
            "-listen".to_owned(),
            "1".to_owned(),
            "http://127.0.0.1:0/".to_owned(),
        ])
        .expect("spawn virtual relay");
        source.host_ctx.register_rescue_relay(relay);
        assert_eq!(source.host_ctx.active_process_count(), 1);

        // High-priority interactive call times out while awaiting slow async I/O (`host.sleep`):
        // cancellation yields control back to worker loop, `Ping` succeeds, so VM & relay survive.
        let err = source
            .call_raw_with_timeout("search", vec![], Duration::from_millis(80))
            .expect_err("slow search must time out");
        assert!(err.to_string().contains("in time"), "got {err}");

        assert_eq!(
            source.host_ctx.active_process_count(),
            1,
            "slow async interactive timeout must NOT kill running processes"
        );
        assert_eq!(
            source.call_raw("getGen", vec![]).unwrap(),
            serde_json::json!(1),
            "VM state must be preserved when liveness ping succeeds"
        );

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synchronous_infinite_loop_fails_ping_and_restarts_cleanly() {
        let root = std::env::temp_dir().join(format!(
            "nsh-plugin-sync-hang-ping-fail-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"id":"sync-hang","name":"Sync Hang","version":"0.1.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            let generation = 0;
            export async function bumpGen() {
                generation += 1;
                return generation;
            }
            export async function getGen() {
                return generation;
            }
            export async function search() {
                while (true) {}
            }
            "#,
        )
        .unwrap();

        let source = PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
            .expect("load sync-hang plugin");
        assert_eq!(
            source.call_raw("bumpGen", vec![]).unwrap(),
            serde_json::json!(1)
        );

        let err = source
            .call_raw_with_timeout("search", vec![], Duration::from_millis(80))
            .expect_err("infinite JS loop must time out");
        assert!(err.to_string().contains("in time"), "got {err}");

        // Liveness ping timed out on the wedged worker and restarted it -> fresh VM (`generation == 0`).
        assert_eq!(
            source.call_raw("getGen", vec![]).unwrap(),
            serde_json::json!(0),
            "wedged JS loop must trigger worker restart cleanly"
        );

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn queued_call_timeout_begins_at_execution_start_not_enqueue() {
        let root = std::env::temp_dir().join(format!(
            "nsh-plugin-queue-start-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"id":"queue-start-timeout","name":"Queue Start Timeout","version":"0.1.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
            export async function search() {
                await sleep(220);
                return { tracks: [] };
            }
            export async function lyrics() {
                await sleep(30);
                return { lrc: "queued-ok" };
            }
            export async function slowLyrics() {
                await sleep(220);
                return { lrc: "too-slow" };
            }
            "#,
        )
        .unwrap();

        let source = Arc::new(
            PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
                .expect("load queue-start-timeout plugin"),
        );

        // 1. Occupy the worker with a 220ms active `search` call.
        let s_blocker = Arc::clone(&source);
        let t_blocker = thread::spawn(move || {
            s_blocker.call_raw_with_timeout("search", vec![], Duration::from_secs(5))
        });
        thread::sleep(Duration::from_millis(35));

        // 2. Enqueue `lyrics` with a 120ms execution timeout (shorter than the remaining ~185ms
        //    queue wait, but longer than its 30ms execution body). With `call_timeout_override == None`,
        //    the 120ms timeout must NOT begin until `lyrics` actually starts executing.
        let t0 = Instant::now();
        let queued = source
            .call_raw_with_timeout("lyrics", vec![], Duration::from_millis(120))
            .expect("queued call must not time out while waiting behind an active call");
        let elapsed = t0.elapsed();
        assert_eq!(queued, serde_json::json!({ "lrc": "queued-ok" }));
        assert!(
            elapsed >= Duration::from_millis(180),
            "expected queued call to wait behind blocker (~215ms), elapsed={elapsed:?}"
        );
        assert!(t_blocker.join().unwrap().is_ok());

        // 3. A call whose own execution exceeds its timeout MUST still time out cleanly.
        let slow_err = source
            .call_raw_with_timeout("slowLyrics", vec![], Duration::from_millis(90))
            .expect_err("genuinely slow execution must still time out");
        assert!(slow_err.to_string().contains("in time"), "got {slow_err}");

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recently_exhausted_ytdlp_skips_duplicate_stream_rescue() {
        let root =
            std::env::temp_dir().join(format!("nsh-plugin-skip-dup-rescue-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
                "id":"yt-rescue-dedup",
                "name":"YT Rescue Dedup",
                "version":"0.1.0",
                "main":"index.js",
                "permissions":{"process":["yt-dlp"]}
            }"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("index.js"),
            r#"
            export async function stream() {
                const err = new Error("yt-dlp failed");
                err.code = "unavailable";
                throw err;
            }
            "#,
        )
        .unwrap();

        let source = PluginSource::load(&plugin_dir, &data_dir, HostRuntimeOptions::default())
            .expect("load rescue-dedup plugin");

        // Record that `VirtualYtdlpShim` just exhausted all clients for `dQw4w9WgXcQ`.
        source.host_ctx.record_ytdlp_exhausted("dQw4w9WgXcQ");
        assert!(
            source
                .host_ctx
                .was_ytdlp_recently_exhausted("dQw4w9WgXcQ", Duration::from_secs(10))
        );
        assert!(
            !source
                .host_ctx
                .was_ytdlp_recently_exhausted("otherVid123", Duration::from_secs(10))
        );

        // `try_youtube_stream_rescue` must short-circuit with `None` immediately (no duplicate cascade).
        let t0 = Instant::now();
        let skipped = source.try_youtube_stream_rescue(
            "dQw4w9WgXcQ",
            Quality::High,
            &SourceError::Unavailable("yt-dlp failed".to_owned()),
        );
        assert!(skipped.is_none());
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "duplicate rescue should skip immediately, elapsed={:?}",
            t0.elapsed()
        );

        source.host_ctx.clear_ytdlp_exhausted();
        assert!(
            !source
                .host_ctx
                .was_ytdlp_recently_exhausted("dQw4w9WgXcQ", Duration::from_secs(10))
        );

        source.shutdown();
        let _ = fs::remove_dir_all(&root);
    }
}
