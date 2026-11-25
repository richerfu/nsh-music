//! `mygo-music` compatible JS/TS plugin runtime for `nsh-music`.
//!
//! Features:
//! - `plugin.json` manifest & `locales/<lang>.json` i18n loading ([`manifest`])
//! - Domain/glob + resource-kind `requestHeaders` resolution with `{{config.xxx}}` / `{{cookie}}` expansion ([`headers`])
//! - Pure-Rust crypto (`md5`, `sha256`, `hmac_sha256`, `aes_ecb_encrypt`/`decrypt`, `base64`) with zero OpenSSL ([`crypto`])
//! - `oxc` TypeScript stripping + isolated per-plugin `quickjs-jit` actor implementing `MusicSource` ([`runtime`])
//! - Complete `host.*` JS global with cross-platform `VirtualFfmpegShim` interception and mobile sandbox guards ([`host`])
//! - Safe local / ZIP (zip-slip protected) / GitHub plugin installer ([`installer`])
//! - Static JS/TS + manifest compatibility analyzer ([`compat`])

pub mod compat;
pub mod crypto;
pub mod headers;
pub mod host;
pub mod installer;
pub mod manifest;
pub mod mobile_compat;
pub mod runtime;
pub mod virtual_ytdlp;
pub(crate) mod yt_clients;
pub(crate) mod yt_ejs;
pub(crate) mod yt_format_selector;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nsh_music_core::model::SettingDef;
use nsh_music_core::source::{MusicSource, SignInPromptCallback, SourceCompat};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::compat::{
    CompatFinding, CompatReport, CompatTier, analyze_plugin_dir, analyze_source,
};
pub use crate::headers::{HeaderRequestKind, HeaderRule, RequestHeadersResolver};
pub use crate::host::{
    HostContext, HostRuntimeOptions, ManagedProcess, MobileSandboxError, OpenUrlHandler,
    SpawnOptions, VirtualPipeHandle, WebSignInHandler, WebSignInRequest, WebSignInResult,
};
pub use crate::installer::{
    GitHubRepoSpec, InstallSource, InstalledPluginMeta, InstallerError, ORIGIN_FILE,
    PendingPluginInstall, PluginInstaller, PluginOrigin, read_plugin_origin,
};
pub use crate::manifest::{
    ManifestError, PluginLocales, PluginManifest, PluginPermissions, effective_host_version,
    is_valid_plugin_id, set_host_app_version,
};
pub use crate::mobile_compat::{
    FfmpegVirtualShim, MobileProcessVirtualizer, VirtualProcessShim, VirtualSpawnError,
    VirtualSpawnResult,
};
pub use crate::runtime::{
    DEFAULT_CALL_TIMEOUT, EXTRA_PLUGIN_EXPORTS, MYGO_PLUGIN_EXPORTS, PluginSource, QrSignInSession,
    QrSignInStatus, RuntimeError, SIGN_IN_CALL_TIMEOUT, transpile_ts_to_js,
};
pub use crate::virtual_ytdlp::{
    VIRTUAL_YTDLP_VERSION_BANNER, VirtualYtdlpShim, YtdlpVirtualShim, is_ytdlp_command,
};

/// Top-level errors returned by [`PluginManager`].
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("installer error: {0}")]
    Installer(#[from] InstallerError),
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("plugin `{0}` not found")]
    NotFound(String),
}

/// UI-friendly summary of an installed plugin for Settings and Plugin Manager views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub homepage: String,
    pub repository: String,
    pub enabled: bool,
    pub loaded: bool,
    pub granted: bool,
    pub dir: PathBuf,
    pub source: InstallSource,
    pub compat: SourceCompat,
    pub report: CompatReport,
    pub settings: Vec<SettingDef>,
    pub load_error: Option<String>,
}

/// Manages lifecycle, persistence, installation, and runtime actors for all installed music plugins.
pub struct PluginManager {
    installer: PluginInstaller,
    data_dir: PathBuf,
    loaded: RwLock<HashMap<String, Arc<PluginSource>>>,
    load_errors: RwLock<HashMap<String, String>>,
    locale: RwLock<String>,
    sign_in_prompt: RwLock<Option<SignInPromptCallback>>,
    web_sign_in_handler: RwLock<Option<WebSignInHandler>>,
    open_url_handler: RwLock<Option<OpenUrlHandler>>,
    is_mobile_override: Option<bool>,
}

impl PluginManager {
    /// Creates a `PluginManager` backed by explicit `plugins_dir` and `data_dir` directories.
    pub fn new(
        plugins_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
    ) -> Result<Self, PluginError> {
        let installer = PluginInstaller::new(plugins_dir)?;
        let data_dir = data_dir.into();
        let _ = std::fs::create_dir_all(&data_dir);
        Ok(Self {
            installer,
            data_dir,
            loaded: RwLock::new(HashMap::new()),
            load_errors: RwLock::new(HashMap::new()),
            locale: RwLock::new("zh-CN".to_owned()),
            sign_in_prompt: RwLock::new(None),
            web_sign_in_handler: RwLock::new(None),
            open_url_handler: RwLock::new(None),
            is_mobile_override: None,
        })
    }

    /// Creates a `PluginManager` using the platform's default app-local data directory.
    pub fn with_default_dirs() -> Result<Self, PluginError> {
        let base = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("nsh-music");
        Self::new(base.join("plugins"), base.join("plugin-data"))
    }

    /// Overrides mobile platform detection (primarily for cross-platform sandbox tests).
    pub fn with_mobile_override(mut self, is_mobile: bool) -> Self {
        self.is_mobile_override = Some(is_mobile);
        self
    }

    /// Returns the underlying [`PluginInstaller`].
    pub fn installer(&self) -> &PluginInstaller {
        &self.installer
    }

    /// Returns the root directory where plugins are installed.
    pub fn plugins_dir(&self) -> &Path {
        self.installer.plugins_dir()
    }

    /// Returns the per-plugin persistent data root directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Discovers all installed plugins and loads every enabled plugin into an isolated QuickJS runtime.
    pub fn load_all(&self) -> Vec<(String, Result<Arc<PluginSource>, PluginError>)> {
        let Ok(installed) = self.installer.discover_plugins() else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for meta in installed {
            if !meta.enabled {
                continue;
            }
            let id = meta.id.clone();
            let res = self.load_from_meta(&meta);
            results.push((id, res));
        }
        results
    }

    /// Loads a single plugin directly from a directory without copying it into `plugins_dir`.
    pub fn load_plugin_dir(&self, dir: &Path) -> Result<Arc<PluginSource>, PluginError> {
        let source = Arc::new(PluginSource::load(
            dir,
            &self.data_dir,
            self.runtime_options(),
        )?);
        let id = source.manifest().id.clone();
        if let Ok(mut errors) = self.load_errors.write() {
            errors.remove(&id);
        }
        if let Ok(mut loaded) = self.loaded.write()
            && let Some(prev) = loaded.insert(id, Arc::clone(&source))
        {
            prev.shutdown();
        }
        Ok(source)
    }

    /// Installs a plugin from a local directory and immediately loads it if enabled.
    pub fn install_from_dir(&self, dir: &Path) -> Result<Arc<PluginSource>, PluginError> {
        let meta = self.installer.install_from_dir(dir)?;
        self.load_from_meta(&meta)
    }

    /// Installs a plugin from a `.zip` archive (with zip-slip protection) and loads it.
    pub fn install_from_zip(&self, zip_path: &Path) -> Result<Arc<PluginSource>, PluginError> {
        let meta = self.installer.install_from_zip(zip_path)?;
        self.load_from_meta(&meta)
    }

    /// Downloads and installs a plugin from GitHub (`owner/repo`, `owner/repo@ref`, or URL) and loads it.
    pub fn install_from_github(&self, spec: &str) -> Result<Arc<PluginSource>, PluginError> {
        let meta = self.installer.install_from_github(spec)?;
        self.load_from_meta(&meta)
    }

    /// Inspects and stages a local directory plugin for confirmation before installing.
    pub fn stage_from_dir(&self, dir: &Path) -> Result<PendingPluginInstall, PluginError> {
        Ok(self.installer.stage_from_dir(dir)?)
    }

    /// Inspects and stages a `.zip` plugin archive for confirmation before installing.
    pub fn stage_from_zip(&self, zip_path: &Path) -> Result<PendingPluginInstall, PluginError> {
        Ok(self.installer.stage_from_zip(zip_path)?)
    }

    /// Downloads and stages a GitHub plugin repository for confirmation before installing.
    pub fn stage_from_github(&self, spec: &str) -> Result<PendingPluginInstall, PluginError> {
        Ok(self.installer.stage_from_github(spec)?)
    }

