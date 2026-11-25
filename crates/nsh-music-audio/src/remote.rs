use std::sync::{Arc, Mutex};

use nsh_music_core::model::Track;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Operating-system media control backend identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteBackend {
    MacOsNowPlaying,
    WindowsSmtc,
    LinuxMpris,
    IosControlCenter,
    AndroidMediaSession,
    HarmonyAvSession,
    Null,
}

impl RemoteBackend {
    /// Detects the native OS media control backend for the current compilation target.
    pub const fn current_platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOsNowPlaying
        }
        #[cfg(target_os = "windows")]
        {
            Self::WindowsSmtc
        }
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        {
            Self::LinuxMpris
        }
        #[cfg(target_os = "ios")]
        {
            Self::IosControlCenter
        }
        #[cfg(target_os = "android")]
        {
            Self::AndroidMediaSession
        }
        #[cfg(target_env = "ohos")]
        {
            Self::HarmonyAvSession
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "windows",
            target_os = "linux",
            target_os = "ios",
            target_os = "android",
            target_env = "ohos"
        )))]
        {
            Self::Null
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOsNowPlaying => "macos-now-playing",
            Self::WindowsSmtc => "windows-smtc",
            Self::LinuxMpris => "linux-mpris",
            Self::IosControlCenter => "ios-control-center",
            Self::AndroidMediaSession => "android-media-session",
            Self::HarmonyAvSession => "harmony-avsession",
            Self::Null => "null",
        }
    }
}

/// Media transport commands dispatched from OS media controls (lock screen,
/// Bluetooth headset, keyboard media keys, Control Center, SMTC, MPRIS, AVSession).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteCommand {
    Play,
    Pause,
    TogglePlayPause,
    Stop,
    Next,
    Prev,
    Seek(u64),
    SeekRelative(i64),
    SetVolume(u8),
}

/// Now-Playing metadata pushed to the OS media control surface.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NowPlayingMetadata {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artwork_url: String,
    pub duration_ms: u64,
    pub position_ms: u64,
    pub is_playing: bool,
    pub can_next: bool,
    pub can_prev: bool,
    /// Playback speed in permille (`1000` == 1.0x).
    pub playback_rate_milli: u32,
}

impl NowPlayingMetadata {
    pub fn from_track(
        track: &Track,
        is_playing: bool,
        position_ms: u64,
        duration_ms: u64,
        can_next: bool,
        can_prev: bool,
    ) -> Self {
        let artwork_url = if !track.local_cover.is_empty() {
            track.local_cover.clone()
        } else {
            track.album.image_url.clone()
        };
        let resolved_duration = if duration_ms > 0 {
            duration_ms
        } else {
            track.duration.max(0) as u64
        };
        Self {
            track_id: track.id.clone(),
            title: track.name.clone(),
            artist: track.artist_names(),
            album: track.album.name.clone(),
            artwork_url,
            duration_ms: resolved_duration,
            position_ms,
            is_playing,
            can_next,
            can_prev,
            playback_rate_milli: if is_playing { 1000 } else { 0 },
        }
    }
}

/// Platform driver hook implemented by native OS bridges (MPNowPlayingInfoCenter,
/// Windows SMTC, Linux MPRIS D-Bus, Android MediaSession, HarmonyOS AVSession).
pub trait PlatformRemoteDriver: Send + Sync {
    fn backend(&self) -> RemoteBackend;
    fn update_metadata(&self, meta: &NowPlayingMetadata);
    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64);
    fn clear(&self);
}

/// Default in-process driver recording platform media state for all OS targets
/// until an optional platform-shell bridge is attached.
#[derive(Debug, Default)]
pub struct DefaultPlatformRemoteDriver {
    backend: RemoteBackend,
}

impl DefaultPlatformRemoteDriver {
    pub const fn for_current_platform() -> Self {
        Self {
            backend: RemoteBackend::current_platform(),
        }
    }
}

impl Default for RemoteBackend {
    fn default() -> Self {
        Self::current_platform()
    }
}

impl PlatformRemoteDriver for DefaultPlatformRemoteDriver {
    fn backend(&self) -> RemoteBackend {
        self.backend
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        log::debug!(
            "[remote:{}] metadata title='{}' artist='{}' playing={} pos={}ms/{}ms",
            self.backend.as_str(),
            meta.title,
            meta.artist,
            meta.is_playing,
            meta.position_ms,
            meta.duration_ms
        );
    }

    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        log::trace!(
            "[remote:{}] playback playing={} pos={}ms/{}ms",
            self.backend.as_str(),
            is_playing,
            position_ms,
            duration_ms
        );
    }

    fn clear(&self) {
        log::debug!("[remote:{}] cleared now playing metadata", self.backend.as_str());
    }
}

/// Callback invoked whenever an OS remote control command arrives.
pub type RemoteCommandHandler = Arc<dyn Fn(RemoteCommand) + Send + Sync>;

