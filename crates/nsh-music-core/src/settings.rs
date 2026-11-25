use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::i18n::Locale;
use crate::model::{PlayerStyle, Quality, StageStyle};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Maximum supported crossfade duration in milliseconds (12 seconds).
pub const MAX_CROSSFADE_MS: u32 = 12_000;

/// Theme appearance mode preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
}

/// Errors encountered while loading or persisting [`AppSettings`].
#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent application settings for `nsh-music`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub theme_mode: ThemeMode,
    pub locale: Locale,
    pub audio_quality: Quality,
    pub replay_gain: bool,
    pub crossfade_ms: u32,
    pub cache_max_mb: u64,
    pub default_player_style: PlayerStyle,
    pub default_stage_style: StageStyle,
    pub lyrics_stage: bool,
    pub hot_comments: bool,
    pub stage_sparks: bool,
    pub fog_backdrop: bool,
    pub smoke_progress: bool,
    pub grow_from_cover: bool,
    pub sidebar_mine_collapsed: bool,
    pub sidebar_saved_collapsed: bool,
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
    pub plugin_configs: BTreeMap<String, serde_json::Value>,
    pub ffmpeg_path: Option<PathBuf>,
    pub local_folders: Vec<PathBuf>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::System,
            locale: Locale::ZhCn,
            audio_quality: Quality::High,
            replay_gain: true,
            crossfade_ms: 0,
            cache_max_mb: 1024,
            default_player_style: PlayerStyle::Cover,
            default_stage_style: StageStyle::Flow,
            lyrics_stage: false,
            hot_comments: true,
            stage_sparks: true,
            fog_backdrop: true,
            smoke_progress: true,
            grow_from_cover: true,
            sidebar_mine_collapsed: false,
            sidebar_saved_collapsed: false,
            enabled_plugins: Vec::new(),
            disabled_plugins: Vec::new(),
            plugin_configs: BTreeMap::new(),
            ffmpeg_path: None,
            local_folders: Vec::new(),
        }
    }
}

impl AppSettings {
    /// Returns the default OS config path (`<config_dir>/nsh-music/settings.json`).
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nsh-music")
            .join("settings.json")
    }

    /// Loads settings from [`Self::default_path`], returning defaults if the file does not exist.
    pub fn load() -> Result<Self, SettingsError> {
        Self::load_from(Self::default_path())
    }

    /// Loads settings from a custom path (ideal for tests or portable profiles).
    /// Returns [`Self::default`] if the file does not exist.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        match fs::read_to_string(path.as_ref()) {
            Ok(raw) => {
                let mut settings: Self = serde_json::from_str(&raw)?;
                settings.crossfade_ms = settings.crossfade_ms.min(MAX_CROSSFADE_MS);
                Ok(settings)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(SettingsError::Io(err)),
        }
    }

    /// Atomically saves settings to [`Self::default_path`].
    pub fn save(&self) -> Result<(), SettingsError> {
        self.save_to(Self::default_path())
    }

    /// Atomically saves settings to `path` via a sibling temporary file + rename.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), SettingsError> {
        let json = serde_json::to_vec_pretty(self)?;
        atomic_write_bytes(path.as_ref(), &json)?;
        Ok(())
    }

    /// Returns whether `plugin_id` is enabled (plugins default to enabled unless explicitly disabled).
    pub fn is_plugin_enabled(&self, plugin_id: &str) -> bool {
        !self.disabled_plugins.iter().any(|id| id == plugin_id)
    }

    /// Marks a plugin enabled or disabled, keeping both lists deduplicated and consistent.
    pub fn set_plugin_enabled(&mut self, plugin_id: &str, enabled: bool) {
        self.enabled_plugins.retain(|id| id != plugin_id);
        self.disabled_plugins.retain(|id| id != plugin_id);
        if enabled {
            self.enabled_plugins.push(plugin_id.to_owned());
        } else {
            self.disabled_plugins.push(plugin_id.to_owned());
        }
    }

    /// Gets the persisted JSON configuration object for `plugin_id`.
    pub fn plugin_config(&self, plugin_id: &str) -> Option<&serde_json::Value> {
        self.plugin_configs.get(plugin_id)
    }

    /// Sets the persisted JSON configuration value for `plugin_id`.
    pub fn set_plugin_config(&mut self, plugin_id: impl Into<String>, config: serde_json::Value) {
        self.plugin_configs.insert(plugin_id.into(), config);
    }

    /// Adds a local music folder if it is not already configured. Returns `true` if added.
    pub fn add_local_folder(&mut self, folder: impl Into<PathBuf>) -> bool {
        let path = folder.into();
        if self.local_folders.iter().any(|existing| existing == &path) {
            return false;
        }
        self.local_folders.push(path);
        true
    }

    /// Removes a local music folder. Returns `true` if present and removed.
    pub fn remove_local_folder(&mut self, folder: &Path) -> bool {
        let before = self.local_folders.len();
        self.local_folders.retain(|existing| existing != folder);
        self.local_folders.len() != before
    }

    /// Returns crossfade duration clamped to `0..=MAX_CROSSFADE_MS`.
    pub fn clamped_crossfade_ms(&self) -> u32 {
        self.crossfade_ms.min(MAX_CROSSFADE_MS)
    }
}

