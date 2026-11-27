use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

use nsh_music_core::model::{NavItem, PlaybackMode, SettingDef, SettingOption, SourceInfo};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::headers::{HeaderRequestKind, HeaderRule, glob_match};

/// Baseline `mygo-music` plugin API compatibility version supported by this host.
pub const MYGO_COMPAT_BASELINE_VERSION: &str = "0.2.0";

static HOST_VERSION_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Overrides the reported host app version for `minAppVersion` checks (pass `Some("")` for
/// dev builds where all plugins load, or `None` to restore the default effective host version).
pub fn set_host_app_version(version: Option<String>) {
    if let Ok(mut guard) = HOST_VERSION_OVERRIDE.write() {
        *guard = version;
    }
}

/// Returns the effective host version used for `minAppVersion` checks:
/// uses any explicit override from [`set_host_app_version`], otherwise the maximum of
/// `env!("CARGO_PKG_VERSION")` and [`MYGO_COMPAT_BASELINE_VERSION`].
pub fn effective_host_version() -> String {
    if let Ok(guard) = HOST_VERSION_OVERRIDE.read()
        && let Some(v) = guard.as_ref()
    {
        return v.clone();
    }
    let pkg = env!("CARGO_PKG_VERSION");
    if is_semver_newer(pkg, MYGO_COMPAT_BASELINE_VERSION) {
        pkg.to_owned()
    } else {
        MYGO_COMPAT_BASELINE_VERSION.to_owned()
    }
}

/// Parses a numeric version string such as `"0.1.8"` or `"v1.2.3"` into integer components.
/// Rejects strings with non-numeric labels (e.g. `"1.2.3-beta"`), matching `mygo-music`'s `versionNumbers`.
pub fn parse_version_numbers(tag: &str) -> Option<Vec<u64>> {
    let trimmed = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
    if trimmed.is_empty() {
        return None;
    }
    let mut nums = Vec::new();
    for part in trimmed.split('.') {
        if part.is_empty() {
            return None;
        }
        let n = part.parse::<u64>().ok()?;
        nums.push(n);
    }
    Some(nums)
}

/// Returns `true` if `candidate` is strictly newer than `current` according to numeric semver parts.
pub fn is_semver_newer(candidate: &str, current: &str) -> bool {
    let Some(a) = parse_version_numbers(candidate) else {
        return false;
    };
    let Some(b) = parse_version_numbers(current) else {
        return false;
    };
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

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
    #[error("{plugin} needs NSH Music {required} or later (current: {current})")]
    NeedsNewerApp {
        plugin: String,
        required: String,
        current: String,
    },
}

/// Permission declarations in `plugin.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginPermissions {
    /// Allowed network hosts/domains (e.g., `["*.bilibili.com", "api.music.163.com"]`).
    #[serde(default, alias = "hosts", alias = "domains")]
    pub network: Vec<String>,
    /// External binaries the plugin may invoke via `host.spawn` (e.g., `["ffmpeg"]` or `true` for `"*"`).
    #[serde(
        default,
        alias = "commands",
        alias = "binaries",
        deserialize_with = "deserialize_programs"
    )]
    pub process: Vec<String>,
}

fn deserialize_programs<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(val) = val else {
        return Ok(Vec::new());
    };
    match val {
        serde_json::Value::Bool(true) => Ok(vec!["*".to_owned()]),
        serde_json::Value::Bool(false) => Ok(Vec::new()),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(serde::de::Error::custom(
                        "process is a list of programs, or true for any",
                    ));
                };
                out.push(s.to_owned());
            }
            Ok(out)
        }
        _ => Err(serde::de::Error::custom(
            "process is a list of programs, or true for any",
        )),
    }
}

impl PluginPermissions {
    /// Returns `true` if `process` permits starting any program (`"*"`).
    pub fn allows_any_process(&self) -> bool {
        self.process.iter().any(|p| p == "*")
    }

