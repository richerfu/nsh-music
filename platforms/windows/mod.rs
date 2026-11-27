//! Native Windows System Media Transport Controls (SMTC) driver.
//!
//! Spawns a dedicated MTA worker thread owning a silent WinRT
//! `Windows::Media::Playback::MediaPlayer` (with its automatic `CommandManager`
//! disabled) so SMTC works cleanly without requiring a Win32 `HWND`. Pushes
//! music display metadata, artwork thumbnails, playback status, timeline,
//! repeat/shuffle state, and forwards OS media button & scrubber events back to
//! [`RemoteController`].

#![cfg(target_os = "windows")]

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteController,
    RemotePlaybackStatus,
};
use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::Playback::MediaPlayer;
use windows::Media::{
    AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
    MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    ShuffleEnabledChangeRequestedEventArgs, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::StorageFile;
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows_core::{HSTRING, Ref};

use crate::{
    artwork_local_path, hns_to_ms, ms_to_hns, smtc_loop_from_repeat, smtc_loop_from_shuffle,
    smtc_playback_status, smtc_repeat_shuffle,
};

#[derive(Default)]
struct PendingState {
    metadata: Option<NowPlayingMetadata>,
    meta_dirty: bool,
    playback_dirty: bool,
    clear_requested: bool,
    shutdown: bool,
}

struct SharedState {
    remote: RemoteController,
    pending: Mutex<PendingState>,
    wake: Condvar,
}

/// Windows SMTC implementation of [`PlatformRemoteDriver`].
pub(crate) struct WindowsSmtcDriver {
    shared: Arc<SharedState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsSmtcDriver {
    pub(crate) fn new(remote: RemoteController) -> Self {
        let shared = Arc::new(SharedState {
            remote,
            pending: Mutex::new(PendingState::default()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("nsh-windows-smtc".into())
            .spawn(move || run_smtc_worker(worker_shared))
            .ok();
        Self {
            shared,
            worker: Mutex::new(handle),
        }
    }
}

impl PlatformRemoteDriver for WindowsSmtcDriver {
    fn backend(&self) -> RemoteBackend {
        RemoteBackend::WindowsSmtc
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = Some(meta.clone());
            pending.meta_dirty = true;
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
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
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
    }

    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            let meta_changed = pending.metadata.as_ref().is_none_or(|prev| {
                prev.track_id != meta.track_id
                    || prev.title != meta.title
                    || prev.artist != meta.artist
                    || prev.album != meta.album
                    || prev.artwork_url != meta.artwork_url
                    || prev.duration_ms != meta.duration_ms
                    || prev.can_next != meta.can_next
                    || prev.can_prev != meta.can_prev
            });
            pending.metadata = Some(meta.clone());
            if meta_changed {
                pending.meta_dirty = true;
            }
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
    }

    fn clear(&self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = None;
            pending.meta_dirty = false;
            pending.playback_dirty = false;
            pending.clear_requested = true;
            self.shared.wake.notify_one();
        }
    }
}

impl Drop for WindowsSmtcDriver {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.shutdown = true;
            self.shared.wake.notify_all();
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

fn run_smtc_worker(shared: Arc<SharedState>) {
    let ro_initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();

    let Ok(player) = MediaPlayer::new() else {
        log::warn!("[remote:windows-smtc] MediaPlayer::new failed");
        if ro_initialized {
            unsafe { RoUninitialize() };
        }
        return;
    };

    // Disable MediaPlayer's built-in automatic SMTC management so we own all state updates.
    if let Ok(cmd_mgr) = player.CommandManager() {
        let _ = cmd_mgr.SetIsEnabled(false);
    }

    let Ok(smtc) = player.SystemMediaTransportControls() else {
        log::warn!("[remote:windows-smtc] SystemMediaTransportControls unavailable");
        let _ = player.Close();
        if ro_initialized {
            unsafe { RoUninitialize() };
        }
        return;
    };

    let _ = smtc.SetIsEnabled(false);
    let _ = smtc.SetIsPlayEnabled(true);
    let _ = smtc.SetIsPauseEnabled(true);
    let _ = smtc.SetIsStopEnabled(true);
    let _ = smtc.SetIsNextEnabled(false);
    let _ = smtc.SetIsPreviousEnabled(false);

    // Subscribe to OS SMTC events.
    let remote_btn = shared.remote.clone();
    let btn_token = smtc
        .ButtonPressed(&TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  args: Ref<'_, SystemMediaTransportControlsButtonPressedEventArgs>| {
                if let Ok(args) = args.ok()
                    && let Ok(button) = args.Button()
                {
                    match button {
                        SystemMediaTransportControlsButton::Play => {
                            remote_btn.dispatch(RemoteCommand::Play);
                        }
                        SystemMediaTransportControlsButton::Pause => {
                            remote_btn.dispatch(RemoteCommand::Pause);
                        }
                        SystemMediaTransportControlsButton::Stop => {
                            remote_btn.dispatch(RemoteCommand::Stop);
                        }
                        SystemMediaTransportControlsButton::Next => {
                            remote_btn.dispatch(RemoteCommand::Next);
                        }
                        SystemMediaTransportControlsButton::Previous => {
                            remote_btn.dispatch(RemoteCommand::Prev);
                        }
                        _ => {}
                    }
                }
                Ok(())
            },
        ))
        .ok();

    let remote_seek = shared.remote.clone();
    let seek_token = smtc
        .PlaybackPositionChangeRequested(&TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  args: Ref<'_, PlaybackPositionChangeRequestedEventArgs>| {
                if let Ok(args) = args.ok()
                    && let Ok(ts) = args.RequestedPlaybackPosition()
                {
                    remote_seek.dispatch(RemoteCommand::Seek(hns_to_ms(ts.Duration)));
                }
                Ok(())
            },
        ))
        .ok();

    let remote_shuffle = shared.remote.clone();
    let shuffle_token = smtc
        .ShuffleEnabledChangeRequested(&TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  args: Ref<'_, ShuffleEnabledChangeRequestedEventArgs>| {
                if let Ok(args) = args.ok()
                    && let Ok(enabled) = args.RequestedShuffleEnabled()
                {
                    let current = remote_shuffle.metadata().loop_mode;
                    remote_shuffle.dispatch(RemoteCommand::SetLoopMode(smtc_loop_from_shuffle(
                        enabled, current,
                    )));
                }
                Ok(())
            },
        ))
        .ok();

    let remote_repeat = shared.remote.clone();
    let repeat_token = smtc
        .AutoRepeatModeChangeRequested(&TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  args: Ref<'_, AutoRepeatModeChangeRequestedEventArgs>| {
                if let Ok(args) = args.ok()
                    && let Ok(mode) = args.RequestedAutoRepeatMode()
                {
                    remote_repeat
                        .dispatch(RemoteCommand::SetLoopMode(smtc_loop_from_repeat(mode.0)));
                }
                Ok(())
            },
        ))
        .ok();

    let mut last_artwork_url = String::new();

    loop {
        let (meta_opt, meta_dirty, playback_dirty, clear_requested, shutdown) = {
            let Ok(mut pending) = shared.pending.lock() else {
                break;
            };
            while !pending.shutdown
                && !pending.clear_requested
                && !pending.meta_dirty
                && !pending.playback_dirty
            {
                let Ok(next) = shared.wake.wait(pending) else {
                    return;
                };
                pending = next;
            }
            let out = (
                pending.metadata.clone(),
                pending.meta_dirty,
                pending.playback_dirty,
                pending.clear_requested,
                pending.shutdown,
            );
            pending.meta_dirty = false;
            pending.playback_dirty = false;
            pending.clear_requested = false;
            out
        };

        if shutdown {
            break;
        }

        if clear_requested {
            clear_smtc(&smtc);
            last_artwork_url.clear();
            continue;
        }

        if let Some(meta) = meta_opt {
            if meta_dirty {
                push_metadata(&smtc, &meta, &mut last_artwork_url);
            }
            if playback_dirty {
                push_playback(&smtc, &meta);
            }
        }
    }

    if let Some(tok) = btn_token {
        let _ = smtc.RemoveButtonPressed(tok);
    }
    if let Some(tok) = seek_token {
        let _ = smtc.RemovePlaybackPositionChangeRequested(tok);
    }
    if let Some(tok) = shuffle_token {
        let _ = smtc.RemoveShuffleEnabledChangeRequested(tok);
    }
    if let Some(tok) = repeat_token {
        let _ = smtc.RemoveAutoRepeatModeChangeRequested(tok);
    }
    clear_smtc(&smtc);
    let _ = player.Close();

    if ro_initialized {
        unsafe { RoUninitialize() };
    }
}

