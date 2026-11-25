use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use nsh_music_core::model::{NavItem, PlaybackMode, SettingDef, SettingOption, SourceInfo};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::headers::{HeaderRequestKind, HeaderRule};

/// Errors produced while loading or validating a plugin manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read manifest at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid plugin.json: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid plugin manifest: {0}")]
    Validation(String),
}

/// Permission declarations in `plugin.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginPermissions {
    /// Allowed network hosts/domains (e.g., `["*.bilibili.com", "api.music.163.com"]`).
    #[serde(default, alias = "hosts", alias = "domains")]
    pub network: Vec<String>,
    /// External binaries the plugin may invoke via `host.spawn` (e.g., `["ffmpeg"]`).
    #[serde(default, alias = "commands", alias = "binaries")]
    pub process: Vec<String>,
}

/// Parsed `plugin.json` manifest compatible with `mygo-music` and `nsh-music` extensions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub homepage: String,
    pub repository: String,
    /// Entry JS/TS file relative to plugin root (supports both `main` and `entry`).
    #[serde(alias = "entry")]
    pub main: String,
    pub icon: String,
    pub playback: PlaybackMode,
    /// Declared capability names (accepts either a JSON array of strings or `{ "search": true }` map).
    #[serde(default, deserialize_with = "deserialize_capabilities")]
    pub capabilities: Vec<String>,
    /// Plugin settings schema (supports `settings`, `configSchema`, `config_schema`).
    #[serde(
        default,
        alias = "configSchema",
        alias = "config_schema",
        deserialize_with = "deserialize_settings"
    )]
    pub settings: Vec<SettingDef>,
    /// Sidebar navigation items.
    #[serde(default)]
    pub navigation: Vec<NavItem>,
    /// URL-scoped HTTP request header rules (supports array of rules or host -> headers map).
    #[serde(default, deserialize_with = "deserialize_header_rules")]
    pub request_headers: Vec<HeaderRule>,
    /// Declared network domains (merged with `permissions.network`).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Declared external commands (merged with `permissions.process`).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Structured permissions block (`mygo-music` / `nsh-music`).
    #[serde(default)]
    pub permissions: PluginPermissions,
    /// Minimum compatible host version (semver string, optional).
    #[serde(alias = "minAppVersion", alias = "min_app_version", alias = "min_host_version")]
    pub min_host_version: String,
}

fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(val) = val else {
        return Ok(Vec::new());
    };
    match val {
        serde_json::Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()),
        serde_json::Value::Object(map) => Ok(map
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::Bool(true) => Some(k),
                serde_json::Value::String(s) if !s.is_empty() => Some(k),
                _ => None,
            })
            .collect()),
        _ => Ok(Vec::new()),
    }
}

