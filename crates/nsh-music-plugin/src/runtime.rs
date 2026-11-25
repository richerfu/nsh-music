use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

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

use crate::compat::{analyze_plugin_dir, CompatReport};
use crate::headers::HeaderRequestKind;
use crate::host::{
    install_host_globals, js_to_json, json_to_js, HostContext, HostRuntimeOptions, WebSignInHandler,
};
use crate::manifest::{ManifestError, PluginLocales, PluginManifest};

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

    let ret = Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
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

enum RuntimeCommand {
    Call {
        method: String,
        args: Vec<serde_json::Value>,
        reply: std_mpsc::SyncSender<Result<serde_json::Value, SourceError>>,
    },
    Shutdown,
}

/// An isolated QuickJS-backed `MusicSource` instance running in its own dedicated actor thread.
pub struct PluginSource {
    manifest: Arc<PluginManifest>,
    locales: Arc<PluginLocales>,
    host_ctx: Arc<HostContext>,
    compat_report: CompatReport,
    exports: Arc<HashSet<String>>,
    tx: tokio_mpsc::UnboundedSender<RuntimeCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
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

        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<RuntimeCommand>();
        let (boot_tx, boot_rx) = std_mpsc::sync_channel::<Result<HashSet<String>, RuntimeError>>(1);

        let worker_dir = plugin_dir.to_path_buf();
        let worker_host = Arc::clone(&host_ctx);
        let thread_name = format!("nsh-plugin-{}", manifest.id);

        let handle = thread::Builder::new()
            .name(thread_name)
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                run_plugin_actor(worker_dir, entry_rel, transpiled, worker_host, cmd_rx, boot_tx);
            })
            .map_err(|e| RuntimeError::Worker(format!("failed to spawn worker thread: {e}")))?;

        let exports = boot_rx
            .recv()
            .map_err(|_| RuntimeError::Worker("plugin worker exited during initialization".to_owned()))??;

        Ok(Self {
            manifest,
            locales,
            host_ctx,
            compat_report,
            exports: Arc::new(exports),
            tx: cmd_tx,
            worker: Mutex::new(Some(handle)),
            granted: AtomicBool::new(true),
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

    /// Updates permission grant state surfaced in `compatibility_info()` and host enforcement.
    pub fn set_granted(&self, granted: bool) {
        self.granted.store(granted, Ordering::SeqCst);
        self.host_ctx.set_granted(granted);
    }

    /// Invokes a specific exported JS function by name with JSON arguments.
    pub fn call_raw(
        &self,
        method: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, SourceError> {
        if !self.exports.contains(method) {
            return Err(SourceError::Unsupported);
        }
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        self.tx
            .send(RuntimeCommand::Call {
                method: method.to_owned(),
                args,
                reply: reply_tx,
            })
            .map_err(|_| SourceError::Message("plugin runtime has shut down".to_owned()))?;
        reply_rx
            .recv()
            .map_err(|_| SourceError::Message("plugin runtime stopped while awaiting response".to_owned()))?
    }

    fn call_any(
        &self,
        candidates: &[&str],
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, SourceError> {
        for name in candidates {
            if self.exports.contains(*name) {
                return self.call_raw(name, args);
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
        if self.exports.contains(canonical) {
            let args = obj_arg.into_iter().collect();
            return self.call_raw(canonical, args);
        }
        self.call_any(legacy, legacy_args)
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
        let val = self.call_any(&["getQrSignIn"], vec![])?;
        let mut session: QrSignInSession = parse_json_result(val)?;
        if session.qr_svg.is_empty() && !session.qr_url.is_empty()
            && let Ok(code) = qrcode::QrCode::new(session.qr_url.as_bytes()) {
                session.qr_svg = code
                    .render::<qrcode::render::svg::Color<'_>>()
                    .min_dimensions(200, 200)
                    .build();
            }
        Ok(session)
    }

    /// Calls `pollQrSignIn(token)` to check QR login progress.
    pub fn poll_qr_sign_in(&self, token: &str) -> Result<QrSignInStatus, SourceError> {
        let val = self.call_any(&["pollQrSignIn"], vec![serde_json::json!(token)])?;
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
        let _ = self.tx.send(RuntimeCommand::Shutdown);
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take() {
                let _ = handle.join();
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
        if info.settings.is_empty() && self.has_any(&["getConfigSchema"])
            && let Ok(dynamic) = self.get_config_schema() {
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
        let val = self.call_any(&["signIn"], vec![])?;
        parse_json_result(val)
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
        let _ = self.call_raw(
            "saveAlbum",
            vec![serde_json::json!({ "id": id, "on": on })],
        )?;
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
        let val = self.call_mygo_or_legacy(
            "stream",
            Some(serde_json::json!({
                "id": id,
                "quality": quality.as_str(),
            })),
            &["getStreamUrl"],
            vec![serde_json::json!(id), serde_json::json!(quality.as_str())],
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
        let _ = self.call_raw("remoteSeek", vec![serde_json::json!({ "position": position })])?;
        Ok(())
    }

    fn remote_volume(&self, percent: i32) -> Result<(), SourceError> {
        let _ = self.call_raw("remoteVolume", vec![serde_json::json!({ "percent": percent })])?;
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
        let _ = self.call_raw("remoteUse", vec![serde_json::json!({ "deviceId": device_id })])?;
        Ok(())
    }
}

fn parse_json_result<T: for<'de> Deserialize<'de>>(val: serde_json::Value) -> Result<T, SourceError> {
    serde_json::from_value(val)
        .map_err(|e| SourceError::Message(format!("invalid plugin return payload: {e}")))
}

fn run_plugin_actor(
    plugin_root: PathBuf,
    entry_rel: String,
    transpiled_source: String,
    host_ctx: Arc<HostContext>,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<RuntimeCommand>,
    boot_tx: std_mpsc::SyncSender<Result<HashSet<String>, RuntimeError>>,
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
        qjs_rt.set_memory_limit(128 * 1024 * 1024).await;
        qjs_rt.set_max_stack_size(2 * 1024 * 1024).await;
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

                    let module = Module::declare(ctx.clone(), entry_rel.as_str(), transpiled_source)
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
                        && let Some(default_obj) = default_val.into_object() {
                            for prop in default_obj.props::<String, Value<'_>>() {
                                if let Ok((k, v)) = prop
                                    && v.is_function() {
                                        let _ = dispatch.set(k.as_str(), v);
                                        export_names.insert(k);
                                    }
                            }
                        }

                    for prop in ns.props::<String, Value<'_>>() {
                        if let Ok((k, v)) = prop
                            && k != "default" && v.is_function() {
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

        // Process RPC invocations sequentially on the isolated QuickJS VM.
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                RuntimeCommand::Shutdown => break,
                RuntimeCommand::Call {
                    method,
                    args,
                    reply,
                } => {
                    let dispatch_ref = dispatch_obj.clone();
                    let result = qjs_ctx
                        .async_with(async |ctx| {
                            invoke_plugin_function(&ctx, dispatch_ref, &method, &args).await
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
        promise
            .into_future::<Value<'js>>()
            .await
            .catch(ctx)
            .map_err(|e| caught_to_source_error(ctx, e))?
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
                && matches!(c.as_str(), "signIn" | "setup" | "unavailable" | "unsupported") {
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
}
