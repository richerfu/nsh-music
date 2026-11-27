//! Per-platform native media session and OS integration drivers for `nsh-music`.
//!
//! Each target platform lives in its own directory under `platforms/<os>/`:
//! - `android/` — Android `MediaSession` + `Notification.MediaStyle` + build-time compiled Java bridge
//! - `ios/` — iOS `MPNowPlayingInfoCenter` + `MPRemoteCommandCenter` + `AVAudioSession`
//! - `macos/` — macOS `MPNowPlayingInfoCenter` + `MPRemoteCommandCenter` + AppKit headless guard
//! - `windows/` — Windows WinRT MTA System Media Transport Controls (SMTC)
//! - `linux/` — Linux GDBus MPRIS2 (`org.mpris.MediaPlayer2.nshmusic`)
//! - `harmonyos/` — HarmonyOS `AVSession` + `wantAgent` + `backgroundTaskManager`

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple_common;

#[cfg(target_os = "android")]
#[path = "../android/mod.rs"]
mod android;

#[cfg(target_os = "ios")]
#[path = "../ios/mod.rs"]
mod ios;

#[cfg(target_os = "macos")]
#[path = "../macos/mod.rs"]
mod macos;

#[cfg(target_os = "windows")]
#[path = "../windows/mod.rs"]
mod windows;

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
#[path = "../linux/mod.rs"]
mod linux;

#[cfg(target_env = "ohos")]
#[path = "../harmonyos/mod.rs"]
mod harmonyos;

use std::path::PathBuf;

use nsh_music_audio::{NowPlayingMetadata, RemoteController, RemoteLoopMode, RemotePlaybackStatus};

/// Installs the native OS media session driver onto `remote` for the active
/// compilation target (macOS, iOS, Windows, Linux, Android, HarmonyOS).
pub fn install_platform_remote(remote: &RemoteController) {
    #[cfg(target_os = "macos")]
    {
        remote.set_driver(std::sync::Arc::new(macos::MacOsNowPlayingDriver::new(
            remote.clone(),
        )));
    }
    #[cfg(target_os = "ios")]
    {
        remote.set_driver(std::sync::Arc::new(ios::IosNowPlayingDriver::new(
            remote.clone(),
        )));
    }
    #[cfg(target_os = "windows")]
    {
        remote.set_driver(std::sync::Arc::new(windows::WindowsSmtcDriver::new(
            remote.clone(),
        )));
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        remote.set_driver(std::sync::Arc::new(linux::LinuxMprisDriver::new(
            remote.clone(),
        )));
    }
    #[cfg(target_os = "android")]
    {
        remote.set_driver(std::sync::Arc::new(
            android::AndroidMediaSessionDriver::new(remote.clone()),
        ));
    }
    #[cfg(target_env = "ohos")]
    {
        remote.set_driver(std::sync::Arc::new(harmonyos::HarmonyAvSessionDriver::new(
            remote.clone(),
        )));
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos",
    )))]
    {
        let _ = remote;
    }
}

/// Resolves a local filesystem path from an artwork URI (`file://...` or
/// native absolute path). Returns `None` for remote HTTP(S) URLs or empty strings.
#[allow(dead_code)]
pub(crate) fn artwork_local_path(artwork_url: &str) -> Option<PathBuf> {
    let trimmed = artwork_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("file://") {
        return url::Url::parse(trimmed).ok()?.to_file_path().ok();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return None;
    }
    if trimmed.starts_with('/') || trimmed.starts_with("\\\\") {
        return Some(PathBuf::from(trimmed));
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return Some(PathBuf::from(trimmed));
    }
    None
}

