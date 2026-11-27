//! Native Apple (`macOS` / `iOS`) media control driver backed by
//! `MediaPlayer.framework` (`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`).
//!
//! - Dispatches all MediaPlayer mutations onto `DispatchQueue::main()` with
//!   coalesced state updates so high-frequency playback ticks never queue up.
//! - Loads local/cached artwork bytes off-thread before constructing
//!   `NSImage` (macOS) / `UIImage` (iOS) + `MPMediaItemArtwork` on the main queue.
//! - Checks `NSApp.is_null()` on macOS so headless `cargo test` runs never
//!   touch AppKit, `DispatchQueue::main()`, or `MediaPlayer`.
//! - Activates `AVAudioSession` playback category on iOS when playback begins.

#![cfg(any(target_os = "macos", target_os = "ios"))]

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AnyThread;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
#[cfg(target_os = "macos")]
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString};
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIImage;

use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteController,
    RemotePlaybackStatus,
};

use super::{
    apple_loop_from_repeat, apple_loop_from_shuffle, apple_playback_rate, apple_playback_state,
    apple_repeat_shuffle_from_loop, artwork_local_path,
};

/// Maximum local artwork file size read into memory for OS Now Playing surfaces (16 MiB).
const MAX_ARTWORK_BYTES: u64 = 16 * 1024 * 1024;

static NEXT_DRIVER_ID: AtomicU64 = AtomicU64::new(1);

#[link(name = "MediaPlayer", kind = "framework")]
unsafe extern "C" {
    static MPMediaItemPropertyTitle: *const NSString;
    static MPMediaItemPropertyArtist: *const NSString;
    static MPMediaItemPropertyAlbumTitle: *const NSString;
    static MPMediaItemPropertyPlaybackDuration: *const NSString;
    static MPMediaItemPropertyArtwork: *const NSString;
    static MPNowPlayingInfoPropertyElapsedPlaybackTime: *const NSString;
    static MPNowPlayingInfoPropertyPlaybackRate: *const NSString;
    static MPNowPlayingInfoPropertyDefaultPlaybackRate: *const NSString;
}

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSApp: *const AnyObject;
}

#[cfg(target_os = "ios")]
#[link(name = "AVFAudio", kind = "framework")]
unsafe extern "C" {}

#[cfg(target_os = "ios")]
#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {}

fn is_gui_active() -> bool {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `NSApp` is AppKit's global `NSApplication *` pointer (null in headless tests).
        !unsafe { NSApp }.is_null()
    }
    #[cfg(target_os = "ios")]
    {
        AnyClass::get(c"MPNowPlayingInfoCenter").is_some()
    }
}

#[derive(Default)]
struct PendingState {
    metadata: Option<NowPlayingMetadata>,
    clear_requested: bool,
    artwork_key: Option<String>,
    artwork_bytes: Option<Arc<Vec<u8>>>,
    artwork_loading_key: Option<String>,
    alive: bool,
}

struct SharedState {
    driver_id: u64,
    remote: RemoteController,
    pending: Mutex<PendingState>,
    scheduled: AtomicBool,
}

struct ScheduleResetGuard {
    shared: Arc<SharedState>,
    armed: bool,
}

impl ScheduleResetGuard {
    fn new(shared: Arc<SharedState>) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScheduleResetGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.scheduled.store(false, Ordering::Release);
        }
    }
}

/// Native macOS / iOS implementation of [`PlatformRemoteDriver`].
pub(crate) struct AppleNowPlayingDriver {
    shared: Arc<SharedState>,
}