struct RemoteControllerInner {
    driver: Mutex<Arc<dyn PlatformRemoteDriver>>,
    metadata: Mutex<NowPlayingMetadata>,
    handlers: Mutex<Vec<RemoteCommandHandler>>,
    cmd_tx: broadcast::Sender<RemoteCommand>,
}

/// Thread-safe, clonable OS media control bridge supporting macOS Now Playing,
/// Windows SMTC, Linux MPRIS, iOS Control Center, Android MediaSession, and
/// HarmonyOS AVSession.
#[derive(Clone)]
pub struct RemoteController {
    inner: Arc<RemoteControllerInner>,
}

impl Default for RemoteController {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RemoteController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteController")
            .field("backend", &self.backend())
            .field("metadata", &self.metadata())
            .finish()
    }
}

impl RemoteController {
    /// Creates a [`RemoteController`] using the current platform's default driver.
    pub fn new() -> Self {
        Self::with_driver(Arc::new(DefaultPlatformRemoteDriver::for_current_platform()))
    }

    /// Creates a [`RemoteController`] backed by a custom platform driver bridge.
    pub fn with_driver(driver: Arc<dyn PlatformRemoteDriver>) -> Self {
        let (cmd_tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RemoteControllerInner {
                driver: Mutex::new(driver),
                metadata: Mutex::new(NowPlayingMetadata::default()),
                handlers: Mutex::new(Vec::new()),
                cmd_tx,
            }),
        }
    }

    /// Replaces the active OS driver bridge at runtime (e.g. when a platform shell attaches
    /// a native AVSession / MediaSession / NowPlayingInfoCenter adapter).
    pub fn set_driver(&self, driver: Arc<dyn PlatformRemoteDriver>) {
        let meta = self.metadata();
        if let Ok(mut guard) = self.inner.driver.lock() {
            *guard = Arc::clone(&driver);
        }
        if !meta.title.is_empty() || !meta.track_id.is_empty() {
            driver.update_metadata(&meta);
        }
    }

    pub fn backend(&self) -> RemoteBackend {
        self.inner
            .driver
            .lock()
            .map(|d| d.backend())
            .unwrap_or(RemoteBackend::Null)
    }

    /// Pushes updated Now-Playing metadata to the OS media surface.
    pub fn set_metadata(&self, meta: NowPlayingMetadata) {
        if let Ok(mut guard) = self.inner.metadata.lock() {
            *guard = meta.clone();
        }
        if let Ok(driver) = self.inner.driver.lock() {
            driver.update_metadata(&meta);
        }
    }

    /// Convenience helper building [`NowPlayingMetadata`] directly from a [`Track`].
    pub fn set_track(
        &self,
        track: &Track,
        is_playing: bool,
        position_ms: u64,
        duration_ms: u64,
        can_next: bool,
        can_prev: bool,
    ) {
        let meta = NowPlayingMetadata::from_track(
            track,
            is_playing,
            position_ms,
            duration_ms,
            can_next,
            can_prev,
        );
        self.set_metadata(meta);
    }

    /// Updates playback state and timeline position without re-sending artwork/title.
    pub fn update_playback_state(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        if let Ok(mut guard) = self.inner.metadata.lock() {
            guard.is_playing = is_playing;
            guard.position_ms = position_ms;
            if duration_ms > 0 {
                guard.duration_ms = duration_ms;
            }
            guard.playback_rate_milli = if is_playing { 1000 } else { 0 };
        }
        if let Ok(driver) = self.inner.driver.lock() {
            driver.update_playback(is_playing, position_ms, duration_ms);
        }
    }

    /// Clears the OS Now-Playing session.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.metadata.lock() {
            *guard = NowPlayingMetadata::default();
        }
        if let Ok(driver) = self.inner.driver.lock() {
            driver.clear();
        }
    }

    /// Returns the most recently published Now-Playing metadata snapshot.
    pub fn metadata(&self) -> NowPlayingMetadata {
        self.inner
            .metadata
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// Registers a synchronous command handler invoked on incoming OS media commands.
    pub fn on_command(&self, handler: RemoteCommandHandler) {
        if let Ok(mut guard) = self.inner.handlers.lock() {
            guard.push(handler);
        }
    }

    /// Subscribes to incoming OS media commands as a Tokio broadcast stream.
    pub fn subscribe(&self) -> broadcast::Receiver<RemoteCommand> {
        self.inner.cmd_tx.subscribe()
    }

    /// Dispatches an incoming OS media command to all registered handlers and subscribers.
    pub fn dispatch(&self, cmd: RemoteCommand) {
        let _ = self.inner.cmd_tx.send(cmd);
        let handlers = self
            .inner
            .handlers
            .lock()
            .map(|h| h.clone())
            .unwrap_or_default();
        for handler in handlers {
            handler(cmd);
        }
    }
}