/// Returns `Some(trimmed_url)` if `artwork_url` is an HTTP(S) URL suitable for
/// remote artwork fallback fetching.
#[allow(dead_code)]
pub(crate) fn artwork_http_url(artwork_url: &str) -> Option<&str> {
    let trimmed = artwork_url.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Apple MediaPlayer (macOS / iOS) mapping helpers
// ---------------------------------------------------------------------------

/// Maps [`RemotePlaybackStatus`] to `MPNowPlayingPlaybackState`:
/// `1 = Playing`, `2 = Paused`, `3 = Stopped`.
#[allow(dead_code)]
pub(crate) const fn apple_playback_state(status: RemotePlaybackStatus) -> usize {
    match status {
        RemotePlaybackStatus::Playing => 1,
        RemotePlaybackStatus::Paused | RemotePlaybackStatus::Buffering => 2,
        RemotePlaybackStatus::Stopped => 3,
    }
}

/// Maps [`NowPlayingMetadata`] to `MPNowPlayingInfoPropertyPlaybackRate`
/// (`1.0` or active rate when playing; `0.0` when paused/buffering/stopped).
#[allow(dead_code)]
pub(crate) fn apple_playback_rate(meta: &NowPlayingMetadata) -> f64 {
    if meta.status.is_playing() {
        if meta.playback_rate_milli > 0 {
            (meta.playback_rate_milli as f64 / 1000.0).max(0.1)
        } else {
            1.0
        }
    } else {
        0.0
    }
}

/// Maps [`RemoteLoopMode`] to `(MPRepeatType, MPShuffleType)`:
/// `MPRepeatType`: `0 = Off`, `1 = One`, `2 = All`;
/// `MPShuffleType`: `0 = Off`, `1 = Items`.
#[allow(dead_code)]
pub(crate) const fn apple_repeat_shuffle_from_loop(mode: RemoteLoopMode) -> (isize, isize) {
    match mode {
        RemoteLoopMode::Sequence => (0, 0),
        RemoteLoopMode::Single => (1, 0),
        RemoteLoopMode::List => (2, 0),
        RemoteLoopMode::Shuffle => (2, 1),
    }
}

/// Maps Apple `MPRepeatType` (`0 = Off`, `1 = One`, `2 = All`) to [`RemoteLoopMode`].
#[allow(dead_code)]
pub(crate) const fn apple_loop_from_repeat(repeat_type: isize) -> RemoteLoopMode {
    match repeat_type {
        1 => RemoteLoopMode::Single,
        2 => RemoteLoopMode::List,
        _ => RemoteLoopMode::Sequence,
    }
}

/// Maps Apple `MPShuffleType` (`0 = Off`, `1 = Items`, `2 = Collections`) to [`RemoteLoopMode`].
#[allow(dead_code)]
pub(crate) const fn apple_loop_from_shuffle(
    shuffle_type: isize,
    current_loop: RemoteLoopMode,
) -> RemoteLoopMode {
    if shuffle_type != 0 {
        RemoteLoopMode::Shuffle
    } else if matches!(current_loop, RemoteLoopMode::Shuffle) {
        RemoteLoopMode::Sequence
    } else {
        current_loop
    }
}

// ---------------------------------------------------------------------------
// Windows SMTC mapping helpers
// ---------------------------------------------------------------------------

/// Maps [`RemotePlaybackStatus`] to WinRT `MediaPlaybackStatus`:
/// `1 = Changing`, `2 = Stopped`, `3 = Playing`, `4 = Paused`.
#[allow(dead_code)]
pub(crate) const fn smtc_playback_status(status: RemotePlaybackStatus) -> i32 {
    match status {
        RemotePlaybackStatus::Buffering => 1,
        RemotePlaybackStatus::Stopped => 2,
        RemotePlaybackStatus::Playing => 3,
        RemotePlaybackStatus::Paused => 4,
    }
}

/// Maps [`RemoteLoopMode`] to `(MediaPlaybackAutoRepeatMode, shuffle_enabled)`:
/// `0 = None`, `1 = Track`, `2 = List`.
#[allow(dead_code)]
pub(crate) const fn smtc_repeat_shuffle(mode: RemoteLoopMode) -> (i32, bool) {
    match mode {
        RemoteLoopMode::Sequence => (0, false),
        RemoteLoopMode::Single => (1, false),
        RemoteLoopMode::List => (2, false),
        RemoteLoopMode::Shuffle => (2, true),
    }
}

/// Maps WinRT `MediaPlaybackAutoRepeatMode` (`0 = None`, `1 = Track`, `2 = List`)
/// to [`RemoteLoopMode`].
#[allow(dead_code)]
pub(crate) const fn smtc_loop_from_repeat(repeat_mode: i32) -> RemoteLoopMode {
    match repeat_mode {
        1 => RemoteLoopMode::Single,
        2 => RemoteLoopMode::List,
        _ => RemoteLoopMode::Sequence,
    }
}

/// Maps WinRT SMTC shuffle toggle to [`RemoteLoopMode`].
#[allow(dead_code)]
pub(crate) const fn smtc_loop_from_shuffle(
    shuffle_enabled: bool,
    current_loop: RemoteLoopMode,
) -> RemoteLoopMode {
    if shuffle_enabled {
        RemoteLoopMode::Shuffle
    } else if matches!(current_loop, RemoteLoopMode::Shuffle) {
        RemoteLoopMode::Sequence
    } else {
        current_loop
    }
}

/// Converts milliseconds to WinRT `TimeSpan` 100-nanosecond ticks.
#[allow(dead_code)]
pub(crate) const fn ms_to_hns(ms: u64) -> i64 {
    let ticks = ms.saturating_mul(10_000);
    if ticks > i64::MAX as u64 {
        i64::MAX
    } else {
        ticks as i64
    }
}

/// Converts WinRT `TimeSpan` 100-nanosecond ticks to milliseconds (clamping negative to `0`).
#[allow(dead_code)]
pub(crate) const fn hns_to_ms(hns: i64) -> u64 {
    if hns <= 0 { 0 } else { (hns as u64) / 10_000 }
}

// ---------------------------------------------------------------------------
// Linux MPRIS2 D-Bus mapping helpers
// ---------------------------------------------------------------------------

/// Maps [`RemotePlaybackStatus`] to MPRIS `PlaybackStatus` string.
#[allow(dead_code)]
pub(crate) const fn mpris_playback_status(status: RemotePlaybackStatus) -> &'static str {
    match status {
        RemotePlaybackStatus::Playing => "Playing",
        RemotePlaybackStatus::Paused | RemotePlaybackStatus::Buffering => "Paused",
        RemotePlaybackStatus::Stopped => "Stopped",
    }
}