impl AppleNowPlayingDriver {
    pub(crate) fn new(remote: RemoteController) -> Self {
        Self {
            shared: Arc::new(SharedState {
                driver_id: NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed),
                remote,
                pending: Mutex::new(PendingState {
                    alive: true,
                    ..PendingState::default()
                }),
                scheduled: AtomicBool::new(false),
            }),
        }
    }

    fn maybe_load_artwork(&self, artwork_url: &str) {
        if !is_gui_active() {
            return;
        }
        let Some(local_path) = artwork_local_path(artwork_url) else {
            return;
        };
        let key = artwork_url.to_owned();
        let should_spawn = {
            let Ok(mut pending) = self.shared.pending.lock() else {
                return;
            };
            if !pending.alive
                || pending.artwork_key.as_deref() == Some(key.as_str())
                || pending.artwork_loading_key.as_deref() == Some(key.as_str())
            {
                false
            } else {
                pending.artwork_loading_key = Some(key.clone());
                true
            }
        };
        if !should_spawn {
            return;
        }

        let shared = Arc::clone(&self.shared);
        let _ = std::thread::Builder::new()
            .name("nsh-apple-artwork".into())
            .spawn(move || load_artwork_bytes_off_thread(shared, key, local_path));
    }

    fn schedule_flush(&self) {
        schedule_flush_shared(&self.shared);
    }
}

fn schedule_flush_shared(shared: &Arc<SharedState>) {
    if !is_gui_active() {
        return;
    }
    if shared.scheduled.swap(true, Ordering::AcqRel) {
        return;
    }
    let shared_clone = Arc::clone(shared);
    let mut guard = ScheduleResetGuard::new(Arc::clone(shared));
    DispatchQueue::main().exec_async(move || {
        guard.disarm();
        flush_on_main_thread(shared_clone);
    });
}

fn load_artwork_bytes_off_thread(shared: Arc<SharedState>, key: String, path: PathBuf) {
    let bytes = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_ARTWORK_BYTES)
        .and_then(|_| std::fs::read(&path).ok());

    let should_flush = {
        let Ok(mut pending) = shared.pending.lock() else {
            return;
        };
        if pending.artwork_loading_key.as_deref() == Some(key.as_str()) {
            pending.artwork_loading_key = None;
        }
        if !pending.alive {
            false
        } else if let Some(data) = bytes {
            pending.artwork_key = Some(key);
            pending.artwork_bytes = Some(Arc::new(data));
            true
        } else {
            false
        }
    };
    if should_flush {
        schedule_flush_shared(&shared);
    }
}

impl PlatformRemoteDriver for AppleNowPlayingDriver {
    fn backend(&self) -> RemoteBackend {
        #[cfg(target_os = "macos")]
        {
            RemoteBackend::MacOsNowPlaying
        }
        #[cfg(target_os = "ios")]
        {
            RemoteBackend::IosControlCenter
        }
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            if pending.artwork_key.as_deref() != Some(meta.artwork_url.as_str())
                && artwork_local_path(&meta.artwork_url).is_none()
            {
                pending.artwork_key = None;
                pending.artwork_bytes = None;
            }
            pending.metadata = Some(meta.clone());
            pending.clear_requested = false;
        }
        self.maybe_load_artwork(&meta.artwork_url);
        self.schedule_flush();
    }

    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            let meta = pending
                .metadata
                .get_or_insert_with(|| self.shared.remote.metadata());
            meta.is_playing = is_playing;
            meta.status = if is_playing {
                RemotePlaybackStatus::Playing
            } else {
                RemotePlaybackStatus::Paused
            };
            meta.position_ms = position_ms;
            if duration_ms > 0 {
                meta.duration_ms = duration_ms;
            }
            meta.playback_rate_milli = if is_playing { 1000 } else { 0 };
            pending.clear_requested = false;
        }
        self.schedule_flush();
    }

    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = Some(meta.clone());
            pending.clear_requested = false;
        }
        self.maybe_load_artwork(&meta.artwork_url);
        self.schedule_flush();
    }

    fn clear(&self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = None;
            pending.clear_requested = true;
            pending.artwork_key = None;
            pending.artwork_bytes = None;
            pending.artwork_loading_key = None;
        }
        self.schedule_flush();
    }
}

impl Drop for AppleNowPlayingDriver {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.alive = false;
            pending.metadata = None;
            pending.clear_requested = true;
            pending.artwork_key = None;
            pending.artwork_bytes = None;
            pending.artwork_loading_key = None;
        }
        if is_gui_active() {
            let driver_id = self.shared.driver_id;
            DispatchQueue::main().exec_async(move || {
                teardown_session_on_main_thread(Some(driver_id));
            });
        }
    }
}