fn push_metadata(
    smtc: &SystemMediaTransportControls,
    meta: &NowPlayingMetadata,
    last_artwork_url: &mut String,
) {
    let _ = smtc.SetIsEnabled(true);
    let _ = smtc.SetIsNextEnabled(meta.can_next);
    let _ = smtc.SetIsPreviousEnabled(meta.can_prev);

    let Ok(updater) = smtc.DisplayUpdater() else {
        return;
    };
    let _ = updater.SetType(MediaPlaybackType::Music);
    if let Ok(music) = updater.MusicProperties() {
        let _ = music.SetTitle(&HSTRING::from(meta.title.as_str()));
        let _ = music.SetArtist(&HSTRING::from(meta.artist.as_str()));
        let _ = music.SetAlbumTitle(&HSTRING::from(meta.album.as_str()));
    }

    if meta.artwork_url != *last_artwork_url {
        *last_artwork_url = meta.artwork_url.clone();
        if let Some(thumb) = build_thumbnail(&meta.artwork_url) {
            let _ = updater.SetThumbnail(&thumb);
        }
    }

    let _ = updater.Update();
}

fn build_thumbnail(artwork_url: &str) -> Option<RandomAccessStreamReference> {
    let trimmed = artwork_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(local_path) = artwork_local_path(trimmed) {
        let win_path = local_path.to_string_lossy().replace('/', "\\");
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(win_path.as_str()))
            .ok()?
            .join()
            .ok()?;
        return RandomAccessStreamReference::CreateFromFile(&file).ok();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let uri = Uri::CreateUri(&HSTRING::from(trimmed)).ok()?;
        return RandomAccessStreamReference::CreateFromUri(&uri).ok();
    }
    None
}