/// Maps [`RemoteLoopMode`] to MPRIS `(LoopStatus, Shuffle)`.
#[allow(dead_code)]
pub(crate) const fn mpris_loop_and_shuffle(mode: RemoteLoopMode) -> (&'static str, bool) {
    match mode {
        RemoteLoopMode::Sequence => ("None", false),
        RemoteLoopMode::Single => ("Track", false),
        RemoteLoopMode::List => ("Playlist", false),
        RemoteLoopMode::Shuffle => ("Playlist", true),
    }
}

/// Maps MPRIS `LoopStatus` string (`"None"`, `"Track"`, `"Playlist"`) to [`RemoteLoopMode`].
#[allow(dead_code)]
pub(crate) fn mpris_loop_from_status(status: &str) -> Option<RemoteLoopMode> {
    match status {
        "None" => Some(RemoteLoopMode::Sequence),
        "Track" => Some(RemoteLoopMode::Single),
        "Playlist" => Some(RemoteLoopMode::List),
        _ => None,
    }
}

/// Encodes an arbitrary `track_id` into a strictly valid D-Bus object path
/// under `/org/mpris/MediaPlayer2/Track/`.
#[allow(dead_code)]
pub(crate) fn mpris_track_object_path(track_id: &str) -> String {
    if track_id.is_empty() {
        return "/org/mpris/MediaPlayer2/Track/empty".to_owned();
    }
    let mut out = String::from("/org/mpris/MediaPlayer2/Track/");
    for byte in track_id.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "_{byte:02x}");
        }
    }
    out
}