struct MainThreadSession {
    driver_id: u64,
    targets: Vec<(Retained<AnyObject>, Retained<AnyObject>)>,
    cached_artwork_key: Option<String>,
    cached_artwork: Option<Retained<AnyObject>>,
    #[cfg(target_os = "ios")]
    audio_session_activated: bool,
}

thread_local! {
    static MAIN_SESSION: RefCell<Option<MainThreadSession>> = const { RefCell::new(None) };
}

fn flush_on_main_thread(shared: Arc<SharedState>) {
    let snapshot = {
        let Ok(mut pending) = shared.pending.lock() else {
            shared.scheduled.store(false, Ordering::Release);
            return;
        };
        shared.scheduled.store(false, Ordering::Release);
        if !pending.alive || pending.clear_requested {
            pending.clear_requested = false;
            None
        } else {
            pending.metadata.clone().map(|meta| {
                let art = if pending.artwork_key.as_deref() == Some(meta.artwork_url.as_str()) {
                    pending
                        .artwork_key
                        .clone()
                        .zip(pending.artwork_bytes.clone())
                } else {
                    None
                };
                (meta, art)
            })
        }
    };

    let Some((meta, art_data)) = snapshot else {
        teardown_session_on_main_thread(Some(shared.driver_id));
        return;
    };

    ensure_session_registered(shared.driver_id, &shared.remote);

    #[cfg(target_os = "ios")]
    if meta.status.is_playing() {
        MAIN_SESSION.with(|cell| {
            if let Some(sess) = cell.borrow_mut().as_mut()
                && !sess.audio_session_activated
            {
                sess.audio_session_activated = activate_ios_audio_session();
            }
        });
    }

    let artwork = MAIN_SESSION.with(|cell| {
        let mut guard = cell.borrow_mut();
        let sess = guard.as_mut()?;
        match art_data {
            Some((key, bytes)) => {
                if sess.cached_artwork_key.as_deref() != Some(key.as_str()) {
                    sess.cached_artwork = build_media_item_artwork(&bytes);
                    sess.cached_artwork_key = Some(key);
                }
                sess.cached_artwork.clone()
            }
            None => {
                if sess.cached_artwork_key.as_deref() != Some(meta.artwork_url.as_str()) {
                    sess.cached_artwork = None;
                    sess.cached_artwork_key = None;
                }
                sess.cached_artwork.clone()
            }
        }
    });

    update_remote_command_states(&meta);
    update_now_playing_info_center(&meta, artwork.as_deref());
}