    /// Reports whether `command` is permitted by `permissions.process`, matching `mygo-music`'s
    /// `Programs.Allows(dir, command)` semantics.
    pub fn allows_process(&self, plugin_dir: &Path, command: &str) -> bool {
        patterns_allow_process(&self.process, plugin_dir, command, cfg!(windows))
    }
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
    #[serde(
        alias = "minAppVersion",
        alias = "min_app_version",
        alias = "min_host_version"
    )]
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

    /// Returns network hosts/domains explicitly declared in `domains` and `permissions.network`
    /// (excluding `requestHeaders` patterns and wildcard `*`), deduplicated.
    pub fn declared_network_hosts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for host in self
            .domains
            .iter()
            .map(String::as_str)
            .chain(self.permissions.network.iter().map(String::as_str))
        {
            let h = host.trim();
            if !h.is_empty() && h != "*" && !out.iter().any(|x| x == h) {
                out.push(h.to_owned());
            }
        }
        out
    }

    /// Returns `true` if `domains` or `permissions.network` has any declared entry.
    pub fn has_declared_network_policy(&self) -> bool {
        self.domains
            .iter()
            .chain(self.permissions.network.iter())
            .any(|h| !h.trim().is_empty())
    }

    /// Returns `true` if `domains` or `permissions.network` explicitly declares wildcard `*`.
    pub fn declares_wildcard_network(&self) -> bool {
        self.domains
            .iter()
            .chain(self.permissions.network.iter())
            .any(|h| h.trim() == "*")
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
        for cmd in self.commands.iter().chain(self.permissions.process.iter()) {
            let c = cmd.trim();
            if !c.is_empty() && !out.iter().any(|x| x == c) {
                out.push(c.to_owned());
            }
        }
        out
    }

    /// Reports whether `command` is permitted by this manifest's declared process permissions
    /// (`permissions.process` and legacy `commands`), matching `mygo-music`'s `Programs.Allows`.
    pub fn allows_process(&self, plugin_dir: &Path, command: &str) -> bool {
        let patterns = self.all_process_commands();
        patterns_allow_process(&patterns, plugin_dir, command, cfg!(windows))
    }

    /// Validates `minAppVersion` / `minHostVersion` against `host_version`.
    /// Skips comparison when `host_version` is empty (dev builds), while still rejecting malformed
    /// `minAppVersion` strings.
    pub fn check_min_app_version(&self, host_version: &str) -> Result<(), ManifestError> {
        let required = self.min_host_version.trim();
        if required.is_empty() {
            return Ok(());
        }
        if parse_version_numbers(required).is_none() {
            return Err(ManifestError::Validation(format!(
                "minAppVersion isn't a version such as 0.1.8: {required:?}"
            )));
        }
        let host = host_version.trim();
        if host.is_empty() {
            return Ok(());
        }
        if parse_version_numbers(host).is_some() && is_semver_newer(required, host) {
            return Err(ManifestError::NeedsNewerApp {
                plugin: if self.name.is_empty() {
                    self.id.clone()
                } else {
                    self.name.clone()
                },
                required: required.to_owned(),
                current: host.to_owned(),
            });
        }
        Ok(())
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
                || rel_path.components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
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
/// Matches `mygo-music`'s `^[A-Za-z0-9][A-Za-z0-9_.-]*$` rule and rejects reserved `"local"`.
pub fn is_valid_plugin_id(id: &str) -> bool {
    if id.is_empty() || id == "local" || id == "." || id == ".." || id.contains("..") {
        return false;
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn patterns_allow_process(
    patterns: &[String],
    plugin_dir: &Path,
    command: &str,
    is_windows: bool,
) -> bool {
    let cmd_trimmed = command.trim();
    if cmd_trimmed.is_empty() {
        return false;
    }
    let bare = !cmd_trimmed.contains('/') && !cmd_trimmed.contains('\\');
    let cleaned_command = if bare {
        cmd_trimmed.to_owned()
    } else {
        let full = if is_rooted_or_abs(cmd_trimmed) {
            cmd_trimmed.to_owned()
        } else {
            join_normalized_path(plugin_dir, cmd_trimmed)
        };
        clean_path_for_match(&full)
    };

    for raw_pattern in patterns {
        let pat = raw_pattern.trim();
        if pat.is_empty() {
            continue;
        }
        if pat == "*" {
            return true;
        }
        let pat_bare = !pat.contains('/') && !pat.contains('\\');
        if pat_bare != bare {
            continue;
        }

        let mut pattern_cmp = if bare {
            pat.to_owned()
        } else {
            let full = if is_rooted_or_abs(pat) {
                pat.to_owned()
            } else {
                join_normalized_path(plugin_dir, pat)
            };
            clean_path_for_match(&full)
        };
        let mut name_cmp = cleaned_command.clone();

        if is_windows {
            pattern_cmp = pattern_cmp.to_ascii_lowercase();
            name_cmp = name_cmp.to_ascii_lowercase();
            if bare
                && Path::new(&pattern_cmp).extension().is_none()
                && let Some(stripped) = name_cmp.strip_suffix(".exe")
            {
                name_cmp = stripped.to_owned();
            }
        }

        if path_segments_glob_match(&pattern_cmp, &name_cmp) {
            return true;
        }
    }

    false
}

fn is_rooted_or_abs(p: &str) -> bool {
    Path::new(p).is_absolute()
        || p.starts_with('/')
        || p.starts_with('\\')
        || starts_with_windows_drive(p)
}

fn starts_with_windows_drive(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\')
}

fn join_normalized_path(base: &Path, rel: &str) -> String {
    let base_s = base.to_string_lossy().replace('\\', "/");
    let rel_s = rel.replace('\\', "/");
    if base_s.is_empty() {
        rel_s
    } else {
        format!(
            "{}/{}",
            base_s.trim_end_matches('/'),
            rel_s.trim_start_matches('/')
        )
    }
}

fn clean_path_for_match(raw: &str) -> String {
    let slash = raw.replace('\\', "/");
    let is_abs = slash.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in slash.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(&last) = stack.last() {
                    if last != ".." && !last.ends_with(':') {
                        stack.pop();
                    } else if !is_abs {
                        stack.push("..");
                    }
                } else if !is_abs {
                    stack.push("..");
                }
            }
            s => stack.push(s),
        }
    }
    if is_abs {
        format!("/{}", stack.join("/"))
    } else if stack.is_empty() {
        ".".to_owned()
    } else {
        stack.join("/")
    }
}