/// Atomically writes `bytes` to `target` by writing to a unique sibling temp file,
/// flushing to disk, and renaming over `target`.
pub(crate) fn atomic_write_bytes(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");

    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_path = parent.join(format!(".{file_name}.{pid}.{seq}.tmp"));

    let write_res = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        // Windows rename fails if destination already exists; fallback to remove + rename.
        #[cfg(windows)]
        {
            if target.exists() {
                let _ = fs::remove_file(target);
                if fs::rename(&tmp_path, target).is_ok() {
                    return Ok(());
                }
            }
        }
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file_path(name: &str) -> PathBuf {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "nsh-music-core-settings-test-{}-{seq}",
                std::process::id()
            ))
            .join(name)
    }

    #[test]
    fn missing_file_loads_defaults() {
        let path = temp_file_path("does-not-exist.json");
        let loaded = AppSettings::load_from(&path).expect("default load");
        assert_eq!(loaded, AppSettings::default());
        assert_eq!(loaded.theme_mode, ThemeMode::System);
        assert_eq!(loaded.locale, Locale::ZhCn);
        assert_eq!(loaded.audio_quality, Quality::High);
        assert!(loaded.replay_gain);
        assert_eq!(loaded.cache_max_mb, 1024);
    }

    #[test]
    fn atomic_save_and_reload_roundtrip() {
        let path = temp_file_path("nested/settings.json");
        let mut settings = AppSettings {
            theme_mode: ThemeMode::Dark,
            locale: Locale::En,
            audio_quality: Quality::Lossless,
            replay_gain: false,
            crossfade_ms: 4_500,
            cache_max_mb: 2048,
            default_player_style: PlayerStyle::Vinyl,
            default_stage_style: StageStyle::Tilt,
            lyrics_stage: true,
            ffmpeg_path: Some(PathBuf::from("/opt/homebrew/bin/ffmpeg")),
            ..AppSettings::default()
        };
        settings.set_plugin_enabled("netease", true);
        settings.set_plugin_enabled("bilibili", false);
        settings.set_plugin_config(
            "netease",
            serde_json::json!({ "cookie": "MUSIC_U=abc", "quality": "lossless" }),
        );
        assert!(settings.add_local_folder("/Users/music/FLAC"));
        assert!(!settings.add_local_folder("/Users/music/FLAC"));

        settings.save_to(&path).expect("save settings");
        let reloaded = AppSettings::load_from(&path).expect("reload settings");
        assert_eq!(reloaded, settings);
        assert!(reloaded.is_plugin_enabled("netease"));
        assert!(!reloaded.is_plugin_enabled("bilibili"));
        assert_eq!(
            reloaded
                .plugin_config("netease")
                .and_then(|v| v.get("quality"))
                .and_then(|v| v.as_str()),
            Some("lossless")
        );

        // Overwrite atomically and ensure no stray temp files remain
        let mut updated = reloaded;
        updated.theme_mode = ThemeMode::Light;
        updated.save_to(&path).expect("overwrite settings");
        let reloaded2 = AppSettings::load_from(&path).expect("reload updated");
        assert_eq!(reloaded2.theme_mode, ThemeMode::Light);

        let dir = path.parent().unwrap();
        let leftover_tmps: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover_tmps.is_empty());

        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }
}
