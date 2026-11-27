use std::sync::{Arc, Mutex};

use nsh_music_core::model::{RepeatMode, Track};
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

/// Playback transport state reported to OS media control surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemotePlaybackStatus {
    #[default]
    Stopped,
    Buffering,
    Playing,
    Paused,
}

impl RemotePlaybackStatus {
    pub const fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }
}

/// Unified loop/shuffle mode exposed to OS media controls (e.g. HarmonyOS AVSession
/// `LoopMode`, Android MediaSession, MPRIS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteLoopMode {
    #[default]
    Sequence,
    Single,
    List,
    Shuffle,
}

impl RemoteLoopMode {
    pub const fn from_repeat_shuffle(repeat: RepeatMode, shuffle: bool) -> Self {
        if shuffle {
            Self::Shuffle
        } else {
            match repeat {
                RepeatMode::One => Self::Single,
                RepeatMode::All => Self::List,
                RepeatMode::Off => Self::Sequence,
            }
        }
    }

    pub const fn to_repeat_shuffle(self) -> (RepeatMode, bool) {
        match self {
            Self::Sequence => (RepeatMode::Off, false),
            Self::Single => (RepeatMode::One, false),
            Self::List => (RepeatMode::All, false),
            Self::Shuffle => (RepeatMode::All, true),
        }
    }