fn ensure_session_registered(driver_id: u64, remote: &RemoteController) {
    let already_active = MAIN_SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|s| s.driver_id == driver_id)
    });
    if already_active {
        return;
    }

    // Remove any targets left over from a previous driver instance.
    teardown_session_on_main_thread(None);

    let Some(center) = shared_command_center() else {
        return;
    };

    let mut targets = Vec::new();

    register_simple_command(
        &center,
        "playCommand",
        remote,
        RemoteCommand::Play,
        &mut targets,
    );
    register_simple_command(
        &center,
        "pauseCommand",
        remote,
        RemoteCommand::Pause,
        &mut targets,
    );
    register_simple_command(
        &center,
        "togglePlayPauseCommand",
        remote,
        RemoteCommand::TogglePlayPause,
        &mut targets,
    );
    register_simple_command(
        &center,
        "stopCommand",
        remote,
        RemoteCommand::Stop,
        &mut targets,
    );
    register_simple_command(
        &center,
        "nextTrackCommand",
        remote,
        RemoteCommand::Next,
        &mut targets,
    );
    register_simple_command(
        &center,
        "previousTrackCommand",
        remote,
        RemoteCommand::Prev,
        &mut targets,
    );
    register_simple_command(
        &center,
        "likeCommand",
        remote,
        RemoteCommand::ToggleFavorite,
        &mut targets,
    );

    // Seek via scrubber (`MPChangePlaybackPositionCommandEvent.positionTime`).
    if let Some(cmd) = command_property(&center, "changePlaybackPositionCommand") {
        let remote_cb = remote.clone();
        let block = RcBlock::new(move |event_ptr: *mut AnyObject| -> isize {
            if let Some(event) = unsafe { event_ptr.as_ref() } {
                let secs: f64 = unsafe { msg_send![event, positionTime] };
                if secs.is_finite() {
                    let ms = (secs.max(0.0) * 1000.0).round() as u64;
                    remote_cb.dispatch(RemoteCommand::Seek(ms));
                }
            }
            0
        });
        unsafe {
            let _: () = msg_send![&cmd, setEnabled: Bool::YES];
            let target: Option<Retained<AnyObject>> =
                msg_send![&cmd, addTargetWithHandler: &*block];
            if let Some(t) = target {
                targets.push((cmd, t));
            }
        }
    }

    // Repeat mode (`MPChangeRepeatModeCommandEvent.repeatType`).
    if let Some(cmd) = command_property(&center, "changeRepeatModeCommand") {
        let remote_cb = remote.clone();
        let block = RcBlock::new(move |event_ptr: *mut AnyObject| -> isize {
            if let Some(event) = unsafe { event_ptr.as_ref() } {
                let repeat_type: isize = unsafe { msg_send![event, repeatType] };
                remote_cb.dispatch(RemoteCommand::SetLoopMode(apple_loop_from_repeat(
                    repeat_type,
                )));
            }
            0
        });
        unsafe {
            let _: () = msg_send![&cmd, setEnabled: Bool::YES];
            let target: Option<Retained<AnyObject>> =
                msg_send![&cmd, addTargetWithHandler: &*block];
            if let Some(t) = target {
                targets.push((cmd, t));
            }
        }
    }

    // Shuffle mode (`MPChangeShuffleModeCommandEvent.shuffleType`).
    if let Some(cmd) = command_property(&center, "changeShuffleModeCommand") {
        let remote_cb = remote.clone();
        let block = RcBlock::new(move |event_ptr: *mut AnyObject| -> isize {
            if let Some(event) = unsafe { event_ptr.as_ref() } {
                let shuffle_type: isize = unsafe { msg_send![event, shuffleType] };
                let current = remote_cb.metadata().loop_mode;
                remote_cb.dispatch(RemoteCommand::SetLoopMode(apple_loop_from_shuffle(
                    shuffle_type,
                    current,
                )));
            }
            0
        });
        unsafe {
            let _: () = msg_send![&cmd, setEnabled: Bool::YES];
            let target: Option<Retained<AnyObject>> =
                msg_send![&cmd, addTargetWithHandler: &*block];
            if let Some(t) = target {
                targets.push((cmd, t));
            }
        }
    }

    MAIN_SESSION.with(|cell| {
        *cell.borrow_mut() = Some(MainThreadSession {
            driver_id,
            targets,
            cached_artwork_key: None,
            cached_artwork: None,
            #[cfg(target_os = "ios")]
            audio_session_activated: false,
        });
    });
}

fn register_simple_command(
    center: &AnyObject,
    prop_name: &str,
    remote: &RemoteController,
    cmd_event: RemoteCommand,
    targets: &mut Vec<(Retained<AnyObject>, Retained<AnyObject>)>,
) {
    let Some(cmd) = command_property(center, prop_name) else {
        return;
    };
    let remote_cb = remote.clone();
    let block = RcBlock::new(move |_event_ptr: *mut AnyObject| -> isize {
        remote_cb.dispatch(cmd_event);
        0 // MPRemoteCommandHandlerStatusSuccess
    });
    unsafe {
        let _: () = msg_send![&cmd, setEnabled: Bool::YES];
        let target: Option<Retained<AnyObject>> = msg_send![&cmd, addTargetWithHandler: &*block];
        if let Some(t) = target {
            targets.push((cmd, t));
        }
    }
}

fn shared_command_center() -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"MPRemoteCommandCenter")?;
    unsafe { msg_send![cls, sharedCommandCenter] }
}