/// Normalizes an artwork URL or filesystem path into an MPRIS `mpris:artUrl` URI.
#[allow(dead_code)]
pub(crate) fn mpris_art_url(artwork_url: &str) -> Option<String> {
    let trimmed = artwork_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
    {
        return Some(trimmed.to_owned());
    }
    if trimmed.starts_with('/') {
        return url::Url::from_file_path(trimmed)
            .ok()
            .map(|u| u.to_string());
    }
    None
}

/// Converts milliseconds to MPRIS microseconds (`i64`).
#[allow(dead_code)]
pub(crate) const fn ms_to_us(ms: u64) -> i64 {
    let us = ms.saturating_mul(1_000);
    if us > i64::MAX as u64 {
        i64::MAX
    } else {
        us as i64
    }
}

/// Converts MPRIS microseconds (`i64`) to milliseconds (clamping negative to `0`).
#[allow(dead_code)]
pub(crate) const fn us_to_ms(us: i64) -> u64 {
    if us <= 0 { 0 } else { (us as u64) / 1_000 }
}

/// Clamps an MPRIS2 `Volume` (`0.0..=1.0`) to `(clamped_f64, percent_u8)`.
/// Non-finite values are rejected (`None`).
#[allow(dead_code)]
pub(crate) fn mpris_volume_to_percent(raw: f64) -> Option<(f64, u8)> {
    if !raw.is_finite() {
        return None;
    }
    let clamped = raw.clamp(0.0, 1.0);
    let pct = (clamped * 100.0).round() as u8;
    Some((clamped, pct))
}

// ---------------------------------------------------------------------------
// Android MediaSession mapping helpers
// ---------------------------------------------------------------------------

/// Maps [`RemotePlaybackStatus`] to `android.media.session.PlaybackState` constants:
/// `1 = STATE_STOPPED`, `2 = STATE_PAUSED`, `3 = STATE_PLAYING`, `6 = STATE_BUFFERING`.
#[allow(dead_code)]
pub(crate) const fn android_playback_state(status: RemotePlaybackStatus) -> i32 {
    match status {
        RemotePlaybackStatus::Stopped => 1,
        RemotePlaybackStatus::Paused => 2,
        RemotePlaybackStatus::Playing => 3,
        RemotePlaybackStatus::Buffering => 6,
    }
}

/// Maps [`NowPlayingMetadata`] to `PlaybackState` playback speed (`f32`).
#[allow(dead_code)]
pub(crate) fn android_playback_speed(meta: &NowPlayingMetadata) -> f32 {
    if meta.status.is_playing() {
        if meta.playback_rate_milli > 0 {
            (meta.playback_rate_milli as f32 / 1000.0).max(0.1)
        } else {
            1.0
        }
    } else {
        0.0
    }
}

/// Computes the `android.media.session.PlaybackState` supported actions bitmask.
#[allow(dead_code)]
pub(crate) const fn android_playback_actions(meta: &NowPlayingMetadata) -> i64 {
    const ACTION_STOP: i64 = 1;
    const ACTION_PAUSE: i64 = 2;
    const ACTION_PLAY: i64 = 4;
    const ACTION_SKIP_TO_PREVIOUS: i64 = 16;
    const ACTION_SKIP_TO_NEXT: i64 = 32;
    const ACTION_SEEK_TO: i64 = 256;
    const ACTION_PLAY_PAUSE: i64 = 512;

    let mut actions = ACTION_PLAY | ACTION_PAUSE | ACTION_PLAY_PAUSE | ACTION_STOP;
    if meta.duration_ms > 0 {
        actions |= ACTION_SEEK_TO;
    }
    if meta.can_next {
        actions |= ACTION_SKIP_TO_NEXT;
    }
    if meta.can_prev {
        actions |= ACTION_SKIP_TO_PREVIOUS;
    }
    actions
}