fn push_playback(smtc: &SystemMediaTransportControls, meta: &NowPlayingMetadata) {
    let _ = smtc.SetPlaybackStatus(MediaPlaybackStatus(smtc_playback_status(meta.status)));
    let (repeat_raw, shuffle) = smtc_repeat_shuffle(meta.loop_mode);
    let _ = smtc.SetAutoRepeatMode(MediaPlaybackAutoRepeatMode(repeat_raw));
    let _ = smtc.SetShuffleEnabled(shuffle);

    if let Ok(timeline) = SystemMediaTransportControlsTimelineProperties::new() {
        let end_ms = meta.duration_ms.max(meta.position_ms);
        let _ = timeline.SetStartTime(TimeSpan { Duration: 0 });
        let _ = timeline.SetMinSeekTime(TimeSpan { Duration: 0 });
        let _ = timeline.SetEndTime(TimeSpan {
            Duration: ms_to_hns(end_ms),
        });
        let _ = timeline.SetMaxSeekTime(TimeSpan {
            Duration: ms_to_hns(end_ms),
        });
        let _ = timeline.SetPosition(TimeSpan {
            Duration: ms_to_hns(meta.position_ms.min(end_ms)),
        });
        let _ = smtc.UpdateTimelineProperties(&timeline);
    }
}

fn clear_smtc(smtc: &SystemMediaTransportControls) {
    if let Ok(updater) = smtc.DisplayUpdater() {
        let _ = updater.ClearAll();
        let _ = updater.Update();
    }
    let _ = smtc.SetPlaybackStatus(MediaPlaybackStatus::Stopped);
    let _ = smtc.SetIsEnabled(false);
}