    /// Finalizes a staged installation with explicit program-permission grant state and loads it.
    pub fn confirm_staged(
        &self,
        pending: &PendingPluginInstall,
        granted: bool,
    ) -> Result<Arc<PluginSource>, PluginError> {
        let meta = self.installer.confirm_staged(pending, granted)?;
        self.load_from_meta(&meta)
    }

    /// Discards a staged plugin installation.
    pub fn cancel_staged(&self, pending: &PendingPluginInstall) {
        self.installer.cancel_staged(pending);
    }

    /// Updates program execution permission grant state for an installed plugin, persists
    /// `granted.json`, and restarts any running runtime (killing spawned processes and refreshing
    /// loopback fetch permissions).
    pub fn set_granted(&self, plugin_id: &str, granted: bool) -> Result<(), PluginError> {
        self.installer.set_granted(plugin_id, granted)?;
        let granted_path = self.data_dir.join(plugin_id).join("granted.json");
        let _ =
            crate::installer::atomic_write(&granted_path, if granted { b"true" } else { b"false" });
        if let Some(loaded) = self.get(plugin_id) {
            loaded.set_granted(granted);
            loaded.restart()?;
        }
        Ok(())
    }

    /// Checks whether a newer version is available upstream for a GitHub-installed plugin.
    pub fn check_update(&self, plugin_id: &str) -> Result<Option<String>, PluginError> {
        Ok(self.installer.check_github_update(plugin_id)?)
    }

    /// Unloads and deletes an installed plugin.
    pub fn uninstall(&self, plugin_id: &str) -> Result<(), PluginError> {
        self.unload(plugin_id);
        self.installer.uninstall(plugin_id)?;
        Ok(())
    }

    /// Enables or disables a plugin, loading or shutting down its QuickJS runtime accordingly.
    pub fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<(), PluginError> {
        let meta = self.installer.set_enabled(plugin_id, enabled)?;
        if enabled {
            let _ = self.load_from_meta(&meta)?;
        } else {
            self.unload(plugin_id);
        }
        Ok(())
    }

    /// Reloads a plugin from disk, replacing its active QuickJS runtime cleanly.
    pub fn reload(&self, plugin_id: &str) -> Result<Arc<PluginSource>, PluginError> {
        let installed = self.installer.discover_plugins()?;
        let meta = installed
            .into_iter()
            .find(|m| m.id == plugin_id)
            .ok_or_else(|| PluginError::NotFound(plugin_id.to_owned()))?;
        self.unload(plugin_id);
        self.load_from_meta(&meta)
    }

    /// Returns a loaded plugin by ID.
    pub fn get(&self, plugin_id: &str) -> Option<Arc<PluginSource>> {
        self.loaded.read().ok()?.get(plugin_id).cloned()
    }

    /// Returns all currently loaded plugins as `Arc<dyn MusicSource>`, sorted by plugin ID.
    pub fn sources(&self) -> Vec<Arc<dyn MusicSource>> {
        let Ok(guard) = self.loaded.read() else {
            return Vec::new();
        };
        let mut pairs: Vec<(&String, &Arc<PluginSource>)> = guard.iter().collect();
        pairs.sort_by_key(|(a, _)| *a);
        pairs
            .into_iter()
            .map(|(_, src)| Arc::clone(src) as Arc<dyn MusicSource>)
            .collect()
    }

    /// Returns UI-ready summaries of all installed (and directly loaded) plugins.
    pub fn plugins(&self) -> Vec<PluginSummary> {
        let locale = self.locale();
        let installed = self.installer.discover_plugins().unwrap_or_default();
        let loaded_guard = self.loaded.read().ok();
        let errors_guard = self.load_errors.read().ok();

        let mut out = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for meta in installed {
            seen_ids.insert(meta.id.clone());
            let loaded_src = loaded_guard.as_ref().and_then(|m| m.get(&meta.id).cloned());
            let load_error = errors_guard.as_ref().and_then(|m| m.get(&meta.id).cloned());

            if let Some(src) = loaded_src {
                let manifest = src.manifest();
                let info = src.info();
                out.push(PluginSummary {
                    id: manifest.id.clone(),
                    name: info.name,
                    version: manifest.version.clone(),
                    description: manifest.description.clone(),
                    author: manifest.author.clone(),
                    homepage: manifest.homepage.clone(),
                    repository: manifest.repository.clone(),
                    enabled: meta.enabled,
                    loaded: true,
                    granted: meta.granted,
                    dir: meta.dir,
                    source: meta.source,
                    compat: src.compatibility_info(),
                    report: src.compat_report().clone(),
                    settings: info.settings,
                    load_error: None,
                });
            } else {
                let manifest =
                    PluginManifest::from_dir(&meta.dir).unwrap_or_else(|_| PluginManifest {
                        id: meta.id.clone(),
                        name: meta.name.clone(),
                        version: meta.version.clone(),
                        ..PluginManifest::default()
                    });
                let locales = PluginLocales::load_from_dir(&meta.dir);
                let info = manifest.to_source_info(&locales, &locale);
                let report = analyze_plugin_dir(&meta.dir).unwrap_or_default();
                let compat = report.to_source_compat(meta.granted);
                out.push(PluginSummary {
                    id: meta.id,
                    name: info.name,
                    version: manifest.version,
                    description: manifest.description,
                    author: manifest.author,
                    homepage: manifest.homepage,
                    repository: manifest.repository,
                    enabled: meta.enabled,
                    loaded: false,
                    granted: meta.granted,
                    dir: meta.dir,
                    source: meta.source,
                    compat,
                    report,
                    settings: info.settings,
                    load_error,
                });
            }
        }

        // Also include plugins loaded directly via `load_plugin_dir` outside `plugins_dir`.
        if let Some(loaded) = loaded_guard {
            for (id, src) in loaded.iter() {
                if seen_ids.contains(id) {
                    continue;
                }
                let manifest = src.manifest();
                let info = src.info();
                out.push(PluginSummary {
                    id: manifest.id.clone(),
                    name: info.name,
                    version: manifest.version.clone(),
                    description: manifest.description.clone(),
                    author: manifest.author.clone(),
                    homepage: manifest.homepage.clone(),
                    repository: manifest.repository.clone(),
                    enabled: true,
                    loaded: true,
                    granted: true,
                    dir: PathBuf::new(),
                    source: InstallSource::Discovered,
                    compat: src.compatibility_info(),
                    report: src.compat_report().clone(),
                    settings: info.settings,
                    load_error: None,
                });
            }
        }

        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Sets the active locale on the manager and every loaded plugin.
    pub fn set_locale(&self, locale: &str) {
        if let Ok(mut g) = self.locale.write() {
            *g = locale.to_owned();
        }
        if let Ok(guard) = self.loaded.read() {
            for src in guard.values() {
                src.set_locale(locale);
            }
        }
    }

    /// Returns the active locale string.
    pub fn locale(&self) -> String {
        self.locale
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "zh-CN".to_owned())
    }

    /// Installs or clears the `SignInPromptCallback` on the manager and all loaded plugins.
    pub fn set_sign_in_prompt(&self, cb: Option<SignInPromptCallback>) {
        if let Ok(mut g) = self.sign_in_prompt.write() {
            *g = cb.clone();
        }
        if let Ok(guard) = self.loaded.read() {
            for src in guard.values() {
                src.set_sign_in_prompt(cb.clone());
            }
        }
    }

    /// Installs or clears the native WebView `WebSignInHandler` on the manager and all loaded plugins.
    pub fn set_web_sign_in_handler(&self, handler: Option<WebSignInHandler>) {
        if let Ok(mut g) = self.web_sign_in_handler.write() {
            *g = handler.clone();
        }
        if let Ok(guard) = self.loaded.read() {
            for src in guard.values() {
                src.set_web_sign_in_handler(handler.clone());
            }
        }
    }

    /// Installs or clears the URL opener handler for `host.openURL` on the manager and all loaded plugins.
    pub fn set_open_url_handler(&self, handler: Option<OpenUrlHandler>) {
        if let Ok(mut g) = self.open_url_handler.write() {
            *g = handler.clone();
        }
        if let Ok(guard) = self.loaded.read() {
            for src in guard.values() {
                src.set_open_url_handler(handler.clone());
            }
        }
    }

    /// Runs static compatibility analysis on a plugin directory without booting its JS runtime.
    pub fn analyze_dir(dir: &Path) -> Result<CompatReport, PluginError> {
        Ok(analyze_plugin_dir(dir)?)
    }

    /// Gracefully shuts down every loaded plugin actor and cleans up spawned processes.
    pub fn shutdown_all(&self) {
        if let Ok(mut loaded) = self.loaded.write() {
            for (_, src) in loaded.drain() {
                src.shutdown();
            }
        }
    }

    fn unload(&self, plugin_id: &str) {
        if let Ok(mut loaded) = self.loaded.write()
            && let Some(src) = loaded.remove(plugin_id)
        {
            src.shutdown();
        }
        if let Ok(mut errors) = self.load_errors.write() {
            errors.remove(plugin_id);
        }
    }