#[cfg(test)]
mod tests {
    use nsh_music_audio::{
        NowPlayingMetadata, RemoteBackend, RemoteController, RemoteLoopMode, RemotePlaybackStatus,
    };

    use super::*;

    #[test]
    fn install_platform_remote_binds_expected_backend_and_is_headless_safe() {
        let remote = RemoteController::new();
        install_platform_remote(&remote);

        #[cfg(target_os = "macos")]
        assert_eq!(remote.backend(), RemoteBackend::MacOsNowPlaying);
        #[cfg(target_os = "ios")]
        assert_eq!(remote.backend(), RemoteBackend::IosControlCenter);
        #[cfg(target_os = "windows")]
        assert_eq!(remote.backend(), RemoteBackend::WindowsSmtc);
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        assert_eq!(remote.backend(), RemoteBackend::LinuxMpris);
        #[cfg(target_os = "android")]
        assert_eq!(remote.backend(), RemoteBackend::AndroidMediaSession);
        #[cfg(target_env = "ohos")]
        assert_eq!(remote.backend(), RemoteBackend::HarmonyAvSession);

        // Exercise metadata, playback, artwork, lyric, favorite, loop mode, and clear
        // in a headless test process without hanging or crashing.
        let meta = NowPlayingMetadata {
            track_id: "track-42".into(),
            title: "Midnight City".into(),
            artist: "M83".into(),
            album: "Hurry Up, We're Dreaming".into(),
            artwork_url: "https://example.com/cover.jpg".into(),
            duration_ms: 243_000,
            position_ms: 15_000,
            is_playing: true,
            can_next: true,
            can_prev: true,
            playback_rate_milli: 1000,
            status: RemotePlaybackStatus::Playing,
            buffered_ms: 60_000,
            is_favorite: false,
            loop_mode: RemoteLoopMode::List,
            lyric: None,
        };
        remote.set_metadata(meta);
        remote.update_playback_state(true, 30_000, 243_000);
        remote.update_artwork("track-42", "file:///tmp/nonexistent-nsh-cover.png");
        remote.update_lyric("track-42", "[00:01.00]Waiting in a car");
        remote.update_favorite("track-42", true);
        remote.update_loop_mode(RemoteLoopMode::Shuffle);

        let snap = remote.metadata();
        assert_eq!(snap.track_id, "track-42");
        assert_eq!(snap.position_ms, 30_000);
        assert!(snap.is_favorite);
        assert_eq!(snap.loop_mode, RemoteLoopMode::Shuffle);
        assert_eq!(snap.lyric.as_deref(), Some("[00:01.00]Waiting in a car"));

        remote.clear();
        assert!(remote.metadata().track_id.is_empty());
    }

    #[test]
    fn apple_mapping_helpers_cover_playback_rate_repeat_and_shuffle() {
        assert_eq!(apple_playback_state(RemotePlaybackStatus::Playing), 1);
        assert_eq!(apple_playback_state(RemotePlaybackStatus::Paused), 2);
        assert_eq!(apple_playback_state(RemotePlaybackStatus::Buffering), 2);
        assert_eq!(apple_playback_state(RemotePlaybackStatus::Stopped), 3);

        let mut meta = NowPlayingMetadata {
            status: RemotePlaybackStatus::Playing,
            playback_rate_milli: 1000,
            ..NowPlayingMetadata::default()
        };
        assert!((apple_playback_rate(&meta) - 1.0).abs() < 1e-6);
        meta.playback_rate_milli = 1500;
        assert!((apple_playback_rate(&meta) - 1.5).abs() < 1e-6);
        meta.status = RemotePlaybackStatus::Paused;
        assert_eq!(apple_playback_rate(&meta), 0.0);

        assert_eq!(
            apple_repeat_shuffle_from_loop(RemoteLoopMode::Sequence),
            (0, 0)
        );
        assert_eq!(
            apple_repeat_shuffle_from_loop(RemoteLoopMode::Single),
            (1, 0)
        );
        assert_eq!(apple_repeat_shuffle_from_loop(RemoteLoopMode::List), (2, 0));
        assert_eq!(
            apple_repeat_shuffle_from_loop(RemoteLoopMode::Shuffle),
            (2, 1)
        );

        assert_eq!(apple_loop_from_repeat(0), RemoteLoopMode::Sequence);
        assert_eq!(apple_loop_from_repeat(1), RemoteLoopMode::Single);
        assert_eq!(apple_loop_from_repeat(2), RemoteLoopMode::List);
        assert_eq!(
            apple_loop_from_shuffle(1, RemoteLoopMode::Sequence),
            RemoteLoopMode::Shuffle
        );
        assert_eq!(
            apple_loop_from_shuffle(0, RemoteLoopMode::Shuffle),
            RemoteLoopMode::Sequence
        );
        assert_eq!(
            apple_loop_from_shuffle(0, RemoteLoopMode::Single),
            RemoteLoopMode::Single
        );
    }