    pub const fn next_cycle(self) -> Self {
        match self {
            Self::Sequence => Self::List,
            Self::List => Self::Single,
            Self::Single => Self::Shuffle,
            Self::Shuffle => Self::Sequence,
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
    ToggleFavorite,
    SetLoopMode(RemoteLoopMode),
    RaiseWindow,
    QuitApp,
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
    pub status: RemotePlaybackStatus,
    pub buffered_ms: u64,
    pub is_favorite: bool,
    pub loop_mode: RemoteLoopMode,
    pub lyric: Option<String>,
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
        let status = if is_playing {
            RemotePlaybackStatus::Playing
        } else {
            RemotePlaybackStatus::Paused
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
            status,
            buffered_ms: 0,
            is_favorite: false,
            loop_mode: RemoteLoopMode::default(),
            lyric: None,
        }
    }
}

/// Platform driver hook implemented by native OS bridges (MPNowPlayingInfoCenter,
/// Windows SMTC, Linux MPRIS D-Bus, Android MediaSession, HarmonyOS AVSession).
pub trait PlatformRemoteDriver: Send + Sync {
    fn backend(&self) -> RemoteBackend;
    fn update_metadata(&self, meta: &NowPlayingMetadata);
    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64);
    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        self.update_playback(meta.is_playing, meta.position_ms, meta.duration_ms);
    }
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
            "[remote:{}] metadata title='{}' artist='{}' status={:?} playing={} pos={}ms/{}ms",
            self.backend.as_str(),
            meta.title,
            meta.artist,
            meta.status,
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

    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        log::trace!(
            "[remote:{}] playback status={:?} loop={:?} fav={} pos={}ms/{}ms buf={}ms",
            self.backend.as_str(),
            meta.status,
            meta.loop_mode,
            meta.is_favorite,
            meta.position_ms,
            meta.duration_ms,
            meta.buffered_ms
        );
    }

    fn clear(&self) {
        log::debug!(
            "[remote:{}] cleared now playing metadata",
            self.backend.as_str()
        );
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

/// Maximum byte length for lyric text pushed to OS media sessions.
const MAX_REMOTE_LYRIC_BYTES: usize = 32_768;

fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
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

    fn current_driver(&self) -> Option<Arc<dyn PlatformRemoteDriver>> {
        self.inner.driver.lock().ok().map(|d| Arc::clone(&d))
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
            driver.update_playback_detail(&meta);
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
        if let Some(driver) = self.current_driver() {
            driver.update_metadata(&meta);
        }
    }

    /// Pushes full track metadata and transport state while preserving same-track
    /// local artwork (`file://` or `/...`), favorite flag, lyrics, and fallback duration.
    #[allow(clippy::too_many_arguments)]
    pub fn set_track_state(
        &self,
        track: &Track,
        status: RemotePlaybackStatus,
        position_ms: u64,
        duration_ms: u64,
        buffered_ms: u64,
        can_next: bool,
        can_prev: bool,
        loop_mode: RemoteLoopMode,
    ) {
        let meta = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            let same_track = guard.track_id == track.id;
            let artwork_url = if same_track
                && (guard.artwork_url.starts_with("file://") || guard.artwork_url.starts_with('/'))
            {
                guard.artwork_url.clone()
            } else if !track.local_cover.is_empty() {
                track.local_cover.clone()
            } else {
                track.album.image_url.clone()
            };
            let resolved_duration = if duration_ms > 0 {
                duration_ms
            } else if track.duration > 0 {
                track.duration as u64
            } else if same_track {
                guard.duration_ms
            } else {
                0
            };
            let is_favorite = if same_track { guard.is_favorite } else { false };
            let lyric = if same_track {
                guard.lyric.clone()
            } else {
                None
            };
            let is_playing = status.is_playing();
            let next = NowPlayingMetadata {
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
                status,
                buffered_ms,
                is_favorite,
                loop_mode,
                lyric,
            };
            *guard = next.clone();
            next
        };

        if let Some(driver) = self.current_driver() {
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
        let loop_mode = self
            .inner
            .metadata
            .lock()
            .map(|m| m.loop_mode)
            .unwrap_or_default();
        let status = if is_playing {
            RemotePlaybackStatus::Playing
        } else {
            RemotePlaybackStatus::Paused
        };
        self.set_track_state(
            track,
            status,
            position_ms,
            duration_ms,
            0,
            can_next,
            can_prev,
            loop_mode,
        );
    }

    /// Updates playback status, timeline position, duration, and buffered position without
    /// re-sending track artwork/title metadata.
    pub fn update_playback_full(
        &self,
        status: RemotePlaybackStatus,
        position_ms: u64,
        duration_ms: u64,
        buffered_ms: u64,
    ) {
        let meta = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            let is_playing = status.is_playing();
            guard.status = status;
            guard.is_playing = is_playing;
            guard.position_ms = position_ms;
            if duration_ms > 0 {
                guard.duration_ms = duration_ms;
            }
            guard.buffered_ms = buffered_ms;
            guard.playback_rate_milli = if is_playing { 1000 } else { 0 };
            guard.clone()
        };
        if let Some(driver) = self.current_driver() {
            driver.update_playback_detail(&meta);
        }
    }

    /// Updates playback state and timeline position without re-sending artwork/title.
    pub fn update_playback_state(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        let buffered_ms = self
            .inner
            .metadata
            .lock()
            .map(|m| m.buffered_ms)
            .unwrap_or(0);
        let status = if is_playing {
            RemotePlaybackStatus::Playing
        } else {
            RemotePlaybackStatus::Paused
        };
        self.update_playback_full(status, position_ms, duration_ms, buffered_ms);
    }

    /// Updates the artwork URI for the currently loaded `track_id` if non-empty and changed.
    pub fn update_artwork(&self, track_id: &str, artwork_uri: impl Into<String>) {
        let uri = artwork_uri.into();
        if uri.is_empty() {
            return;
        }
        let updated = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            if guard.track_id == track_id && guard.artwork_url != uri {
                guard.artwork_url = uri;
                Some(guard.clone())
            } else {
                None
            }
        };
        if let Some(meta) = updated
            && let Some(driver) = self.current_driver()
        {
            driver.update_metadata(&meta);
        }
    }

    /// Updates the lyric payload for `track_id` (truncating to `<= 32768` bytes on a UTF-8
    /// char boundary; empty text clears the lyric to `None`).
    pub fn update_lyric(&self, track_id: &str, lyric: impl Into<String>) {
        let raw = lyric.into();
        let next_lyric = if raw.is_empty() {
            None
        } else {
            Some(truncate_utf8_bytes(&raw, MAX_REMOTE_LYRIC_BYTES).to_owned())
        };
        let updated = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            if guard.track_id == track_id && guard.lyric != next_lyric {
                guard.lyric = next_lyric;
                Some(guard.clone())
            } else {
                None
            }
        };
        if let Some(meta) = updated
            && let Some(driver) = self.current_driver()
        {
            driver.update_metadata(&meta);
        }
    }

    /// Updates the favorite/liked status for `track_id` if changed.
    pub fn update_favorite(&self, track_id: &str, is_favorite: bool) {
        let updated = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            if guard.track_id == track_id && guard.is_favorite != is_favorite {
                guard.is_favorite = is_favorite;
                Some(guard.clone())
            } else {
                None
            }
        };
        if let Some(meta) = updated
            && let Some(driver) = self.current_driver()
        {
            driver.update_playback_detail(&meta);
        }
    }

    /// Updates the loop/shuffle mode and notifies the active driver when a track is loaded.
    pub fn update_loop_mode(&self, loop_mode: RemoteLoopMode) {
        let updated = {
            let Ok(mut guard) = self.inner.metadata.lock() else {
                return;
            };
            guard.loop_mode = loop_mode;
            if !guard.track_id.is_empty() || !guard.title.is_empty() {
                Some(guard.clone())
            } else {
                None
            }
        };
        if let Some(meta) = updated
            && let Some(driver) = self.current_driver()
        {
            driver.update_playback_detail(&meta);
        }
    }

    /// Clears the OS Now-Playing session.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.metadata.lock() {
            *guard = NowPlayingMetadata::default();
        }
        if let Some(driver) = self.current_driver() {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use nsh_music_core::model::{Artist, RepeatMode, Track};

    use super::*;

    #[derive(Default)]
    struct MockDriver {
        metadata_calls: Mutex<Vec<NowPlayingMetadata>>,
        playback_detail_calls: Mutex<Vec<NowPlayingMetadata>>,
        clear_calls: Mutex<usize>,
    }

    impl PlatformRemoteDriver for MockDriver {
        fn backend(&self) -> RemoteBackend {
            RemoteBackend::HarmonyAvSession
        }

        fn update_metadata(&self, meta: &NowPlayingMetadata) {
            self.metadata_calls.lock().unwrap().push(meta.clone());
        }

        fn update_playback(&self, _is_playing: bool, _position_ms: u64, _duration_ms: u64) {}

        fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
            self.playback_detail_calls
                .lock()
                .unwrap()
                .push(meta.clone());
        }

        fn clear(&self) {
            *self.clear_calls.lock().unwrap() += 1;
        }
    }

    fn sample_track(id: &str, name: &str, image_url: &str, duration_ms: i64) -> Track {
        let mut track = Track {
            id: id.to_owned(),
            source: "youtube-music".to_owned(),
            name: name.to_owned(),
            artists: vec![Artist {
                id: "a1".to_owned(),
                name: "Artist".to_owned(),
                ..Artist::default()
            }],
            duration: duration_ms,
            ..Track::default()
        };
        track.album.id = "alb1".to_owned();
        track.album.name = "Album".to_owned();
        track.album.image_url = image_url.to_owned();
        track
    }

    #[test]
    fn test_remote_loop_mode_roundtrips_and_next_cycle() {
        assert_eq!(
            RemoteLoopMode::from_repeat_shuffle(RepeatMode::Off, false),
            RemoteLoopMode::Sequence
        );
        assert_eq!(
            RemoteLoopMode::from_repeat_shuffle(RepeatMode::One, false),
            RemoteLoopMode::Single
        );
        assert_eq!(
            RemoteLoopMode::from_repeat_shuffle(RepeatMode::All, false),
            RemoteLoopMode::List
        );
        for rep in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(
                RemoteLoopMode::from_repeat_shuffle(rep, true),
                RemoteLoopMode::Shuffle
            );
        }

        for mode in [
            RemoteLoopMode::Sequence,
            RemoteLoopMode::Single,
            RemoteLoopMode::List,
            RemoteLoopMode::Shuffle,
        ] {
            let (rep, shuf) = mode.to_repeat_shuffle();
            assert_eq!(RemoteLoopMode::from_repeat_shuffle(rep, shuf), mode);
        }

        assert_eq!(RemoteLoopMode::Sequence.next_cycle(), RemoteLoopMode::List);
        assert_eq!(RemoteLoopMode::List.next_cycle(), RemoteLoopMode::Single);
        assert_eq!(RemoteLoopMode::Single.next_cycle(), RemoteLoopMode::Shuffle);
        assert_eq!(
            RemoteLoopMode::Shuffle.next_cycle(),
            RemoteLoopMode::Sequence
        );
    }

    #[test]
    fn test_set_track_state_preserves_same_track_local_artwork_favorite_and_lyric() {
        let driver = Arc::new(MockDriver::default());
        let remote = RemoteController::with_driver(driver.clone());
        let t1 = sample_track("t1", "Song One", "https://cdn.example/cover.jpg", 180_000);

        // Initial Buffering publish before cover/favorite/lyrics resolve.
        remote.set_track_state(
            &t1,
            RemotePlaybackStatus::Buffering,
            0,
            180_000,
            0,
            true,
            false,
            RemoteLoopMode::Sequence,
        );
        assert_eq!(remote.metadata().status, RemotePlaybackStatus::Buffering);
        assert!(!remote.metadata().is_playing);
        assert_eq!(remote.metadata().playback_rate_milli, 0);

        // AppStore enriches the active track with local cached cover art, favorite, and lyrics.
        remote.update_artwork("t1", "file:///data/covers/t1.jpg");
        remote.update_favorite("t1", true);
        remote.update_lyric("t1", "[00:01.00]Hello world");

        // Second set_track_state for the same track (once decoding enters Playing) must preserve
        // local file:// artwork, is_favorite, and lyric, and fall back to duration when 0.
        let mut t1_no_dur = t1.clone();
        t1_no_dur.duration = 0;
        remote.set_track_state(
            &t1_no_dur,
            RemotePlaybackStatus::Playing,
            500,
            0,
            12_000,
            true,
            true,
            RemoteLoopMode::List,
        );
        let meta = remote.metadata();
        assert_eq!(meta.status, RemotePlaybackStatus::Playing);
        assert!(meta.is_playing);
        assert_eq!(meta.playback_rate_milli, 1000);
        assert_eq!(meta.artwork_url, "file:///data/covers/t1.jpg");
        assert!(meta.is_favorite);
        assert_eq!(meta.lyric.as_deref(), Some("[00:01.00]Hello world"));
        assert_eq!(meta.duration_ms, 180_000);
        assert_eq!(meta.buffered_ms, 12_000);
        assert_eq!(meta.loop_mode, RemoteLoopMode::List);

        // Switching to a different track resets favorite, lyric, and artwork.
        let t2 = sample_track("t2", "Song Two", "https://cdn.example/t2.jpg", 200_000);
        remote.set_track_state(
            &t2,
            RemotePlaybackStatus::Playing,
            0,
            200_000,
            5_000,
            false,
            true,
            RemoteLoopMode::List,
        );
        let meta2 = remote.metadata();
        assert_eq!(meta2.track_id, "t2");
        assert_eq!(meta2.artwork_url, "https://cdn.example/t2.jpg");
        assert!(!meta2.is_favorite);
        assert_eq!(meta2.lyric, None);
    }

    #[test]
    fn test_updates_truncation_set_driver_and_command_dispatch() {
        let driver = Arc::new(MockDriver::default());
        let remote = RemoteController::with_driver(driver.clone());
        let t1 = sample_track("t1", "Song One", "https://cdn.example/c.jpg", 120_000);

        // Loop mode before any track is loaded updates state without pushing playback detail.
        remote.update_loop_mode(RemoteLoopMode::Shuffle);
        assert_eq!(remote.metadata().loop_mode, RemoteLoopMode::Shuffle);
        assert_eq!(driver.playback_detail_calls.lock().unwrap().len(), 0);

        remote.set_track(&t1, true, 1_000, 120_000, true, true);
        assert_eq!(remote.metadata().loop_mode, RemoteLoopMode::Shuffle);

        // Multi-byte UTF-8 lyric exceeding 32768 bytes must truncate cleanly on char boundary.
        let huge_cjk = "歌".repeat(15_000); // 45_000 bytes (3 bytes/char)
        remote.update_lyric("t1", huge_cjk);
        let stored_lyric = remote.metadata().lyric.expect("lyric stored");
        assert!(stored_lyric.len() <= MAX_REMOTE_LYRIC_BYTES);
        assert!(stored_lyric.len() >= MAX_REMOTE_LYRIC_BYTES - 3);
        assert!(stored_lyric.chars().all(|c| c == '歌'));

        // Mismatched track_id updates are ignored.
        let meta_calls_before = driver.metadata_calls.lock().unwrap().len();
        remote.update_artwork("other", "file:///wrong.jpg");
        remote.update_favorite("other", true);
        assert_eq!(
            driver.metadata_calls.lock().unwrap().len(),
            meta_calls_before
        );
        assert!(!remote.metadata().is_favorite);

        // Valid updates fire expected driver hooks.
        remote.update_favorite("t1", true);
        remote.update_loop_mode(RemoteLoopMode::Single);
        remote.update_playback_full(RemotePlaybackStatus::Paused, 42_000, 120_000, 90_000);
        let last_detail = driver
            .playback_detail_calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("playback detail call");
        assert_eq!(last_detail.status, RemotePlaybackStatus::Paused);
        assert!(!last_detail.is_playing);
        assert_eq!(last_detail.position_ms, 42_000);
        assert_eq!(last_detail.buffered_ms, 90_000);
        assert_eq!(last_detail.loop_mode, RemoteLoopMode::Single);
        assert!(last_detail.is_favorite);

        // Attaching a new driver replays both metadata and playback detail when loaded.
        let driver2 = Arc::new(MockDriver::default());
        remote.set_driver(driver2.clone());
        assert_eq!(driver2.metadata_calls.lock().unwrap().len(), 1);
        assert_eq!(driver2.playback_detail_calls.lock().unwrap().len(), 1);

        // Command dispatch for ToggleFavorite, SetLoopMode, RaiseWindow, and QuitApp.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = Arc::clone(&seen);
        remote.on_command(Arc::new(move |cmd| {
            seen_cb.lock().unwrap().push(cmd);
        }));
        let mut rx = remote.subscribe();
        remote.dispatch(RemoteCommand::ToggleFavorite);
        remote.dispatch(RemoteCommand::SetLoopMode(RemoteLoopMode::Shuffle));
        remote.dispatch(RemoteCommand::RaiseWindow);
        remote.dispatch(RemoteCommand::QuitApp);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                RemoteCommand::ToggleFavorite,
                RemoteCommand::SetLoopMode(RemoteLoopMode::Shuffle),
                RemoteCommand::RaiseWindow,
                RemoteCommand::QuitApp,
            ]
        );
        assert_eq!(rx.try_recv().unwrap(), RemoteCommand::ToggleFavorite);
        assert_eq!(
            rx.try_recv().unwrap(),
            RemoteCommand::SetLoopMode(RemoteLoopMode::Shuffle)
        );
        assert_eq!(rx.try_recv().unwrap(), RemoteCommand::RaiseWindow);
        assert_eq!(rx.try_recv().unwrap(), RemoteCommand::QuitApp);
    }
}