    fn load_from_meta(&self, meta: &InstalledPluginMeta) -> Result<Arc<PluginSource>, PluginError> {
        match PluginSource::load(&meta.dir, &self.data_dir, self.runtime_options()) {
            Ok(source) => {
                source.set_granted(meta.granted);
                let arc = Arc::new(source);
                if let Ok(mut errors) = self.load_errors.write() {
                    errors.remove(&meta.id);
                }
                if let Ok(mut loaded) = self.loaded.write()
                    && let Some(prev) = loaded.insert(meta.id.clone(), Arc::clone(&arc))
                {
                    prev.shutdown();
                }
                Ok(arc)
            }
            Err(err) => {
                if let Ok(mut errors) = self.load_errors.write() {
                    errors.insert(meta.id.clone(), err.to_string());
                }
                Err(PluginError::Runtime(err))
            }
        }
    }

    fn runtime_options(&self) -> HostRuntimeOptions {
        HostRuntimeOptions {
            locale: Some(self.locale()),
            is_mobile_override: self.is_mobile_override,
            platform_override: None,
            sign_in_prompt: self.sign_in_prompt.read().ok().and_then(|g| g.clone()),
            web_sign_in_handler: self.web_sign_in_handler.read().ok().and_then(|g| g.clone()),
            open_url_handler: self.open_url_handler.read().ok().and_then(|g| g.clone()),
        }
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    use nsh_music_core::model::{Quality, SearchKind};
    use nsh_music_core::source::{PluginCompatLevel, RequestKind};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn temp_workspace(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nsh-music-plugin-test-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp workspace");
        dir
    }

    #[test]
    fn loads_js_default_export_plugin_and_dispatches_music_source() {
        let root = temp_workspace("js-default");
        let fixture = root.join("fixture-js");
        fs::create_dir_all(fixture.join("locales")).unwrap();

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "demo-js",
                "name": "plugin.title",
                "version": "1.0.0",
                "description": "Default export JS fixture",
                "main": "index.js",
                "configSchema": [
                    { "key": "token", "label": "settings.token", "type": "text" }
                ],
                "requestHeaders": [
                    {
                        "match": "*.cdn.example.com",
                        "kinds": ["image", "audio"],
                        "headers": {
                            "Referer": "https://music.example.com/",
                            "X-Auth": "Token {{config.token}}"
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        fs::write(
            fixture.join("locales/zh-CN.json"),
            r#"{"plugin":{"title":"示例插件"},"settings":{"token":"访问令牌"},"greeting":"你好 {name}"}"#,
        )
        .unwrap();
        fs::write(
            fixture.join("locales/en.json"),
            r#"{"plugin":{"title":"Demo Plugin"},"settings":{"token":"Access Token"},"greeting":"Hello {name}"}"#,
        )
        .unwrap();

        fs::write(
            fixture.join("index.js"),
            r#"
            export default {
                async getHome() {
                    return {
                        title: host.i18n.t("greeting", { name: "NSH" }),
                        sections: [
                            {
                                title: "Featured",
                                tracks: [
                                    { id: "t1", name: "Midnight Vinyl", duration: 215000 }
                                ]
                            }
                        ]
                    };
                },
                async search({ query, kind, offset }) {
                    return {
                        tracks: [
                            { id: "s1", name: `Result:${query}:${kind}:${offset}`, duration: 180000 }
                        ],
                        more: false
                    };
                },
                async getStreamUrl(id, quality) {
                    return {
                        url: `https://edge.cdn.example.com/audio/${id}.flac`,
                        type: "flac",
                        quality
                    };
                },
                async getLyrics(id) {
                    return `[00:00.00]Lyrics for ${id}`;
                },
                async onConfigChange(key, value) {
                    host.storage.set("lastChanged", `${key}=${value}`);
                }
            };
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let src = mgr.install_from_dir(&fixture).expect("install JS plugin");

        assert_eq!(src.info().name, "示例插件");
        mgr.set_locale("en");
        assert_eq!(src.info().name, "Demo Plugin");
        assert!(src.can("home"));
        assert!(src.can("search"));
        assert!(src.can("stream"));
        assert!(!src.can("remotePlay"));

        let home = src.home().expect("home");
        assert_eq!(home.title, "Hello NSH");
        assert_eq!(home.sections[0].tracks[0].source, "demo-js");

        let sr = src
            .search("city pop", SearchKind::Tracks, 10)
            .expect("search");
        assert_eq!(sr.tracks[0].name, "Result:city pop:tracks:10");

        src.set_setting("token", "xyz-999").expect("set setting");
        assert_eq!(src.setting("token"), "xyz-999");

        let stream = src.stream("t1", Quality::Lossless).expect("stream");
        assert_eq!(stream.url, "https://edge.cdn.example.com/audio/t1.flac");
        assert_eq!(
            stream.headers.get("Referer").map(String::as_str),
            Some("https://music.example.com/")
        );
        assert_eq!(
            stream.headers.get("X-Auth").map(String::as_str),
            Some("Token xyz-999")
        );

        let img_headers =
            src.request_headers_for("https://img.cdn.example.com/a.jpg", RequestKind::Image);
        assert!(img_headers.contains(&("X-Auth".to_owned(), "Token xyz-999".to_owned())));

        let lyr = src.lyrics("t1").expect("lyrics");
        assert_eq!(lyr.lrc, "[00:00.00]Lyrics for t1");

        // Reload & verify persistence.
        let reloaded = mgr.reload("demo-js").expect("reload");
        assert_eq!(reloaded.setting("token"), "xyz-999");

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_ts_plugin_with_crypto_storage_and_virtual_ffmpeg_relay() {
        let root = temp_workspace("ts-ffmpeg");
        let fixture = root.join("fixture-ts");
        fs::create_dir_all(&fixture).unwrap();

        // Start a tiny local HTTP upstream server serving dummy MP3 bytes for VirtualFfmpegShim relay.
        let upstream_listener = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        let upstream_payload = b"ID3\x04\x00\x00\x00\x00\x00\x00VIRTUAL_FFMPEG_RELAY_OK".to_vec();
        let payload_for_server = upstream_payload.clone();
        let upstream_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = upstream_listener.accept() {
                let mut req_buf = [0u8; 2048];
                let _ = stream.read(&mut req_buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload_for_server.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(&payload_for_server);
            }
        });

        // Pick a free loopback port for the ffmpeg `-listen 1` relay.
        let relay_port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().unwrap().port()
        };

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "ts-ffmpeg-demo",
                "name": "TS Virtual FFmpeg Demo",
                "version": "2.0.0",
                "entry": "index.ts",
                "permissions": {
                    "network": ["127.0.0.1"],
                    "process": ["ffmpeg"]
                }
            }"#,
        )
        .unwrap();

        fs::write(
            fixture.join("helper.ts"),
            r#"
            export interface DigestBundle {
                md5: string;
                sha256: string;
                hmac: string;
                decrypted: string;
                b64: string;
            }
            export function computeDigests(input: string, key: string): DigestBundle {
                const enc = host.crypto.aesEcbEncrypt(input, key, "base64");
                const decrypted = host.crypto.aesEcbDecrypt(enc, key, "base64");
                return {
                    md5: host.crypto.md5(input),
                    sha256: host.crypto.sha256(input),
                    hmac: host.crypto.hmacSha256(key, input),
                    decrypted,
                    b64: host.crypto.base64Decode(host.crypto.base64Encode(input)),
                };
            }
            "#,
        )
        .unwrap();

        let ts_code = format!(
            r#"
            import {{ computeDigests, type DigestBundle }} from "./helper";

            let activeRelay: any = null;

            export async function getTrack(id: string) {{
                const digests: DigestBundle = computeDigests("hello world", "0123456789abcdef");
                host.storage.set("savedDigest", digests);
                const loaded = host.storage.get("savedDigest");
                const keys = host.storage.keys();

                const verProc = host.spawn("ffmpeg", ["-version"]);
                const verCode = await verProc.wait();

                return {{
                    id,
                    name: `${{loaded.md5}}|${{loaded.decrypted}}|${{keys.join(",")}}|${{verCode === 0 && verProc.stdout().includes("ffmpeg")}}`,
                    duration: 123000,
                }};
            }}

            export async function getStreamUrl(_id: string, quality: string) {{
                const listenUrl = "http://127.0.0.1:{relay_port}/";
                activeRelay = host.spawn("ffmpeg", [
                    "-headers", "Referer: https://www.bilibili.com/\r\n",
                    "-user_agent", "nsh-music-test/1.0",
                    "-reconnect", "1",
                    "-i", "http://127.0.0.1:{upstream_port}/audio.mp3",
                    "-vn",
                    "-f", "mp3",
                    "-listen", "1",
                    listenUrl,
                ]);
                for (let i = 0; i < 40; i++) {{
                    if (activeRelay.errorOutput().includes("Input #0")) {{
                        break;
                    }}
                    await host.sleep(15);
                }}
                if (!activeRelay.errorOutput().includes("Input #0")) {{
                    throw new Error("virtual ffmpeg never emitted Input #0: " + activeRelay.errorOutput());
                }}
                return {{
                    url: listenUrl,
                    type: "mp3",
                    quality,
                }};
            }}

            export async function signOut() {{
                if (activeRelay) {{
                    activeRelay.kill();
                    activeRelay = null;
                }}
            }}
            "#
        );
        fs::write(fixture.join("index.ts"), ts_code).unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let src = mgr.install_from_dir(&fixture).expect("install TS plugin");

        // Static compat analyzer must recognize virtualized ffmpeg as Full / VirtualFfmpeg.
        assert_eq!(src.compat_report().tier, CompatTier::Full);
        assert_eq!(
            src.compatibility_info().level,
            PluginCompatLevel::VirtualFfmpeg
        );

        let track = src
            .get_track("tr-1")
            .expect("getTrack runs TS + crypto + storage + ffmpeg -version");
        assert!(
            track
                .name
                .starts_with("5eb63bbbe01eeed093cb22bb8f5acdc3|hello world|savedDigest|true"),
            "unexpected track name: {}",
            track.name
        );

        let stream = src
            .stream("tr-1", Quality::High)
            .expect("virtual ffmpeg loopback relay");
        assert_eq!(stream.url, format!("http://127.0.0.1:{relay_port}/"));

        // Fetch audio directly from the virtual ffmpeg loopback server!
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let relayed = rt.block_on(async {
            let client = reqwest::Client::new();
            let resp = client
                .get(&stream.url)
                .send()
                .await
                .expect("GET loopback relay");
            assert!(resp.status().is_success(), "status={}", resp.status());
            resp.bytes().await.expect("relay bytes").to_vec()
        });
        assert_eq!(relayed, upstream_payload);

        src.sign_out().expect("kill active relay");
        let _ = upstream_thread.join();
        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mobile_sandbox_allows_virtual_ffmpeg_and_blocks_external_cli() {
        let root = temp_workspace("mobile-sandbox");
        let fixture = root.join("fixture-mobile");
        fs::create_dir_all(&fixture).unwrap();

        fs::write(
            fixture.join("plugin.json"),
            r#"{"id":"mobile-check","name":"Mobile Check","main":"index.ts"}"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.ts"),
            r#"
            export async function getTrack(_id: string) {
                const ff = host.spawn("ffmpeg", ["-version"]);
                const ffCode = await ff.wait();
                const yt = host.spawn("yt-dlp", ["--version"]);
                const ytLine = await yt.readLine();
                const ytCode = await yt.wait();
                return {
                    id: "ok",
                    name: `mobile=${host.capabilities.isMobile}|canSpawn=${host.capabilities.canSpawnExternal}|ff=${ffCode === 0 && ff.stdout().includes("ffmpeg")}|yt=${ytCode === 0 && Boolean(ytLine && ytLine.includes("nsh-virtual-ytdlp"))}|ey=${host.capabilities.embeddedYtdlp}|vp=${host.capabilities.virtualProcesses.join(",")}|open=${host.capabilities.openUrl}`,
                    duration: 1000
                };
            }
            export async function search(_opts: any) {
                host.spawn("python3", ["--version"]);
                return { tracks: [] };
            }
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data"))
            .unwrap()
            .with_mobile_override(true);
        let src = mgr
            .install_from_dir(&fixture)
            .expect("load plugin in mobile mode");

        assert_eq!(src.compat_report().tier, CompatTier::MigrationRequired);
        assert_eq!(
            src.compatibility_info().level,
            PluginCompatLevel::DesktopProcessLimited
        );

        let track = src
            .get_track("1")
            .expect("ffmpeg and yt-dlp work even on mobile");
        assert_eq!(
            track.name,
            "mobile=true|canSpawn=false|ff=true|yt=true|ey=true|vp=ffmpeg,yt-dlp|open=true"
        );

        let err = src
            .search("test", SearchKind::Tracks, 0)
            .expect_err("unvirtualized binary must fail on mobile");
        let msg = err.to_string();
        assert!(
            msg.contains("MobileSandboxError") && msg.contains("python3"),
            "unexpected error: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase().contains("allowed"),
            "MobileSandboxError must never contain 'allowed': {msg}"
        );

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn plugin_manager_zip_install_enable_disable_and_zip_slip() {
        let root = temp_workspace("manager-zip");
        let zip_path = root.join("sample.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("sample/plugin.json", opts).unwrap();
            zw.write_all(
                br#"{"id":"sample-zip","name":"Sample Zip","version":"0.2.0","main":"index.js"}"#,
            )
            .unwrap();
            zw.start_file("sample/index.js", opts).unwrap();
            zw.write_all(b"export async function suggest({ query }) { return [query + '!']; }")
                .unwrap();
            zw.finish().unwrap();
        }

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let src = mgr.install_from_zip(&zip_path).expect("install zip");
        assert_eq!(src.suggest("hi").unwrap(), vec!["hi!"]);
        assert_eq!(mgr.sources().len(), 1);
        assert_eq!(mgr.plugins().len(), 1);

        mgr.set_enabled("sample-zip", false).expect("disable");
        assert!(mgr.sources().is_empty());
        assert!(!mgr.plugins()[0].enabled);

        mgr.set_enabled("sample-zip", true).expect("re-enable");
        assert_eq!(mgr.sources().len(), 1);

        // Verify malicious zip-slip archive fails cleanly.
        let evil_zip = root.join("evil.zip");
        {
            let mut cursor = Cursor::new(Vec::new());
            {
                let mut zw = ZipWriter::new(&mut cursor);
                let opts = SimpleFileOptions::default();
                zw.start_file("../../outside.txt", opts).unwrap();
                zw.write_all(b"nope").unwrap();
                zw.finish().unwrap();
            }
            fs::write(&evil_zip, cursor.into_inner()).unwrap();
        }
        assert!(mgr.install_from_zip(&evil_zip).is_err());

        mgr.uninstall("sample-zip").expect("uninstall");
        assert!(mgr.plugins().is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn staged_install_inspects_permissions_and_enforces_program_grant() {
        let root = temp_workspace("staged-grant");
        let fixture = root.join("fixture-proc");
        fs::create_dir_all(&fixture).unwrap();
        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "proc-staged",
                "name": "Staged Program Plugin",
                "version": "1.2.3",
                "main": "index.js",
                "permissions": {
                    "network": ["api.example.test"],
                    "process": ["echo"]
                }
            }"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            r#"
            export async function search({ query }) {
                const p = host.spawn("echo", [query]);
                const code = await p.wait();
                if (code !== 0) throw new Error("exit " + code);
                return { tracks: [{ id: "1", name: p.stdout().trim() }] };
            }
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let pending = mgr.stage_from_dir(&fixture).expect("stage plugin dir");
        assert_eq!(pending.id, "proc-staged");
        assert_eq!(pending.version, "1.2.3");
        assert!(
            pending
                .network_hosts
                .contains(&"api.example.test".to_owned())
        );
        assert!(pending.process_commands.contains(&"echo".to_owned()));

        // Install without granting external program execution -> search fails cleanly.
        let src = mgr
            .confirm_staged(&pending, false)
            .expect("confirm install ungranted");
        assert!(!src.compatibility_info().granted);
        assert!(src.search("hi", SearchKind::Tracks, 0).is_err());

        // Grant permission via manager -> succeeds on desktop.
        mgr.set_granted("proc-staged", true).expect("set granted");
        assert!(src.compatibility_info().granted);
        let res = src
            .search("hi", SearchKind::Tracks, 0)
            .expect("search after grant");
        assert_eq!(res.tracks[0].name, "hi");

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn host_fetch_web_sign_in_and_qr_sign_in_work_end_to_end() {
        let root = temp_workspace("fetch-auth");
        let fixture = root.join("fixture-auth");
        fs::create_dir_all(&fixture).unwrap();

        // Tiny local JSON API server verifying requestHeaders injection on `host.fetch`.
        let api_listener = TcpListener::bind("127.0.0.1:0").expect("bind api");
        let api_port = api_listener.local_addr().unwrap().port();
        let api_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = api_listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let has_referer = req
                    .to_ascii_lowercase()
                    .contains("referer: https://auth.example.com/");
                let body = format!(r#"{{"ok":true,"hasReferer":{has_referer}}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "auth-demo",
                "name": "Auth Demo",
                "main": "index.ts",
                "permissions": {
                    "network": ["127.0.0.1", "auth.example.com"]
                },
                "requestHeaders": [
                    {
                        "match": "127.0.0.1",
                        "kinds": ["api"],
                        "headers": {
                            "Referer": "https://auth.example.com/"
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        fs::write(
            fixture.join("index.ts"),
            format!(
                r#"
                export async function signIn() {{
                    const web = await host.webSignIn({{
                        url: "https://auth.example.com/login",
                        title: "Sign In",
                        incognito: true,
                        cookieNames: ["SESSION"],
                        allowManualCookie: true,
                    }});
                    const res = await host.fetch("http://127.0.0.1:{api_port}/me");
                    const data = await res.json();
                    return {{
                        id: web.cookies.SESSION || "none",
                        name: `ref=${{data.hasReferer}}|cookie=${{web.cookie}}`,
                    }};
                }}

                export async function getQrSignIn() {{
                    return {{
                        token: "qr-tok-1",
                        qrUrl: "https://auth.example.com/qr/qr-tok-1",
                        message: "Scan with app",
                        expiresIn: 120,
                    }};
                }}

                export async function pollQrSignIn(token: string) {{
                    return {{
                        status: "confirmed",
                        message: token,
                        account: {{ id: "u42", name: "QR User" }},
                    }};
                }}
                "#
            ),
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        mgr.set_web_sign_in_handler(Some(Arc::new(|req| {
            assert_eq!(req.url, "https://auth.example.com/login");
            assert!(req.incognito);
            assert!(req.allow_manual_cookie);
            Ok(WebSignInResult {
                ok: true,
                url: req.url,
                cookies: HashMap::from([("SESSION".to_owned(), "sess-42".to_owned())]),
                cookie: "SESSION=sess-42".to_owned(),
                eval_result: None,
                cancelled: false,
                manual_fallback: false,
            })
        })));

        let src = mgr.install_from_dir(&fixture).expect("install auth plugin");
        let acc = src.sign_in().expect("webSignIn + host.fetch");
        assert_eq!(acc.id, "sess-42");
        assert_eq!(acc.name, "ref=true|cookie=SESSION=sess-42");

        let qr = src.get_qr_sign_in().expect("qr session");
        assert_eq!(qr.token, "qr-tok-1");
        assert!(qr.qr_svg.contains("<svg"), "expected auto-rendered QR SVG");

        let poll = src.poll_qr_sign_in(&qr.token).expect("poll qr");
        assert_eq!(poll.status, "confirmed");
        assert_eq!(poll.account.unwrap().id, "u42");

        let _ = api_thread.join();
        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mygo_music_canonical_contract_and_web_polyfills() {
        let root = temp_workspace("mygo-canonical");
        let fixture = root.join("fixture-mygo");
        fs::create_dir_all(fixture.join("locales")).unwrap();

        let server = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let port = server.local_addr().unwrap().port();
        let server_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((mut stream, _)) = server.accept() {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.contains("/slow") {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    let body = r#"{"ok":true}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: a=1; Path=/\r\nSet-Cookie: b=2; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "mygo-canonical",
                "name": "MyGo Canonical",
                "minAppVersion": "0.1.21",
                "main": "index.js",
                "permissions": {
                    "network": ["127.0.0.1"],
                    "process": ["ffmpeg", "/opt/homebrew/bin/ffmpeg"]
                },
                "settings": [
                    { "key": "savedOrder", "label": "Order", "type": "text" }
                ]
            }"#,
        )
        .unwrap();
        fs::write(
            fixture.join("locales/zh-CN.json"),
            r#"{"Greeting": "你好"}"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            format!(
                r#"
                const t = host.t;
                export async function browse({{ page }}) {{
                    const u = new URL("https://cdn.example.com:8443/path/to/file?x=1&y=2#frag");
                    const cloned = structuredClone({{ page, host: u.host, hostname: u.hostname, x: u.searchParams.get("x") }});
                    const res = await fetch("http://127.0.0.1:{port}/cookies");
                    const setCookies = res.headers.getSetCookie();

                    // Test AbortController aborts slow fetch
                    const ac = new AbortController();
                    setTimeout(() => ac.abort(), 30);
                    let abortedName = "none";
                    try {{
                        await fetch("http://127.0.0.1:{port}/slow", {{ signal: ac.signal }});
                    }} catch (e) {{
                        abortedName = e.name || "err";
                    }}

                    // Test long guard timer + clearTimeout doesn't stall runtime shutdown
                    const guardTimer = setTimeout(() => {{}}, 900000);
                    clearTimeout(guardTimer);

                    // Test virtual ffmpeg exit code integer 0 and closeStdin/readLine
                    const proc = host.spawn("/opt/homebrew/bin/ffmpeg", ["-version"]);
                    proc.closeStdin();
                    const firstLine = await proc.readLine();
                    const exitCode = await proc.wait();

                    return {{
                        title: `${{t("Greeting")}}|${{host.setting("savedOrder")}}|${{host.language()}}|${{typeof host.webSignIn}}|${{cloned.hostname}}|${{cloned.x}}|${{setCookies.length}}|${{abortedName}}|${{exitCode === 0}}|${{Boolean(firstLine && firstLine.includes("ffmpeg"))}}`,
                        sections: []
                    }};
                }}
                export async function search({{ query, kind, offset }}) {{
                    return {{
                        tracks: [{{ id: "t1", name: `${{query}}:${{kind}}:${{offset}}`, duration: 1000 }}],
                        more: false
                    }};
                }}
                async function getAlbum({{ id }}) {{
                    return {{ album: {{ id, name: "Album-" + id }}, tracks: [] }};
                }}
                async function getArtist({{ id }}) {{
                    return {{ artist: {{ id, name: "Artist-" + id }}, tracks: [], albums: [] }};
                }}
                export {{ getAlbum as album, getArtist as artist }};
                "#
            ),
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let src = mgr
            .install_from_dir(&fixture)
            .expect("install canonical plugin");
        src.set_setting("savedOrder", "fav1, fav2").unwrap();

        assert_eq!(src.compat_report().tier, CompatTier::Full);
        assert_eq!(
            src.compatibility_info().level,
            PluginCompatLevel::VirtualFfmpeg
        );
        assert!(src.can("album"));
        assert!(src.can("artist"));

        let b = src.browse("charts").expect("browse");
        assert_eq!(
            b.title,
            "你好|fav1, fav2|zh-CN|undefined|cdn.example.com|1|2|AbortError|true|true"
        );

        let sr = src.search("bili", SearchKind::Tracks, 20).expect("search");
        assert_eq!(sr.tracks[0].name, "bili:tracks:20");

        let alb = src.album("BV1").expect("aliased album export");
        assert_eq!(alb.album.name, "Album-BV1");
        let art = src.artist("UP9").expect("aliased artist export");
        assert_eq!(art.artist.name, "Artist-UP9");

        let shut_start = std::time::Instant::now();
        mgr.shutdown_all();
        assert!(
            shut_start.elapsed() < std::time::Duration::from_secs(2),
            "shutdown stalled on cancelled timer"
        );
        let _ = server_thread.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "live network check against github.com and api.bilibili.com"]
    fn live_install_and_query_bilibili_plugin() {
        let root = temp_workspace("live-bilibili");
        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let pending = mgr
            .stage_from_github("https://github.com/GentleM4dman/mygo-music-bilibili")
            .expect("stage bilibili from GitHub");
        assert_eq!(pending.id, "bilibili");
        assert!(
            !pending
                .process_commands
                .iter()
                .any(|c| c.contains("dynamic"))
        );

        let src = mgr
            .confirm_staged(&pending, true)
            .expect("confirm and load bilibili");
        assert_eq!(
            src.compatibility_info().level,
            PluginCompatLevel::VirtualFfmpeg
        );
        assert!(src.can("home"));
        assert!(src.can("browse"));
        assert!(src.can("search"));
        assert!(src.can("suggest"));
        assert!(src.can("album"));
        assert!(src.can("artist"));
        assert!(src.can("stream"));
        assert!(src.info().navigation.iter().any(|n| n.page == "charts"));

        let home = src.home().expect("bilibili home()");
        assert!(
            !home.sections.is_empty(),
            "expected non-empty home sections"
        );

        let charts = src.browse("charts").expect("bilibili browse(charts)");
        assert!(
            !charts.sections.is_empty(),
            "expected non-empty charts sections"
        );

        let suggestions = src.suggest("周杰伦").expect("bilibili suggest");
        assert!(!suggestions.is_empty(), "expected non-empty suggestions");

        let search_res = src
            .search("千本樱", SearchKind::Tracks, 0)
            .expect("bilibili search");
        assert!(!search_res.tracks.is_empty(), "expected search tracks");
        let first = &search_res.tracks[0];
        assert_eq!(first.source, "bilibili");

        let stream = src
            .stream(&first.id, Quality::High)
            .expect("bilibili stream");
        assert!(!stream.url.is_empty(), "expected non-empty stream url");

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn web_sign_in_parses_youtube_music_fields_and_supports_late_handler() {
        let root = temp_workspace("web-signin-ytm");
        let fixture = root.join("fixture-ytm-auth");
        fs::create_dir_all(&fixture).unwrap();

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "ytm-auth-test",
                "name": "YTM Auth Test",
                "main": "index.js",
                "permissions": { "network": ["music.youtube.com"] }
            }"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            r#"
            export async function suggest() {
                return [typeof host.webSignIn];
            }
            export async function signIn() {
                const res = await host.webSignIn({
                    url: "https://accounts.google.com/ServiceLogin?service=youtube",
                    cookiesOf: "https://music.youtube.com",
                    until: ["SAPISID", "__Secure-3PSID"],
                    userAgent: "Mozilla/5.0 TestSafari",
                    title: "YouTube Music",
                });
                return {
                    id: res.cookies.SAPISID || "missing",
                    name: `${res.ok}|${res.cancelled}|${res.manualFallback}|${res.cookie}`,
                };
            }
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        // Install BEFORE setting WebSignInHandler -> `host.webSignIn` must initially be `undefined`.
        let src = mgr.install_from_dir(&fixture).expect("install plugin");
        assert_eq!(src.suggest("").unwrap(), vec!["undefined"]);

        // Now register the WebSignInHandler dynamically -> `host.webSignIn` becomes callable immediately.
        let captured = Arc::new(std::sync::Mutex::new(None::<WebSignInRequest>));
        let captured_clone = Arc::clone(&captured);
        mgr.set_web_sign_in_handler(Some(Arc::new(move |req| {
            *captured_clone.lock().unwrap() = Some(req.clone());
            Ok(WebSignInResult {
                ok: true,
                url: "https://music.youtube.com/".to_owned(),
                cookies: HashMap::from([
                    ("SAPISID".to_owned(), "sap-123".to_owned()),
                    ("__Secure-3PSID".to_owned(), "sid-456".to_owned()),
                ]),
                cookie: String::new(), // verify auto-synthesis from `cookies` map
                eval_result: None,
                cancelled: false,
                manual_fallback: false,
            })
        })));

        assert_eq!(src.suggest("").unwrap(), vec!["function"]);
        let acc = src.sign_in().expect("signIn via late-registered handler");
        assert_eq!(acc.id, "sap-123");
        assert_eq!(
            acc.name,
            "true|false|false|SAPISID=sap-123; __Secure-3PSID=sid-456"
        );

        let req = captured
            .lock()
            .unwrap()
            .clone()
            .expect("captured WebSignInRequest");
        assert_eq!(req.plugin_id, "ytm-auth-test");
        assert_eq!(
            req.url,
            "https://accounts.google.com/ServiceLogin?service=youtube"
        );
        assert_eq!(req.cookies_of.as_deref(), Some("https://music.youtube.com"));
        assert_eq!(
            req.until,
            vec!["SAPISID".to_owned(), "__Secure-3PSID".to_owned()]
        );
        assert_eq!(
            req.cookie_names,
            vec!["SAPISID".to_owned(), "__Secure-3PSID".to_owned()]
        );
        assert_eq!(req.user_agent.as_deref(), Some("Mozilla/5.0 TestSafari"));
        assert_eq!(req.title, "YouTube Music");

        // Clearing handler hides `host.webSignIn` again.
        mgr.set_web_sign_in_handler(None);
        assert_eq!(src.suggest("").unwrap(), vec!["undefined"]);

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_ungranted_matches_allowed_and_missing_binary_is_enoent() {
        let root = temp_workspace("spawn-perm-ytm");
        let fixture = root.join("fixture-perm");
        fs::create_dir_all(&fixture).unwrap();

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "ytm-perm-test",
                "name": "YTM Perm Test",
                "main": "index.js",
                "permissions": { "process": ["ffmpeg", "yt-dlp", "python3", "__nsh_missing_js_runtime_xyz__"] }
            }"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            r#"
            function fail(code, message) {
                const e = new Error(message);
                e.code = code;
                return e;
            }
            async function probe(cmd) {
                try {
                    const p = host.spawn(cmd, ["--version"]);
                    p.closeStdin();
                    while ((await p.readLine()) !== null) {}
                    return (await p.wait()) === 0 ? "ok" : "nonzero";
                } catch (e) {
                    if (/allowed/.test(e.message)) {
                        throw fail("setup", "Allow programs in plugin settings");
                    }
                    return "missing:" + e.message;
                }
            }
            export async function suggest({ query }) {
                return [await probe(query)];
            }
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        let pending = mgr.stage_from_dir(&fixture).expect("stage");
        let src = mgr
            .confirm_staged(&pending, false)
            .expect("confirm ungranted");

        // Unvirtualized external binary when `!granted` MUST fail with `/allowed/` -> structured `SourceError::Setup`.
        let err = src
            .suggest("python3")
            .expect_err("ungranted unvirtualized spawn must fail");
        assert!(
            matches!(err, nsh_music_core::model::SourceError::Setup(_)),
            "expected SourceError::Setup, got {err:?}"
        );

        // Virtualized `ffmpeg` and `yt-dlp` still work in-process even when `!granted`.
        let ff_res = src
            .suggest("ffmpeg")
            .expect("ffmpeg falls back to VirtualFfmpegShim when ungranted");
        assert_eq!(ff_res, vec!["ok"]);
        let ytdlp_res = src
            .suggest("yt-dlp")
            .expect("yt-dlp falls back to VirtualYtdlpShim when ungranted");
        assert_eq!(ytdlp_res, vec!["ok"]);

        // Once granted, missing binaries (like uninstalled `deno`/`node`/`bun`) return ENOENT without `allowed`.
        mgr.set_granted("ytm-perm-test", true).expect("grant");
        let missing = src
            .suggest("__nsh_missing_js_runtime_xyz__")
            .expect("missing binary should not trigger setup/allowed error");
        assert!(
            missing[0].starts_with("missing:") && !missing[0].contains("allowed"),
            "unexpected probe output: {:?}",
            missing
        );

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn desktop_ffmpeg_transcoding_heuristic_and_non_deadlocking_kill() {
        use crate::mobile_compat::ffmpeg_requests_transcoding;

        // Audio transcoding flags (`-c:a libmp3lame -b:a 192k`) -> real ffmpeg preferred on desktop.
        let ytm_args: Vec<String> = [
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-i",
            "https://media.example.com/audio/stream?id=1",
            "-vn",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
            "-f",
            "mp3",
            "-listen",
            "1",
            "http://127.0.0.1:25000/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(ffmpeg_requests_transcoding(&ytm_args));

        // Stream copy / remux (`-c:a copy`) -> VirtualFfmpegShim.
        let copy_args: Vec<String> = [
            "-i",
            "https://cdn.example.com/audio.m4s",
            "-vn",
            "-c:a",
            "copy",
            "-f",
            "mp4",
            "-listen",
            "1",
            "http://127.0.0.1:25001/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(!ffmpeg_requests_transcoding(&copy_args));

        // Direct MP3 relay with `.mp3` input and no codec/bitrate flag -> VirtualFfmpegShim.
        let mp3_relay_args: Vec<String> = [
            "-i",
            "http://127.0.0.1:9999/audio.mp3",
            "-vn",
            "-f",
            "mp3",
            "-listen",
            "1",
            "http://127.0.0.1:25002/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(!ffmpeg_requests_transcoding(&mp3_relay_args));

        // Verify `ExternalProcessHandle::kill()` never deadlocks while `proc.wait()` is pending.
        #[cfg(unix)]
        {
            let root = temp_workspace("kill-while-wait");
            let fixture = root.join("fixture-kill");
            fs::create_dir_all(&fixture).unwrap();
            fs::write(
                fixture.join("plugin.json"),
                r#"{"id":"kill-test","name":"Kill Test","main":"index.js","permissions":{"process":["sleep"]}}"#,
            )
            .unwrap();
            fs::write(
                fixture.join("index.js"),
                r#"
                export async function suggest() {
                    const p = host.spawn("sleep", ["30"]);
                    const waitPromise = p.wait();
                    setTimeout(() => p.kill(), 40);
                    const code = await waitPromise;
                    return [String(code), String(p.isRunning())];
                }
                "#,
            )
            .unwrap();

            let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
            let src = mgr.install_from_dir(&fixture).expect("install kill-test");
            let start = std::time::Instant::now();
            let res = src.suggest("").expect("kill while wait must resolve");
            assert!(
                start.elapsed() < std::time::Duration::from_secs(3),
                "kill deadlocked while wait() was pending"
            );
            assert_eq!(res[1], "false");
            mgr.shutdown_all();
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    #[ignore = "live network check against github.com and music.youtube.com"]
    fn live_install_and_query_youtube_music_plugin() {
        let root = temp_workspace("live-youtube-music");
        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();

        let pending = mgr
            .stage_from_github("egoist/mygo-music-youtube-music")
            .expect("stage youtube-music from GitHub");
        assert_eq!(pending.id, "youtube-music");
        assert!(
            pending
                .network_hosts
                .iter()
                .any(|h| h == "music.youtube.com"),
            "expected music.youtube.com in network_hosts: {:?}",
            pending.network_hosts
        );
        assert!(
            !pending
                .process_commands
                .iter()
                .any(|c| c.contains("dynamic")),
            "unexpected dynamic placeholder in process_commands: {:?}",
            pending.process_commands
        );

        let src = mgr
            .confirm_staged(&pending, true)
            .expect("confirm and load youtube-music");

        // Verify localized navigation in zh-CN and English phrase preservation in en.
        let zh_info = src.info();
        assert!(
            zh_info
                .navigation
                .iter()
                .any(|n| n.page == "charts" && n.title == "排行榜"),
            "expected zh-CN Charts nav item, got {:?}",
            zh_info.navigation
        );
        assert!(
            zh_info
                .navigation
                .iter()
                .any(|n| n.page == "new" && n.title == "新发行"),
            "expected zh-CN New Releases nav item, got {:?}",
            zh_info.navigation
        );

        mgr.set_locale("en");
        let en_info = src.info();
        assert!(
            en_info
                .navigation
                .iter()
                .any(|n| n.page == "charts" && n.title == "Charts"),
            "expected en Charts nav item, got {:?}",
            en_info.navigation
        );
        assert!(
            en_info
                .navigation
                .iter()
                .any(|n| n.page == "new" && n.title == "New Releases"),
            "expected en New Releases nav item, got {:?}",
            en_info.navigation
        );
        mgr.set_locale("zh-CN");

        // Verify capabilities.
        for cap in [
            "home",
            "browse",
            "search",
            "suggest",
            "album",
            "artist",
            "playlist",
            "stations",
            "stationNext",
            "lyrics",
            "stream",
            "account",
            "signIn",
            "signOut",
        ] {
            assert!(src.can(cap), "expected youtube-music capability `{cap}`");
        }

        let home = src.home().expect("youtube-music home()");
        assert!(
            !home.sections.is_empty(),
            "expected non-empty home sections"
        );

        let charts = src.browse("charts").expect("youtube-music browse(charts)");
        assert!(
            !charts.sections.is_empty(),
            "expected non-empty charts sections"
        );

        let new_rel = src.browse("new").expect("youtube-music browse(new)");
        assert!(
            !new_rel.sections.is_empty(),
            "expected non-empty new releases sections"
        );

        let search_res = src
            .search("周杰伦", SearchKind::Tracks, 0)
            .expect("youtube-music search");
        assert!(!search_res.tracks.is_empty(), "expected search tracks");
        let first = &search_res.tracks[0];
        assert_eq!(first.source, "youtube-music");
        assert!(!first.id.is_empty());

        let suggestions = src.suggest("周杰伦").expect("youtube-music suggest");
        assert!(!suggestions.is_empty(), "expected non-empty suggestions");

        let stations = src.stations().expect("youtube-music stations");
        assert!(!stations.is_empty(), "expected non-empty stations");

        // Exercise album, artist, playlist, and stationNext deserialization against live YouTube Music data.
        if let Some(alb_id) = home
            .sections
            .iter()
            .flat_map(|s| s.albums.iter())
            .map(|a| a.id.as_str())
            .find(|id| !id.is_empty())
        {
            let alb_page = src.album(alb_id).expect("youtube-music album()");
            assert!(!alb_page.album.name.is_empty());
        }
        if let Some(art_id) = first
            .artists
            .iter()
            .map(|a| a.id.as_str())
            .find(|id| !id.is_empty())
        {
            let art_page = src.artist(art_id).expect("youtube-music artist()");
            assert!(!art_page.artist.name.is_empty());
        }
        if let Some(pl_id) = home
            .sections
            .iter()
            .flat_map(|s| s.playlists.iter())
            .map(|p| p.id.as_str())
            .find(|id| !id.is_empty() && !id.starts_with("RD"))
        {
            let pl_page = src.playlist(pl_id).expect("youtube-music playlist()");
            assert!(!pl_page.name.is_empty());
        }
        src.played(first);
        let radio_tracks = src
            .station_next("radio")
            .expect("youtube-music station_next()");
        assert!(
            !radio_tracks.is_empty(),
            "expected radio tracks from station_next"
        );

        let _lyrics = src.lyrics(&first.id).expect("youtube-music lyrics");

        let stream = src
            .stream(&first.id, Quality::High)
            .expect("youtube-music stream");
        assert!(
            stream.url.starts_with("http://127.0.0.1:"),
            "expected loopback relay URL, got {}",
            stream.url
        );
        assert_eq!(stream.media_type, "mp3");
        assert_eq!(stream.bit_rate, 192_000);

        // Verify actual MP3 audio bytes stream from the loopback ffmpeg relay.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first_chunk = rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap();
            let mut resp = client
                .get(&stream.url)
                .send()
                .await
                .expect("GET youtube-music loopback stream");
            assert!(
                resp.status().is_success(),
                "stream status={}",
                resp.status()
            );
            resp.chunk()
                .await
                .expect("read first stream chunk")
                .expect("non-empty chunk")
                .to_vec()
        });
        assert!(
            first_chunk.len() >= 4,
            "expected audio bytes from stream, got {} bytes",
            first_chunk.len()
        );
        // When real ffmpeg transcodes with `-id3v2_version 0 -write_xing 0 -f mp3`,
        // frames begin with an MPEG frame sync (0xFF 0xE0..0xFF) or ID3 tag.
        let looks_mp3 = first_chunk.starts_with(b"ID3")
            || (first_chunk[0] == 0xFF && (first_chunk[1] & 0xE0) == 0xE0);
        assert!(
            looks_mp3,
            "expected MP3 audio header from desktop ffmpeg transcode, got {:02x?}",
            &first_chunk[..first_chunk.len().min(16)]
        );

        // Verify shutdown kills active ffmpeg relay without deadlocking or stalling.
        let shut_start = std::time::Instant::now();
        mgr.shutdown_all();
        assert!(
            shut_start.elapsed() < std::time::Duration::from_secs(3),
            "shutdown_all stalled on active ffmpeg relay"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enforces_network_and_process_permissions_and_settings_persistence() {
        let root = temp_workspace("net-proc-perms");
        let fixture = root.join("fixture-sec");
        fs::create_dir_all(&fixture).unwrap();

        let srv = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = srv.local_addr().unwrap().port();
        let srv_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((mut s, _)) = srv.accept() {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let resp =
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                    let _ = s.write_all(resp.as_bytes());
                }
            }
        });

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "sec-demo",
                "name": "Sec Demo",
                "main": "index.js",
                "permissions": {
                    "network": ["*.allowed.example"],
                    "process": ["echo"]
                }
            }"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            format!(
                r#"
                export async function suggest({{ query }}) {{
                    if (query === "blocked-host") {{
                        await fetch("https://evil.other.test/");
                        return ["unexpected"];
                    }}
                    if (query === "loopback") {{
                        const r = await fetch("http://127.0.0.1:{port}/ping");
                        return [await r.text()];
                    }}
                    if (query === "bad-signin-url") {{
                        await host.webSignIn({{ url: "http://insecure.example/login", cookiesOf: "https://api.allowed.example", until: ["SID"] }});
                        return ["unexpected"];
                    }}
                    if (query === "bad-cookies-of") {{
                        await host.webSignIn({{ url: "https://accounts.example/login", cookiesOf: "https://evil.other.test", until: ["SID"] }});
                        return ["unexpected"];
                    }}
                    if (query === "undeclared-proc") {{
                        host.spawn("uname", ["-a"]);
                        return ["unexpected"];
                    }}
                    return ["ok"];
                }}
                "#
            ),
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data")).unwrap();
        mgr.set_web_sign_in_handler(Some(Arc::new(|req| {
            Ok(WebSignInResult {
                ok: true,
                url: req.url,
                cookie: "SID=1".to_owned(),
                ..WebSignInResult::default()
            })
        })));
        let pending = mgr.stage_from_dir(&fixture).expect("stage");
        let src = mgr
            .confirm_staged(&pending, false)
            .expect("confirm ungranted");

        // Disallowed host rejected.
        let err = src.suggest("blocked-host").expect_err("disallowed host");
        assert!(err.to_string().contains("network permission"), "got {err}");

        // Loopback fetch denied while `!granted` (not explicitly in `permissions.network`).
        assert!(src.suggest("loopback").is_err());

        // `webSignIn` rejects non-https URL and `cookiesOf` outside `permissions.network`.
        let err_http = src.suggest("bad-signin-url").expect_err("http webSignIn");
        assert!(
            err_http.to_string().contains("not a secure web address"),
            "got {err_http}"
        );
        let err_co = src
            .suggest("bad-cookies-of")
            .expect_err("disallowed cookiesOf");
        assert!(
            err_co.to_string().contains("can't reach evil.other.test"),
            "got {err_co}"
        );

        // Granting Allow Programs writes `granted.json`, restarts runtime, and unlocks loopback fetch.
        mgr.set_granted("sec-demo", true).expect("grant");
        assert!(root.join("data/sec-demo/granted.json").is_file());
        assert_eq!(
            src.suggest("loopback").expect("loopback after grant"),
            vec!["ok"]
        );

        // Undeclared desktop binary rejected by `allows_process`.
        let err_proc = src.suggest("undeclared-proc").expect_err("undeclared proc");
        assert!(
            err_proc.to_string().contains("isn't one of the programs"),
            "got {err_proc}"
        );

        // Settings persist in `settings.json`.
        src.set_setting("k1", "v1").expect("set_setting");
        assert!(root.join("data/sec-demo/settings.json").is_file());
        let reloaded = mgr.reload("sec-demo").expect("reload");
        assert_eq!(reloaded.setting("k1"), "v1");
        assert!(reloaded.compatibility_info().granted);

        mgr.shutdown_all();
        // Wake loopback listener if second accept is still pending.
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        let _ = srv_thread.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn aes_ecb_two_arg_mygo_order_and_open_url_handler_and_timeout_recovery() {
        let root = temp_workspace("crypto-openurl-timeout");
        let fixture = root.join("fixture-misc");
        fs::create_dir_all(&fixture).unwrap();

        fs::write(
            fixture.join("plugin.json"),
            r#"{"id":"misc-demo","name":"Misc Demo","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            fixture.join("index.js"),
            r#"
            export async function suggest({ query }) {
                if (query === "aes") {
                    // mygo-music 2-arg convention: aesEcbEncrypt(key, text) -> lowercase hex
                    const hexOut = host.crypto.aesEcbEncrypt("e82ckenh8dichen8", "hello mygo-music");
                    // legacy 3-arg convention: aesEcbEncrypt(plaintext, key, "base64")
                    const b64Out = host.crypto.aesEcbEncrypt("hello mygo-music", "e82ckenh8dichen8", "base64");
                    const dec = host.crypto.aesEcbDecrypt(hexOut, "e82ckenh8dichen8", "hex");
                    return [hexOut, b64Out, dec];
                }
                if (query === "open-ok") {
                    host.openURL("https://music.example.com/auth");
                    return ["opened"];
                }
                if (query === "open-bad") {
                    host.openURL("file:///etc/passwd");
                    return ["bad"];
                }
                if (query === "hang") {
                    while (true) {}
                }
                return ["alive:" + query];
            }
            "#,
        )
        .unwrap();

        let opened_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let opened_clone = Arc::clone(&opened_urls);

        // Run in mobile mode to prove OpenUrlHandler works on mobile too.
        let mgr = PluginManager::new(root.join("plugins"), root.join("data"))
            .unwrap()
            .with_mobile_override(true);
        mgr.set_open_url_handler(Some(Arc::new(move |url| {
            opened_clone.lock().unwrap().push(url.to_owned());
            Ok(())
        })));

        let src = mgr.install_from_dir(&fixture).expect("install misc-demo");

        // 1. AES-ECB both 2-arg (mygo-music) and 3-arg (legacy) signatures match exact Rust crypto.
        let aes = src.suggest("aes").expect("aes");
        let expected_ct =
            crate::crypto::aes_ecb_encrypt(b"hello mygo-music", b"e82ckenh8dichen8").unwrap();
        assert_eq!(aes[0], crate::crypto::hex_encode(&expected_ct));
        assert_eq!(aes[1], crate::crypto::base64_encode(&expected_ct));
        assert_eq!(aes[2], "hello mygo-music");

        // 2. OpenUrlHandler works on mobile and rejects non-http(s) schemes.
        assert_eq!(src.suggest("open-ok").expect("openURL"), vec!["opened"]);
        assert_eq!(
            *opened_urls.lock().unwrap(),
            vec!["https://music.example.com/auth".to_owned()]
        );
        assert!(src.suggest("open-bad").is_err());

        // 3. Per-call timeout interrupts infinite loop & transparently restarts QuickJS worker.
        src.set_call_timeout_override(Some(std::time::Duration::from_millis(120)));
        let timeout_err = src
            .suggest("hang")
            .expect_err("infinite loop must time out");
        assert!(
            timeout_err.to_string().contains("in time"),
            "unexpected timeout err: {timeout_err}"
        );
        src.set_call_timeout_override(None);
        assert_eq!(
            src.suggest("recovered")
                .expect("worker must recover after timeout"),
            vec!["alive:recovered"]
        );

        mgr.shutdown_all();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mobile_virtual_ffmpeg_relays_audio_without_desktop_grant_and_probes_degrade_cleanly() {
        let root = temp_workspace("mobile-virtual-ffmpeg");
        let fixture = root.join("fixture-mobile-relay");
        fs::create_dir_all(&fixture).unwrap();

        let upstream = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
        let upstream_port = upstream.local_addr().unwrap().port();
        let audio_payload =
            b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00M4A mp42isomGENERIC_RELAY_OK".to_vec();
        let payload_for_srv = audio_payload.clone();
        let upstream_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = upstream.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload_for_srv.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(&payload_for_srv);
            }
        });

        let relay_port = {
            let p = TcpListener::bind("127.0.0.1:0").unwrap();
            p.local_addr().unwrap().port()
        };

        fs::write(
            fixture.join("plugin.json"),
            r#"{
                "id": "mobile-relay-sim",
                "name": "Mobile Relay Sim",
                "main": "index.js",
                "permissions": {
                    "network": ["127.0.0.1"],
                    "process": ["ffmpeg"]
                }
            }"#,
        )
        .unwrap();

        fs::write(
            fixture.join("index.js"),
            format!(
                r#"
                export async function suggest() {{
                    // Probing an optional desktop binary on mobile must NOT contain "allowed" in error text.
                    try {{
                        host.spawn("deno", ["--version"]);
                        return ["unexpected"];
                    }} catch (e) {{
                        return [String(/allowed/i.test(e.message)), e.name || ""];
                    }}
                }}

                export async function stream({{ id, quality }}) {{
                    const listenUrl = "http://127.0.0.1:{relay_port}/";
                    const proc = host.spawn("ffmpeg", [
                        "-nostdin", "-hide_banner", "-nostats",
                        "-user_agent", "nsh-mobile-test/1.0",
                        "-i", "http://127.0.0.1:{upstream_port}/track.m4a",
                        "-vn", "-c:a", "libmp3lame", "-b:a", "192k", "-id3v2_version", "0", "-write_xing", "0",
                        "-f", "mp3", "-listen", "1", listenUrl,
                    ]);
                    proc.closeStdin();
                    for (let i = 0; i < 60; i++) {{
                        if (proc.errorOutput().includes("Input #0")) break;
                        await host.sleep(15);
                    }}
                    if (!proc.errorOutput().includes("Input #0")) {{
                        throw new Error("no Input #0: " + proc.errorOutput());
                    }}
                    return {{ url: listenUrl, type: "mp3", quality, bitRate: 192000 }};
                }}
                "#
            ),
        )
        .unwrap();

        // Mobile mode without desktop Allow Programs (`granted = false`):
        // - `ffmpeg` still runs in-process via `VirtualFfmpegShim` (pure passthrough)
        // - optional non-virtualized CLI probes reject without `"allowed"` in error text.
        let mgr = PluginManager::new(root.join("plugins"), root.join("data"))
            .unwrap()
            .with_mobile_override(true);
        let pending = mgr.stage_from_dir(&fixture).expect("stage");
        let src = mgr
            .confirm_staged(&pending, false)
            .expect("confirm mobile ungranted");

        let probe = src.suggest("").expect("mobile probe catches sandbox error");
        assert_eq!(probe, vec!["false", "MobileSandboxError"]);

        let st = src
            .stream("t1", Quality::High)
            .expect("mobile ffmpeg relay");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (ct, bytes) = rt.block_on(async {
            let client = reqwest::Client::new();
            let resp = client.get(&st.url).send().await.expect("GET mobile relay");
            assert!(resp.status().is_success());
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let b = resp.bytes().await.expect("relay body").to_vec();
            (ct, b)
        });
        assert_eq!(ct, "audio/mp4");
        assert_eq!(bytes, audio_payload);

        mgr.shutdown_all();
        let _ = upstream_thread.join();
        let _ = fs::remove_dir_all(&root);
    }
}