    #[test]
    fn smtc_mapping_helpers_cover_status_repeat_shuffle_and_timespan() {
        assert_eq!(smtc_playback_status(RemotePlaybackStatus::Buffering), 1);
        assert_eq!(smtc_playback_status(RemotePlaybackStatus::Stopped), 2);
        assert_eq!(smtc_playback_status(RemotePlaybackStatus::Playing), 3);
        assert_eq!(smtc_playback_status(RemotePlaybackStatus::Paused), 4);

        assert_eq!(smtc_repeat_shuffle(RemoteLoopMode::Sequence), (0, false));
        assert_eq!(smtc_repeat_shuffle(RemoteLoopMode::Single), (1, false));
        assert_eq!(smtc_repeat_shuffle(RemoteLoopMode::List), (2, false));
        assert_eq!(smtc_repeat_shuffle(RemoteLoopMode::Shuffle), (2, true));

        assert_eq!(smtc_loop_from_repeat(0), RemoteLoopMode::Sequence);
        assert_eq!(smtc_loop_from_repeat(1), RemoteLoopMode::Single);
        assert_eq!(smtc_loop_from_repeat(2), RemoteLoopMode::List);
        assert_eq!(
            smtc_loop_from_shuffle(true, RemoteLoopMode::List),
            RemoteLoopMode::Shuffle
        );
        assert_eq!(
            smtc_loop_from_shuffle(false, RemoteLoopMode::Shuffle),
            RemoteLoopMode::Sequence
        );

        assert_eq!(ms_to_hns(1_250), 12_500_000);
        assert_eq!(hns_to_ms(12_500_000), 1_250);
        assert_eq!(hns_to_ms(-50_000), 0);
    }