fn command_property(center: &AnyObject, prop_name: &str) -> Option<Retained<AnyObject>> {
    unsafe {
        match prop_name {
            "playCommand" => msg_send![center, playCommand],
            "pauseCommand" => msg_send![center, pauseCommand],
            "togglePlayPauseCommand" => msg_send![center, togglePlayPauseCommand],
            "stopCommand" => msg_send![center, stopCommand],
            "nextTrackCommand" => msg_send![center, nextTrackCommand],
            "previousTrackCommand" => msg_send![center, previousTrackCommand],
            "changePlaybackPositionCommand" => msg_send![center, changePlaybackPositionCommand],
            "likeCommand" => msg_send![center, likeCommand],
            "changeRepeatModeCommand" => msg_send![center, changeRepeatModeCommand],
            "changeShuffleModeCommand" => msg_send![center, changeShuffleModeCommand],
            _ => None,
        }
    }
}

fn update_remote_command_states(meta: &NowPlayingMetadata) {
    let Some(center) = shared_command_center() else {
        return;
    };
    unsafe {
        if let Some(cmd) = command_property(&center, "nextTrackCommand") {
            let _: () = msg_send![&cmd, setEnabled: Bool::new(meta.can_next)];
        }
        if let Some(cmd) = command_property(&center, "previousTrackCommand") {
            let _: () = msg_send![&cmd, setEnabled: Bool::new(meta.can_prev)];
        }
        if let Some(cmd) = command_property(&center, "changePlaybackPositionCommand") {
            let _: () = msg_send![&cmd, setEnabled: Bool::new(meta.duration_ms > 0)];
        }
        if let Some(cmd) = command_property(&center, "likeCommand") {
            let _: () = msg_send![&cmd, setEnabled: Bool::YES];
            let _: () = msg_send![&cmd, setActive: Bool::new(meta.is_favorite)];
        }
        let (repeat_type, shuffle_type) = apple_repeat_shuffle_from_loop(meta.loop_mode);
        if let Some(cmd) = command_property(&center, "changeRepeatModeCommand") {
            let _: () = msg_send![&cmd, setCurrentRepeatType: repeat_type];
        }
        if let Some(cmd) = command_property(&center, "changeShuffleModeCommand") {
            let _: () = msg_send![&cmd, setCurrentShuffleType: shuffle_type];
        }
    }
}

fn insert_dict_entry(
    dict: &NSMutableDictionary<NSString, AnyObject>,
    key_ptr: *const NSString,
    value: &AnyObject,
) {
    // SAFETY: `key_ptr` is an `MP_EXTERN NSString * const` constant from `MediaPlayer.framework`.
    if let Some(key) = unsafe { key_ptr.as_ref() } {
        dict.insert(key, value);
    }
}

fn update_now_playing_info_center(meta: &NowPlayingMetadata, artwork: Option<&AnyObject>) {
    let Some(center_cls) = AnyClass::get(c"MPNowPlayingInfoCenter") else {
        return;
    };
    let center: Option<Retained<AnyObject>> = unsafe { msg_send![center_cls, defaultCenter] };
    let Some(center) = center else {
        return;
    };

    let dict = NSMutableDictionary::<NSString, AnyObject>::dictionary();
    let title = NSString::from_str(&meta.title);
    unsafe {
        insert_dict_entry(&dict, MPMediaItemPropertyTitle, title.as_ref());

        if !meta.artist.is_empty() {
            let artist = NSString::from_str(&meta.artist);
            insert_dict_entry(&dict, MPMediaItemPropertyArtist, artist.as_ref());
        }
        if !meta.album.is_empty() {
            let album = NSString::from_str(&meta.album);
            insert_dict_entry(&dict, MPMediaItemPropertyAlbumTitle, album.as_ref());
        }

        let duration = NSNumber::new_f64(meta.duration_ms as f64 / 1000.0);
        insert_dict_entry(
            &dict,
            MPMediaItemPropertyPlaybackDuration,
            duration.as_ref(),
        );

        let elapsed = NSNumber::new_f64(meta.position_ms as f64 / 1000.0);
        insert_dict_entry(
            &dict,
            MPNowPlayingInfoPropertyElapsedPlaybackTime,
            elapsed.as_ref(),
        );

        let rate = NSNumber::new_f64(apple_playback_rate(meta));
        insert_dict_entry(&dict, MPNowPlayingInfoPropertyPlaybackRate, rate.as_ref());

        let default_rate = NSNumber::new_f64(1.0);
        insert_dict_entry(
            &dict,
            MPNowPlayingInfoPropertyDefaultPlaybackRate,
            default_rate.as_ref(),
        );

        if let Some(art) = artwork {
            insert_dict_entry(&dict, MPMediaItemPropertyArtwork, art);
        }

        let _: () = msg_send![&center, setNowPlayingInfo: &*dict];
        let state = apple_playback_state(meta.status);
        let _: () = msg_send![&center, setPlaybackState: state];
    }
}