/// Segment-scoped glob matching (`*` and `?` never cross `/`), matching Go's `filepath.Match`.
fn path_segments_glob_match(pattern: &str, name: &str) -> bool {
    if pattern.starts_with('/') != name.starts_with('/') {
        return false;
    }
    let p_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let n_segs: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
    if p_segs.len() != n_segs.len() {
        return false;
    }
    p_segs
        .into_iter()
        .zip(n_segs)
        .all(|(ps, ns)| glob_match(ps, ns))
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

    /// Translates `key` for `locale`, falling back to `en` / `en-us` for non-English locales.
    /// When `locale` is English (or no matching English bundle exists), returns `None` rather than
    /// falling back to a non-English bundle (e.g. `zh-CN.json`), preserving English phrase keys
    /// used by `mygo-music` plugins such as `youtube-music` and `bilibili`.
    pub fn translate(&self, locale: &str, key: &str) -> Option<&str> {
        if self.bundles.is_empty() {
            return None;
        }
        // 1. Exact locale match (`zh-cn`), then base language (`zh`), then same-language regional variant (`zh-*`).
        let norm = normalize_locale_tag(locale);
        let base = norm.split_once('-').map_or(norm.as_str(), |(l, _)| l);
        if !norm.is_empty() {
            if let Some(val) = self.bundles.get(&norm).and_then(|m| m.get(key)) {
                return Some(val.as_str());
            }
            if base != norm
                && let Some(val) = self.bundles.get(base).and_then(|m| m.get(key))
            {
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

        // 2. Universal English fallback (`en` -> `en-us`) when the requested locale isn't English.
        if base != "en" {
            for fallback in ["en", "en-us"] {
                if let Some(val) = self.bundles.get(fallback).and_then(|m| m.get(key)) {
                    return Some(val.as_str());
                }
            }
        }

        // 3. Only fall back to arbitrary bundles when no locale was specified at all.
        if norm.is_empty() {
            for fallback in ["zh-cn", "zh"] {
                if let Some(val) = self.bundles.get(fallback).and_then(|m| m.get(key)) {
                    return Some(val.as_str());
                }
            }
            let mut tags: Vec<&String> = self.bundles.keys().collect();
            tags.sort();
            for tag in tags {
                if let Some(val) = self.bundles[tag].get(key) {
                    return Some(val.as_str());
                }
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
        let mut text = self.translate(locale, key).unwrap_or(key).to_owned();
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

        let declared = manifest.declared_network_hosts();
        assert!(declared.contains(&"api.bilibili.com".to_owned()));
        assert!(declared.contains(&"*.bilivideo.com".to_owned()));
        assert!(
            !declared.contains(&"*.hdslb.com".to_owned()),
            "requestHeaders hosts must not leak into declared_network_hosts()"
        );
        assert!(manifest.has_declared_network_policy());
        assert!(!manifest.declares_wildcard_network());
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
        assert_eq!(
            locales.translate("ja-JP", "plugin.name"),
            Some("Bili Music")
        );

        let params = HashMap::from([("user".to_owned(), "NSH".to_owned())]);
        assert_eq!(
            locales.translate_with_params("zh-CN", "hello", &params),
            "你好，NSH"
        );
    }

    #[test]
    fn zh_only_locales_preserve_english_phrase_keys_in_english_mode() {
        let mut locales = PluginLocales::default();
        locales.insert_bundle(
            "zh-CN",
            HashMap::from([
                ("Charts".to_owned(), "排行榜".to_owned()),
                ("New Releases".to_owned(), "新发行".to_owned()),
            ]),
        );

        assert_eq!(locales.resolve_text("zh-CN", "Charts"), "排行榜");
        assert_eq!(locales.resolve_text("zh-CN", "New Releases"), "新发行");
        assert_eq!(locales.resolve_text("en", "Charts"), "Charts");
        assert_eq!(
            locales.resolve_text("en-US", "New Releases"),
            "New Releases"
        );
    }

    #[test]
    fn process_permissions_support_bool_and_mygo_allows_semantics() {
        let m_true = PluginManifest::parse_json(r#"{"id":"p-all","permissions":{"process":true}}"#)
            .expect("process: true parses");
        assert!(m_true.permissions.allows_any_process());
        assert_eq!(m_true.permissions.process, vec!["*"]);
        assert!(m_true.allows_process(Path::new("/plugins/p-all"), "anything"));
        assert!(m_true.allows_process(Path::new("/plugins/p-all"), "/usr/bin/foo"));

        let m_false =
            PluginManifest::parse_json(r#"{"id":"p-none","permissions":{"process":false}}"#)
                .expect("process: false parses");
        assert!(m_false.permissions.process.is_empty());
        assert!(!m_false.allows_process(Path::new("/plugins/p-none"), "ffmpeg"));

        let m_scoped = PluginManifest::parse_json(
            r#"{"id":"p-scoped","permissions":{"process":["ffmpeg","bin/helper-*","/opt/homebrew/bin/yt-dlp"]}}"#,
        )
        .expect("scoped process list parses");
        let pdir = Path::new("/plugins/p-scoped");
        // Bare name matches bare pattern ONLY (not arbitrary path with same basename).
        assert!(m_scoped.allows_process(pdir, "ffmpeg"));
        assert!(!m_scoped.allows_process(pdir, "/tmp/evil/ffmpeg"));
        // Relative path resolves against plugin_dir and matches single-segment glob.
        assert!(m_scoped.allows_process(pdir, "bin/helper-arm64"));
        assert!(m_scoped.allows_process(pdir, "/plugins/p-scoped/bin/helper-v2"));
        assert!(!m_scoped.allows_process(pdir, "bin/sub/helper-v2"));
        assert!(!m_scoped.allows_process(pdir, "helper-v2"));
        // Absolute path matches absolute pattern.
        assert!(m_scoped.allows_process(pdir, "/opt/homebrew/bin/yt-dlp"));
        assert!(!m_scoped.allows_process(pdir, "yt-dlp"));

        // Windows case-insensitive + bare `.exe` + drive-letter path matching.
        assert!(patterns_allow_process(
            &["ffmpeg".to_owned()],
            pdir,
            "FFMPEG.EXE",
            true
        ));
        assert!(!patterns_allow_process(
            &["ffmpeg.bat".to_owned()],
            pdir,
            "ffmpeg.exe",
            true
        ));
        let win_pdir = Path::new(r"C:\plugins\p-scoped");
        assert!(patterns_allow_process(
            &["bin/helper-*".to_owned()],
            win_pdir,
            r"bin\helper-win.exe",
            true
        ));
        assert!(patterns_allow_process(
            &["bin/helper-*".to_owned()],
            win_pdir,
            r"C:\plugins\p-scoped\bin\helper-win.exe",
            true
        ));
        assert!(!patterns_allow_process(
            &["bin/helper-*".to_owned()],
            win_pdir,
            r"C:\evil\bin\helper-win.exe",
            true
        ));
    }

    #[test]
    fn validates_plugin_id_and_min_app_version() {
        assert!(!is_valid_plugin_id("local"));
        assert!(!is_valid_plugin_id("-starts-with-dash"));
        assert!(!is_valid_plugin_id(".hidden"));
        assert!(!is_valid_plugin_id("a..b"));
        assert!(is_valid_plugin_id("youtube-music"));
        assert!(is_valid_plugin_id("MyPlugin_1.0"));

        let m = PluginManifest::parse_json(
            r#"{"id":"ytm","name":"YouTube Music","minAppVersion":"0.1.8"}"#,
        )
        .unwrap();
        // Empty host version (dev build) skips version check.
        assert!(m.check_min_app_version("").is_ok());
        assert!(m.check_min_app_version("0.1.8").is_ok());
        assert!(m.check_min_app_version("0.2.0").is_ok());
        let err = m
            .check_min_app_version("0.1.0")
            .expect_err("older host fails");
        assert!(matches!(err, ManifestError::NeedsNewerApp { .. }));
        assert!(err.to_string().contains("needs NSH Music 0.1.8"));

        let bad =
            PluginManifest::parse_json(r#"{"id":"bad","minAppVersion":"0.1.8-beta"}"#).unwrap();
        assert!(bad.check_min_app_version("").is_err());
    }
}