fn deserialize_settings<'de, D>(deserializer: D) -> Result<Vec<SettingDef>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(val) = val else {
        return Ok(Vec::new());
    };
    match val {
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let Ok(def) = serde_json::from_value::<SettingDef>(item.clone()) {
                    out.push(def);
                } else if let Some(obj) = item.as_object() {
                    out.push(parse_setting_object(None, obj));
                }
            }
            Ok(out)
        }
        serde_json::Value::Object(map) => {
            // Support JSON-Schema-like `{ "properties": { ... } }` or direct `{ "apiKey": { ... } }`.
            let source_map = map
                .get("properties")
                .and_then(|v| v.as_object())
                .unwrap_or(&map);
            let mut out = Vec::with_capacity(source_map.len());
            for (k, v) in source_map {
                if let Some(obj) = v.as_object() {
                    out.push(parse_setting_object(Some(k), obj));
                }
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_setting_object(
    fallback_key: Option<&str>,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> SettingDef {
    let key = obj
        .get("key")
        .and_then(|v| v.as_str())
        .or(fallback_key)
        .unwrap_or_default()
        .to_owned();
    let label = obj
        .get("label")
        .or_else(|| obj.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or(&key)
        .to_owned();
    let help = obj
        .get("help")
        .or_else(|| obj.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let link = obj
        .get("link")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let kind = obj
        .get("type")
        .or_else(|| obj.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("text")
        .to_owned();
    let options = obj
        .get("options")
        .or_else(|| obj.get("enum"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|opt| {
                    if let Some(s) = opt.as_str() {
                        Some(SettingOption {
                            value: s.to_owned(),
                            label: s.to_owned(),
                        })
                    } else if let Some(o) = opt.as_object() {
                        let value = o.get("value")?.as_str()?.to_owned();
                        let label = o
                            .get("label")
                            .and_then(|l| l.as_str())
                            .unwrap_or(&value)
                            .to_owned();
                        Some(SettingOption { value, label })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    SettingDef {
        key,
        label,
        help,
        link,
        kind,
        options,
    }
}

fn deserialize_header_rules<'de, D>(deserializer: D) -> Result<Vec<HeaderRule>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(val) = val else {
        return Ok(Vec::new());
    };
    match val {
        serde_json::Value::Array(items) => {
            let mut rules = Vec::with_capacity(items.len());
            for item in items {
                if let Ok(rule) = serde_json::from_value::<HeaderRule>(item) {
                    rules.push(rule);
                }
            }
            Ok(rules)
        }
        serde_json::Value::Object(map) => {
            // Shorthand: `{ "*.hdslb.com": { "Referer": "https://www.bilibili.com" } }`
            // Or mygo-music form: `{ "image": { "Referer": "..." }, "audio": { ... } }`
            let mut rules = Vec::new();
            for (key, value) in map {
                let Some(headers_obj) = value.as_object() else {
                    continue;
                };
                let mut headers = HashMap::new();
                for (hk, hv) in headers_obj {
                    if let Some(s) = hv.as_str() {
                        headers.insert(hk.clone(), s.to_owned());
                    }
                }
                if headers.is_empty() {
                    continue;
                }
                let lower = key.to_ascii_lowercase();
                if let Some(kind) = match lower.as_str() {
                    "image" | "images" => Some(HeaderRequestKind::Image),
                    "audio" | "stream" => Some(HeaderRequestKind::Audio),
                    "api" | "fetch" => Some(HeaderRequestKind::Api),
                    _ => None,
                } {
                    rules.push(HeaderRule {
                        match_pattern: "*".to_owned(),
                        hosts: Vec::new(),
                        kinds: vec![kind],
                        headers,
                    });
                } else {
                    rules.push(HeaderRule {
                        match_pattern: key,
                        hosts: Vec::new(),
                        kinds: Vec::new(),
                        headers,
                    });
                }
            }
            Ok(rules)
        }
        _ => Ok(Vec::new()),
    }
}

impl PluginManifest {
    /// Parses a manifest from a JSON string and validates required fields.
    pub fn parse_json(json: &str) -> Result<Self, ManifestError> {
        let mut manifest: PluginManifest = serde_json::from_str(json)?;
        manifest.normalize_and_validate()?;
        Ok(manifest)
    }

    /// Loads `plugin.json` from a plugin directory.
    pub fn from_dir(dir: &Path) -> Result<Self, ManifestError> {
        let path = dir.join("plugin.json");
        let content = fs::read_to_string(&path).map_err(|source| ManifestError::Io {
            path: path.clone(),
            source,
        })?;
        Self::parse_json(&content)
    }

    fn normalize_and_validate(&mut self) -> Result<(), ManifestError> {
        self.id = self.id.trim().to_owned();
        if self.id.is_empty() {
            return Err(ManifestError::Validation(
                "manifest `id` must not be empty".to_owned(),
            ));
        }
        if !is_valid_plugin_id(&self.id) {
            return Err(ManifestError::Validation(format!(
                "manifest `id` `{}` contains invalid characters (allowed: ASCII alphanumeric, `.`, `_`, `-`)",
                self.id
            )));
        }
        if self.name.trim().is_empty() {
            self.name = self.id.clone();
        }
        if self.version.trim().is_empty() {
            self.version = "0.1.0".to_owned();
        }
        Ok(())
    }

    /// Returns all declared network hosts/domains (deduplicated).
    pub fn all_network_hosts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for host in self
            .domains
            .iter()
            .map(String::as_str)
            .chain(self.permissions.network.iter().map(String::as_str))
            .chain(self.request_headers.iter().flat_map(|r| r.all_patterns()))
        {
            let h = host.trim();
            if !h.is_empty() && h != "*" && !out.iter().any(|x| x == h) {
                out.push(h.to_owned());
            }
        }
        out
    }

    /// Returns all declared process/CLI commands (deduplicated).
    pub fn all_process_commands(&self) -> Vec<String> {
        let mut out = Vec::new();
        for cmd in self
            .commands
            .iter()
            .chain(self.permissions.process.iter())
        {
            let c = cmd.trim();
            if !c.is_empty() && !out.iter().any(|x| x == c) {
                out.push(c.to_owned());
            }
        }
        out
    }

    /// Resolves the plugin entry script path safely inside `plugin_dir`.
    pub fn resolve_entry_path(&self, plugin_dir: &Path) -> Result<PathBuf, ManifestError> {
        let candidates: Vec<String> = if !self.main.trim().is_empty() {
            vec![self.main.trim().to_owned()]
        } else {
            vec![
                "index.ts".to_owned(),
                "index.js".to_owned(),
                "index.mjs".to_owned(),
                "main.ts".to_owned(),
                "main.js".to_owned(),
            ]
        };

        for rel in &candidates {
            let rel_path = Path::new(rel);
            if rel_path.is_absolute()
                || rel_path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
            {
                return Err(ManifestError::Validation(format!(
                    "manifest entry `{rel}` escapes plugin directory"
                )));
            }
            let full = plugin_dir.join(rel_path);
            if full.is_file() {
                return Ok(full);
            }
        }

        Err(ManifestError::Validation(format!(
            "plugin entry script not found in {} (tried: {})",
            plugin_dir.display(),
            candidates.join(", ")
        )))
    }

    /// Builds a localized `SourceInfo` from this manifest and optional locale bundle.
    pub fn to_source_info(&self, locales: &PluginLocales, locale: &str) -> SourceInfo {
        let name = locales.resolve_text(locale, &self.name);
        let settings = self
            .settings
            .iter()
            .map(|s| SettingDef {
                key: s.key.clone(),
                label: locales.resolve_text(locale, &s.label),
                help: locales.resolve_text(locale, &s.help),
                link: s.link.clone(),
                kind: s.kind.clone(),
                options: s
                    .options
                    .iter()
                    .map(|o| SettingOption {
                        value: o.value.clone(),
                        label: locales.resolve_text(locale, &o.label),
                    })
                    .collect(),
            })
            .collect();
        let navigation = self
            .navigation
            .iter()
            .map(|n| NavItem {
                title: locales.resolve_text(locale, &n.title),
                page: n.page.clone(),
                builtin: n.builtin.clone(),
                icon: n.icon.clone(),
            })
            .collect();

        SourceInfo {
            id: self.id.clone(),
            name,
            version: self.version.clone(),
            playback: self.playback,
            settings,
            navigation,
        }
    }
}

/// Checks whether a plugin ID is safe for filesystem storage and routing.
pub fn is_valid_plugin_id(id: &str) -> bool {
    if id.is_empty() || id == "." || id == ".." || id.contains("..") {
        return false;
    }
    id.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Loaded `locales/<lang>.json` translation tables for a plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginLocales {
    /// Canonical lowercase locale tag -> flattened key/value strings.
    bundles: HashMap<String, HashMap<String, String>>,
}

impl PluginLocales {
    /// Loads all `*.json` files under `<plugin_dir>/locales`.
    pub fn load_from_dir(plugin_dir: &Path) -> Self {
        let locales_dir = plugin_dir.join("locales");
        let Ok(entries) = fs::read_dir(&locales_dir) else {
            return Self::default();
        };
        let mut bundles = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let mut flat = HashMap::new();
            flatten_locale_json("", &json, &mut flat);
            bundles.insert(normalize_locale_tag(stem), flat);
        }
        Self { bundles }
    }

    /// Inserts a locale bundle programmatically (useful for tests or inline plugins).
    pub fn insert_bundle(&mut self, locale: &str, entries: HashMap<String, String>) {
        self.bundles.insert(normalize_locale_tag(locale), entries);
    }

    /// Returns all available locale keys.
    pub fn available_locales(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.bundles.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Translates `key` for `locale`, falling back through `<lang>`, `en`, `en-us`, `zh-cn`, then any loaded bundle.
    pub fn translate(&self, locale: &str, key: &str) -> Option<&str> {
        if self.bundles.is_empty() {
            return None;
        }
        // 1. Exact locale match (`zh-cn`), then base language (`zh`), then same-language regional variant (`zh-*`).
        let norm = normalize_locale_tag(locale);
        if !norm.is_empty() {
            if let Some(val) = self.bundles.get(&norm).and_then(|m| m.get(key)) {
                return Some(val.as_str());
            }
            let base = norm.split_once('-').map_or(norm.as_str(), |(l, _)| l);
            if base != norm
                && let Some(val) = self.bundles.get(base).and_then(|m| m.get(key)) {
                    return Some(val.as_str());
                }
            let prefix = format!("{base}-");
            let mut matching_tags: Vec<&String> = self
                .bundles
                .keys()
                .filter(|t| t.starts_with(&prefix))
                .collect();
            matching_tags.sort();
            for tag in matching_tags {
                if let Some(val) = self.bundles[tag].get(key) {
                    return Some(val.as_str());
                }
            }
        }

        // 2. Standard fallback chain: `en` -> `en-us` -> `zh-cn` -> `zh`.
        for fallback in ["en", "en-us", "zh-cn", "zh"] {
            if let Some(val) = self.bundles.get(fallback).and_then(|m| m.get(key)) {
                return Some(val.as_str());
            }
        }

        // 3. Last resort: first bundle containing the key in deterministic order.
        let mut tags: Vec<&String> = self.bundles.keys().collect();
        tags.sort();
        for tag in tags {
            if let Some(val) = self.bundles[tag].get(key) {
                return Some(val.as_str());
            }
        }
        None
    }

    /// Translates `key` and interpolates `{param}` / `{{param}}` placeholders.
    pub fn translate_with_params(
        &self,
        locale: &str,
        key: &str,
        params: &HashMap<String, String>,
    ) -> String {
        let mut text = self
            .translate(locale, key)
            .unwrap_or(key)
            .to_owned();
        for (k, v) in params {
            text = text.replace(&format!("{{{{{k}}}}}"), v);
            text = text.replace(&format!("{{{k}}}"), v);
        }
        text
    }

    /// Resolves manifest strings that may be direct locale keys or prefixed with `$t:` / `@i18n:`.
    pub fn resolve_text(&self, locale: &str, value: &str) -> String {
        if let Some(key) = value
            .strip_prefix("$t:")
            .or_else(|| value.strip_prefix("@i18n:"))
        {
            return self
                .translate(locale, key.trim())
                .unwrap_or(key.trim())
                .to_owned();
        }
        if let Some(translated) = self.translate(locale, value) {
            return translated.to_owned();
        }
        value.to_owned()
    }
}

fn normalize_locale_tag(tag: &str) -> String {
    tag.trim().replace('_', "-").to_ascii_lowercase()
}

fn flatten_locale_json(prefix: &str, value: &serde_json::Value, out: &mut HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_locale_json(&next, v, out);
            }
        }
        serde_json::Value::String(s) if !prefix.is_empty() => {
            out.insert(prefix.to_owned(), s.clone());
        }
        serde_json::Value::Number(n) if !prefix.is_empty() => {
            out.insert(prefix.to_owned(), n.to_string());
        }
        serde_json::Value::Bool(b) if !prefix.is_empty() => {
            out.insert(prefix.to_owned(), b.to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_mygo_manifest() {
        let json = r#"{
            "id": "bilibili-music",
            "name": "plugin.name",
            "version": "1.2.3",
            "description": "Bilibili audio plugin",
            "author": "community",
            "homepage": "https://example.com/bili",
            "repository": "owner/bilibili-music",
            "entry": "src/index.ts",
            "icon": "icon.svg",
            "capabilities": ["search", "stream", "lyrics"],
            "configSchema": [
                {
                    "key": "cookie",
                    "label": "settings.cookie",
                    "type": "password",
                    "help": "Paste SESSDATA"
                }
            ],
            "requestHeaders": [
                {
                    "match": "*.hdslb.com",
                    "kinds": ["image"],
                    "headers": {
                        "Referer": "https://www.bilibili.com/"
                    }
                }
            ],
            "domains": ["api.bilibili.com"],
            "permissions": {
                "network": ["*.bilivideo.com"],
                "process": ["ffmpeg"]
            },
            "minHostVersion": "0.1.0"
        }"#;

        let manifest = PluginManifest::parse_json(json).expect("manifest parses");
        assert_eq!(manifest.id, "bilibili-music");
        assert_eq!(manifest.main, "src/index.ts");
        assert_eq!(manifest.capabilities, vec!["search", "stream", "lyrics"]);
        assert_eq!(manifest.settings.len(), 1);
        assert_eq!(manifest.settings[0].key, "cookie");
        assert_eq!(manifest.request_headers.len(), 1);
        assert_eq!(manifest.all_process_commands(), vec!["ffmpeg"]);
        let hosts = manifest.all_network_hosts();
        assert!(hosts.contains(&"api.bilibili.com".to_owned()));
        assert!(hosts.contains(&"*.bilivideo.com".to_owned()));
        assert!(hosts.contains(&"*.hdslb.com".to_owned()));
    }

    #[test]
    fn rejects_invalid_plugin_id_and_entry_traversal() {
        assert!(PluginManifest::parse_json(r#"{"id":"../evil"}"#).is_err());
        let manifest = PluginManifest::parse_json(r#"{"id":"ok","main":"../escape.js"}"#)
            .expect("parses before entry check");
        let dir = std::env::temp_dir();
        assert!(manifest.resolve_entry_path(&dir).is_err());
    }

    #[test]
    fn locales_resolve_with_fallback_and_params() {
        let mut locales = PluginLocales::default();
        locales.insert_bundle(
            "zh-CN",
            HashMap::from([
                ("plugin.name".to_owned(), "哔哩音乐".to_owned()),
                ("hello".to_owned(), "你好，{user}".to_owned()),
            ]),
        );
        locales.insert_bundle(
            "en",
            HashMap::from([
                ("plugin.name".to_owned(), "Bili Music".to_owned()),
                ("only.en".to_owned(), "English Fallback".to_owned()),
            ]),
        );

        assert_eq!(locales.translate("zh-CN", "plugin.name"), Some("哔哩音乐"));
        assert_eq!(locales.translate("zh_CN", "plugin.name"), Some("哔哩音乐"));
        assert_eq!(locales.translate("zh", "plugin.name"), Some("哔哩音乐"));
        assert_eq!(
            locales.translate("zh-CN", "only.en"),
            Some("English Fallback")
        );
        assert_eq!(locales.translate("ja-JP", "plugin.name"), Some("Bili Music"));

        let params = HashMap::from([("user".to_owned(), "NSH".to_owned())]);
        assert_eq!(
            locales.translate_with_params("zh-CN", "hello", &params),
            "你好，NSH"
        );
    }
}