fn build_media_item_artwork(bytes: &[u8]) -> Option<Retained<AnyObject>> {
    let artwork_cls = AnyClass::get(c"MPMediaItemArtwork")?;
    let data = NSData::with_bytes(bytes);

    #[cfg(target_os = "macos")]
    let (img, size) = {
        let img = NSImage::initWithData(NSImage::alloc(), &data)?;
        let sz = img.size();
        (img, CGSize::new(sz.width, sz.height))
    };

    #[cfg(target_os = "ios")]
    let (img, size) = {
        let img = UIImage::initWithData(UIImage::alloc(), &data)?;
        let sz = unsafe { img.size() };
        (img, sz)
    };

    let bounds = if size.width > 0.0 && size.height > 0.0 {
        size
    } else {
        CGSize::new(512.0, 512.0)
    };

    let handler = RcBlock::new(move |_requested: CGSize| -> *mut AnyObject {
        Retained::autorelease_return(img.clone()).cast::<AnyObject>()
    });

    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![artwork_cls, alloc];
        msg_send![alloc, initWithBoundsSize: bounds, requestHandler: &*handler]
    }
}

fn teardown_session_on_main_thread(expected_driver_id: Option<u64>) {
    let removed = MAIN_SESSION.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(expected) = expected_driver_id
            && guard.as_ref().is_none_or(|s| s.driver_id != expected)
        {
            return None;
        }
        guard.take()
    });

    if let Some(sess) = removed {
        for (cmd, target) in sess.targets {
            unsafe {
                let _: () = msg_send![&cmd, removeTarget: &*target];
                let _: () = msg_send![&cmd, setEnabled: Bool::NO];
            }
        }
    }

    if let Some(center_cls) = AnyClass::get(c"MPNowPlayingInfoCenter") {
        unsafe {
            let center: Option<Retained<AnyObject>> = msg_send![center_cls, defaultCenter];
            if let Some(center) = center {
                let _: () = msg_send![&center, setNowPlayingInfo: std::ptr::null::<AnyObject>()];
                let _: () = msg_send![&center, setPlaybackState: 3usize]; // Stopped
            }
        }
    }
}

#[cfg(target_os = "ios")]
fn activate_ios_audio_session() -> bool {
    let Some(cls) = AnyClass::get(c"AVAudioSession") else {
        return false;
    };
    unsafe {
        let session: Option<Retained<AnyObject>> = msg_send![cls, sharedInstance];
        let Some(session) = session else {
            return false;
        };
        let category = NSString::from_str("AVAudioSessionCategoryPlayback");
        let mut cat_err: *mut AnyObject = std::ptr::null_mut();
        let ok_cat: Bool = msg_send![&session, setCategory: &*category, error: &mut cat_err];
        if !ok_cat.as_bool() {
            log::warn!("[remote:ios] AVAudioSession setCategory playback failed");
        }
        let mut act_err: *mut AnyObject = std::ptr::null_mut();
        let ok_act: Bool = msg_send![&session, setActive: Bool::YES, error: &mut act_err];
        if !ok_act.as_bool() {
            log::warn!("[remote:ios] AVAudioSession setActive failed");
        }
        ok_act.as_bool()
    }
}
