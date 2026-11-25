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
pub mod runtime;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nsh_music_core::model::SettingDef;
use nsh_music_core::source::{MusicSource, SignInPromptCallback, SourceCompat};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::compat::{
    analyze_plugin_dir, analyze_source, CompatFinding, CompatReport, CompatTier,
};
pub use crate::headers::{HeaderRequestKind, HeaderRule, RequestHeadersResolver};
pub use crate::host::{
    HostContext, HostRuntimeOptions, MobileSandboxError, WebSignInHandler, WebSignInRequest,
    WebSignInResult,
};
pub use crate::installer::{
    GitHubRepoSpec, InstallSource, InstalledPluginMeta, InstallerError, PendingPluginInstall,
    PluginInstaller,
};
pub use crate::manifest::{
    is_valid_plugin_id, ManifestError, PluginLocales, PluginManifest, PluginPermissions,
};
pub use crate::runtime::{
    transpile_ts_to_js, PluginSource, QrSignInSession, QrSignInStatus, RuntimeError,
    EXTRA_PLUGIN_EXPORTS, MYGO_PLUGIN_EXPORTS,
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
            && let Some(prev) = loaded.insert(id, Arc::clone(&source)) {
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

    /// Updates program execution permission grant state for an installed plugin.
    pub fn set_granted(&self, plugin_id: &str, granted: bool) -> Result<(), PluginError> {
        self.installer.set_granted(plugin_id, granted)?;
        if let Some(loaded) = self.get(plugin_id) {
            loaded.set_granted(granted);
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
            let loaded_src = loaded_guard
                .as_ref()
                .and_then(|m| m.get(&meta.id).cloned());
            let load_error = errors_guard
                .as_ref()
                .and_then(|m| m.get(&meta.id).cloned());

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
                let manifest = PluginManifest::from_dir(&meta.dir).unwrap_or_else(|_| {
                    PluginManifest {
                        id: meta.id.clone(),
                        name: meta.name.clone(),
                        version: meta.version.clone(),
                        ..PluginManifest::default()
                    }
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
            && let Some(src) = loaded.remove(plugin_id) {
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
                    && let Some(prev) = loaded.insert(meta.id.clone(), Arc::clone(&arc)) {
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
            web_sign_in_handler: self
                .web_sign_in_handler
                .read()
                .ok()
                .and_then(|g| g.clone()),
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
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

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

        let sr = src.search("city pop", SearchKind::Tracks, 10).expect("search");
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
        assert_eq!(src.compatibility_info().level, PluginCompatLevel::VirtualFfmpeg);

        let track = src.get_track("tr-1").expect("getTrack runs TS + crypto + storage + ffmpeg -version");
        assert!(
            track
                .name
                .starts_with("5eb63bbbe01eeed093cb22bb8f5acdc3|hello world|savedDigest|true"),
            "unexpected track name: {}",
            track.name
        );

        let stream = src.stream("tr-1", Quality::High).expect("virtual ffmpeg loopback relay");
        assert_eq!(stream.url, format!("http://127.0.0.1:{relay_port}/"));

        // Fetch audio directly from the virtual ffmpeg loopback server!
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let relayed = rt.block_on(async {
            let client = reqwest::Client::new();
            let resp = client.get(&stream.url).send().await.expect("GET loopback relay");
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
                const code = await ff.wait();
                return {
                    id: "ok",
                    name: `mobile=${host.capabilities.isMobile}|canSpawn=${host.capabilities.canSpawnExternal}|ff=${code === 0 && ff.stdout().includes("ffmpeg")}`,
                    duration: 1000
                };
            }
            export async function search(_opts: any) {
                host.spawn("yt-dlp", ["--version"]);
                return { tracks: [] };
            }
            "#,
        )
        .unwrap();

        let mgr = PluginManager::new(root.join("plugins"), root.join("data"))
            .unwrap()
            .with_mobile_override(true);
        let src = mgr.install_from_dir(&fixture).expect("load plugin in mobile mode");

        assert_eq!(src.compat_report().tier, CompatTier::MigrationRequired);
        assert_eq!(
            src.compatibility_info().level,
            PluginCompatLevel::DesktopProcessLimited
        );

        let track = src.get_track("1").expect("ffmpeg works even on mobile");
        assert_eq!(track.name, "mobile=true|canSpawn=false|ff=true");

        let err = src
            .search("test", SearchKind::Tracks, 0)
            .expect_err("non-ffmpeg binary must fail on mobile");
        let msg = err.to_string();
        assert!(
            msg.contains("MobileSandboxError") && msg.contains("yt-dlp"),
            "unexpected error: {msg}"
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
            zw.write_all(br#"{"id":"sample-zip","name":"Sample Zip","version":"0.2.0","main":"index.js"}"#)
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
        assert!(pending.network_hosts.contains(&"api.example.test".to_owned()));
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
        let res = src.search("hi", SearchKind::Tracks, 0).expect("search after grant");
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
                let has_referer = req.to_ascii_lowercase().contains("referer: https://auth.example.com/");
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
        let src = mgr.install_from_dir(&fixture).expect("install canonical plugin");
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
        assert!(!pending.process_commands.iter().any(|c| c.contains("dynamic")));

        let src = mgr
            .confirm_staged(&pending, true)
            .expect("confirm and load bilibili");
        assert_eq!(src.compatibility_info().level, PluginCompatLevel::VirtualFfmpeg);
        assert!(src.can("home"));
        assert!(src.can("browse"));
        assert!(src.can("search"));
        assert!(src.can("suggest"));
        assert!(src.can("album"));
        assert!(src.can("artist"));
        assert!(src.can("stream"));
        assert!(src.info().navigation.iter().any(|n| n.page == "charts"));

        let home = src.home().expect("bilibili home()");
        assert!(!home.sections.is_empty(), "expected non-empty home sections");

        let charts = src.browse("charts").expect("bilibili browse(charts)");
        assert!(!charts.sections.is_empty(), "expected non-empty charts sections");

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
}