    #[test]
    fn mpris_mapping_helpers_cover_status_loop_paths_art_urls_and_time() {
        assert_eq!(
            mpris_playback_status(RemotePlaybackStatus::Playing),
            "Playing"
        );
        assert_eq!(
            mpris_playback_status(RemotePlaybackStatus::Paused),
            "Paused"
        );
        assert_eq!(
            mpris_playback_status(RemotePlaybackStatus::Buffering),
            "Paused"
        );
        assert_eq!(
            mpris_playback_status(RemotePlaybackStatus::Stopped),
            "Stopped"
        );

        assert_eq!(
            mpris_loop_and_shuffle(RemoteLoopMode::Sequence),
            ("None", false)
        );
        assert_eq!(
            mpris_loop_and_shuffle(RemoteLoopMode::Single),
            ("Track", false)
        );
        assert_eq!(
            mpris_loop_and_shuffle(RemoteLoopMode::List),
            ("Playlist", false)
        );
        assert_eq!(
            mpris_loop_and_shuffle(RemoteLoopMode::Shuffle),
            ("Playlist", true)
        );

        assert_eq!(
            mpris_loop_from_status("None"),
            Some(RemoteLoopMode::Sequence)
        );
        assert_eq!(
            mpris_loop_from_status("Track"),
            Some(RemoteLoopMode::Single)
        );
        assert_eq!(
            mpris_loop_from_status("Playlist"),
            Some(RemoteLoopMode::List)
        );
        assert_eq!(mpris_loop_from_status("Unknown"), None);

        let path = mpris_track_object_path("yt:abc-123/45");
        assert!(path.starts_with("/org/mpris/MediaPlayer2/Track/"));
        assert!(
            path.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_')
        );
        assert_eq!(
            mpris_track_object_path(""),
            "/org/mpris/MediaPlayer2/Track/empty"
        );

        assert_eq!(
            mpris_art_url("https://cdn.example/a.jpg").as_deref(),
            Some("https://cdn.example/a.jpg")
        );
        assert_eq!(
            mpris_art_url("/tmp/my cover.png").as_deref(),
            Some("file:///tmp/my%20cover.png")
        );
        assert_eq!(mpris_art_url(""), None);

        assert_eq!(ms_to_us(3_500), 3_500_000);
        assert_eq!(us_to_ms(3_500_000), 3_500);
        assert_eq!(us_to_ms(-100), 0);

        assert_eq!(mpris_volume_to_percent(0.0), Some((0.0, 0)));
        assert_eq!(mpris_volume_to_percent(0.456), Some((0.456, 46)));
        assert_eq!(mpris_volume_to_percent(1.0), Some((1.0, 100)));
        assert_eq!(mpris_volume_to_percent(-0.25), Some((0.0, 0)));
        assert_eq!(mpris_volume_to_percent(1.8), Some((1.0, 100)));
        assert_eq!(mpris_volume_to_percent(f64::NAN), None);
        assert_eq!(mpris_volume_to_percent(f64::INFINITY), None);
    }

    #[test]
    fn android_mapping_and_artwork_local_path_helpers() {
        assert_eq!(android_playback_state(RemotePlaybackStatus::Stopped), 1);
        assert_eq!(android_playback_state(RemotePlaybackStatus::Paused), 2);
        assert_eq!(android_playback_state(RemotePlaybackStatus::Playing), 3);
        assert_eq!(android_playback_state(RemotePlaybackStatus::Buffering), 6);

        let meta = NowPlayingMetadata {
            status: RemotePlaybackStatus::Playing,
            playback_rate_milli: 1000,
            duration_ms: 180_000,
            can_next: true,
            can_prev: false,
            ..NowPlayingMetadata::default()
        };
        assert!((android_playback_speed(&meta) - 1.0).abs() < 1e-6);
        let actions = android_playback_actions(&meta);
        assert_ne!(actions & 4, 0); // ACTION_PLAY
        assert_ne!(actions & 2, 0); // ACTION_PAUSE
        assert_ne!(actions & 32, 0); // ACTION_SKIP_TO_NEXT
        assert_eq!(actions & 16, 0); // ACTION_SKIP_TO_PREVIOUS
        assert_ne!(actions & 256, 0); // ACTION_SEEK_TO

        assert_eq!(
            artwork_local_path("file:///tmp/cover.png"),
            Some(PathBuf::from("/tmp/cover.png"))
        );
        assert_eq!(
            artwork_local_path("/var/cache/art.jpg"),
            Some(PathBuf::from("/var/cache/art.jpg"))
        );
        assert_eq!(artwork_local_path("https://cdn.example/art.jpg"), None);
        assert_eq!(artwork_local_path(""), None);

        assert_eq!(
            artwork_http_url("https://cdn.example/art.jpg"),
            Some("https://cdn.example/art.jpg")
        );
        assert_eq!(
            artwork_http_url("  http://cdn.example/art.png  "),
            Some("http://cdn.example/art.png")
        );
        assert_eq!(artwork_http_url("file:///tmp/cover.png"), None);
        assert_eq!(artwork_http_url("/var/cache/art.jpg"), None);
        assert_eq!(artwork_http_url(""), None);
    }
}
