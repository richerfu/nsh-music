use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nsh_music_core::model::{Quality, RepeatMode, SourceError, StreamInfo, Track};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::cache::{AudioCache, StreamCacheConfig};
use crate::decoder::{AudioDecoder, DecoderPreference, open_decoder};
use crate::dsp::{DspConfig, DspProcessor, SPECTRUM_BANDS, mix_crossfade};
use crate::output::{AudioOutput, OutputConfig, default_audio_output};
use crate::remote::{RemoteCommand, RemoteController};
use crate::{AudioError, AudioResult};

/// High-level player transport state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
    Error(String),
}

impl PlaybackStatus {
    pub fn is_playing(&self) -> bool {
        matches!(self, Self::Playing)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused)
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

/// Immutable state snapshot published whenever playback, queue, or timeline updates.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSnapshot {
    pub status: PlaybackStatus,
    pub current_track: Option<Track>,
    pub queue: Vec<Track>,
    pub current_index: Option<usize>,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub buffered_ms: u64,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub quality: Quality,
    pub replay_gain_enabled: bool,
    pub crossfade_ms: u32,
    pub decoder_backend: String,
    pub output_backend: String,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            current_track: None,
            queue: Vec::new(),
            current_index: None,
            position_ms: 0,
            duration_ms: 0,
            buffered_ms: 0,
            volume: 0.85,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            quality: Quality::High,
            replay_gain_enabled: true,
            crossfade_ms: 0,
            decoder_backend: String::new(),
            output_backend: String::new(),
        }
    }
}

/// Configuration for [`AudioEngine`].
#[derive(Debug, Clone)]
pub struct AudioEngineConfig {
    pub cache: StreamCacheConfig,
    pub decoder_preference: DecoderPreference,
    pub initial_volume: f32,
    pub replay_gain_enabled: bool,
    pub crossfade_ms: u32,
    pub quality: Quality,
    pub prefetch_next: bool,
}

impl Default for AudioEngineConfig {
    fn default() -> Self {
        Self {
            cache: StreamCacheConfig::default(),
            decoder_preference: DecoderPreference::Auto,
            initial_volume: 0.85,
            replay_gain_enabled: true,
            crossfade_ms: 0,
            quality: Quality::High,
            prefetch_next: true,
        }
    }
}

/// Callback resolving a [`Track`] into a playable [`StreamInfo`] (URL, custom HTTP
/// headers, ReplayGain peak/gain) off the UI thread.
pub type StreamResolver =
    Arc<dyn Fn(&Track, Quality) -> Result<StreamInfo, SourceError> + Send + Sync>;

/// Callback supplying fresh tracks for endless radio mode when the queue runs low.
pub type RadioRefillCallback = Arc<dyn Fn(Option<&Track>) -> Vec<Track> + Send + Sync>;

/// Optional callback notified whenever [`PlaybackSnapshot`] updates.
pub type StateChangeCallback = Arc<dyn Fn(&PlaybackSnapshot) + Send + Sync>;

enum EngineCommand {
    PlayTrack {
        track: Track,
        explicit_stream: Option<StreamInfo>,
    },
    PlayQueue {
        tracks: Vec<Track>,
        start_index: usize,
    },
    Play,
    Pause,
    TogglePlay,
    Stop,
    Seek(u64),
    Next,
    Prev,
    SetVolume(f32),
    SetMuted(bool),
    ToggleMute,
    SetShuffle(bool),
    ToggleShuffle,
    SetRepeat(RepeatMode),
    CycleRepeat,
    SetQuality(Quality),
    SetReplayGainEnabled(bool),
    SetCrossfadeMs(u32),
    Enqueue(Track),
    EnqueueNext(Track),
    RemoveFromQueue(usize),
    MoveQueueItem {
        from: usize,
        to: usize,
    },
    ClearQueue,
    SetStreamResolver(Option<StreamResolver>),
    SetRadioRefill(Option<RadioRefillCallback>),
    Shutdown,
}

struct AudioRenderShared {
    ring: VecDeque<f32>,
    dsp: DspProcessor,
    output_config: OutputConfig,
    playing: bool,
    decoder_eof: bool,
    played_frames_since_seek: u64,
    base_position_ms: u64,
    latest_spectrum: [f32; SPECTRUM_BANDS],
    track_ended_flag: bool,
}

impl AudioRenderShared {
    fn new(config: &AudioEngineConfig) -> Self {
        let dsp_cfg = DspConfig {
            volume: config.initial_volume.clamp(0.0, 1.0),
            replay_gain_enabled: config.replay_gain_enabled,
            ..DspConfig::default()
        };
        Self {
            ring: VecDeque::with_capacity(44_100 * 2),
            dsp: DspProcessor::new(dsp_cfg),
            output_config: OutputConfig::default(),
            playing: false,
            decoder_eof: false,
            played_frames_since_seek: 0,
            base_position_ms: 0,
            latest_spectrum: [0.0; SPECTRUM_BANDS],
            track_ended_flag: false,
        }
    }

    fn current_position_ms(&self) -> u64 {
        let rate = self.output_config.sample_rate.max(1) as u64;
        self.base_position_ms
            .saturating_add(self.played_frames_since_seek.saturating_mul(1000) / rate)
    }

    fn clear_ring(&mut self, base_position_ms: u64) {
        self.ring.clear();
        self.base_position_ms = base_position_ms;
        self.played_frames_since_seek = 0;
        self.decoder_eof = false;
        self.track_ended_flag = false;
    }
}

struct EngineSignals {
    state_tx: watch::Sender<PlaybackSnapshot>,
    spectrum_tx: watch::Sender<[f32; SPECTRUM_BANDS]>,
    state_callbacks: Mutex<Vec<StateChangeCallback>>,
}

struct AudioEngineInner {
    cmd_tx: Sender<EngineCommand>,
    signals: Arc<EngineSignals>,
    cache: AudioCache,
    remote: RemoteController,
    shutdown_called: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for AudioEngineInner {
    fn drop(&mut self) {
        if !self.shutdown_called.swap(true, Ordering::SeqCst) {
            let _ = self.cmd_tx.send(EngineCommand::Shutdown);
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

/// Thread-safe, clonable cross-platform audio engine managing playback transport,
/// queue, shuffle, repeat, radio auto-refill, seek, volume/mute, ReplayGain/DSP,
/// next-track prefetch, OS remote controls, and state/spectrum subscriptions.
#[derive(Clone)]
pub struct AudioEngine {
    inner: Arc<AudioEngineInner>,
}

impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl AudioEngine {
    /// Creates an [`AudioEngine`] using the platform's default hardware [`AudioOutput`]
    /// (or [`NullOutput`](crate::output::NullOutput) fallback on headless machines).
    pub fn new(config: AudioEngineConfig) -> AudioResult<Self> {
        let output = default_audio_output();
        Self::with_output(config, output)
    }

    /// Creates an [`AudioEngine`] with default configuration.
    pub fn default_engine() -> AudioResult<Self> {
        Self::new(AudioEngineConfig::default())
    }

    /// Creates an [`AudioEngine`] backed by an explicit [`AudioOutput`] implementation
    /// (e.g., [`NullOutput`](crate::output::NullOutput) for deterministic tests).
    pub fn with_output(
        config: AudioEngineConfig,
        output: Box<dyn AudioOutput>,
    ) -> AudioResult<Self> {
        let cache = AudioCache::new(config.cache.clone())?;
        let remote = RemoteController::new();
        let output_name = output.backend_name().to_owned();

        let initial_snapshot = PlaybackSnapshot {
            volume: config.initial_volume.clamp(0.0, 1.0),
            replay_gain_enabled: config.replay_gain_enabled,
            crossfade_ms: config.crossfade_ms,
            quality: config.quality,
            output_backend: output_name,
            ..PlaybackSnapshot::default()
        };

        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>();
        let (state_tx, _) = watch::channel(initial_snapshot.clone());
        let (spectrum_tx, _) = watch::channel([0.0f32; SPECTRUM_BANDS]);
        let signals = Arc::new(EngineSignals {
            state_tx,
            spectrum_tx,
            state_callbacks: Mutex::new(Vec::new()),
        });

        let inner = Arc::new(AudioEngineInner {
            cmd_tx,
            signals: Arc::clone(&signals),
            cache: cache.clone(),
            remote: remote.clone(),
            shutdown_called: AtomicBool::new(false),
            worker: Mutex::new(None),
        });

        let weak_inner = Arc::downgrade(&inner);
        let worker_signals = Arc::clone(&signals);
        let worker_handle = thread::Builder::new()
            .name("nsh-audio-engine".into())
            .spawn(move || {
                let mut actor = EngineActor::new(
                    config,
                    output,
                    cache,
                    remote,
                    initial_snapshot,
                    worker_signals,
                );
                actor.run(cmd_rx);
            })
            .map_err(|e| AudioError::Other(format!("failed to spawn audio engine thread: {e}")))?;

        if let Ok(mut guard) = inner.worker.lock() {
            *guard = Some(worker_handle);
        }

        // Wire OS remote control commands cleanly using a Weak reference to avoid Arc cycles.
        inner.remote.on_command(Arc::new(move |cmd| {
            let Some(strong) = weak_inner.upgrade() else {
                return;
            };
            let engine = AudioEngine { inner: strong };
            match cmd {
                RemoteCommand::Play => engine.play(),
                RemoteCommand::Pause => engine.pause(),
                RemoteCommand::TogglePlayPause => engine.toggle_play(),
                RemoteCommand::Stop => engine.stop(),
                RemoteCommand::Next => engine.next_track(),
                RemoteCommand::Prev => engine.prev_track(),
                RemoteCommand::Seek(ms) => engine.seek(ms),
                RemoteCommand::SeekRelative(delta_ms) => {
                    let cur = engine.snapshot().position_ms as i64;
                    engine.seek(cur.saturating_add(delta_ms).max(0) as u64);
                }
                RemoteCommand::SetVolume(pct) => {
                    engine.set_volume((pct.min(100) as f32) / 100.0);
                }
            }
        }));

        Ok(Self { inner })
    }

    /// Returns the underlying progressive HTTP / LRU disk [`AudioCache`].
    pub fn cache(&self) -> &AudioCache {
        &self.inner.cache
    }

    /// Returns total disk bytes currently occupied by cached audio files.
    pub fn cache_used_bytes(&self) -> u64 {
        self.inner.cache.total_bytes()
    }

    /// Removes all cached audio files from disk.
    pub fn clear_cache(&self) -> AudioResult<()> {
        self.inner.cache.clear()
    }

    /// Updates the LRU cache byte ceiling and evicts older entries if over limit.
    pub fn set_cache_max_bytes(&self, max_bytes: u64) -> AudioResult<usize> {
        self.inner.cache.set_max_bytes(max_bytes)
    }

    /// Returns the OS media [`RemoteController`] wired to this engine.
    pub fn remote_controller(&self) -> &RemoteController {
        &self.inner.remote
    }

    /// Returns the latest playback state snapshot.
    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.inner.signals.state_tx.borrow().clone()
    }

    /// Returns the latest 16-band spectrum levels (`0.0..=1.0`).
    pub fn spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        *self.inner.signals.spectrum_tx.borrow()
    }

    /// Subscribes to playback state snapshots via `tokio::sync::watch`.
    pub fn subscribe_state(&self) -> watch::Receiver<PlaybackSnapshot> {
        self.inner.signals.state_tx.subscribe()
    }

    /// Subscribes to 16-band spectrum frames via `tokio::sync::watch`.
    pub fn subscribe_spectrum(&self) -> watch::Receiver<[f32; SPECTRUM_BANDS]> {
        self.inner.signals.spectrum_tx.subscribe()
    }

    /// Registers a synchronous state-change callback.
    pub fn on_state_change(&self, cb: StateChangeCallback) {
        if let Ok(mut guard) = self.inner.signals.state_callbacks.lock() {
            guard.push(cb);
        }
    }

    /// Registers a callback that resolves a [`Track`] into a [`StreamInfo`] (URL, headers,
    /// ReplayGain metadata) before decoding or prefetching.
    pub fn set_stream_resolver(&self, resolver: Option<StreamResolver>) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::SetStreamResolver(resolver));
    }

    /// Registers a radio auto-refill callback invoked when playback approaches the end of the queue.
    pub fn set_radio_refill(&self, callback: Option<RadioRefillCallback>) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::SetRadioRefill(callback));
    }

    /// Replaces the queue with a single track and begins playing immediately.
    pub fn play_track(&self, track: Track) {
        let _ = self.inner.cmd_tx.send(EngineCommand::PlayTrack {
            track,
            explicit_stream: None,
        });
    }

    /// Plays a track using a caller-supplied pre-resolved [`StreamInfo`].
    pub fn play_stream(&self, track: Track, stream: StreamInfo) {
        let _ = self.inner.cmd_tx.send(EngineCommand::PlayTrack {
            track,
            explicit_stream: Some(stream),
        });
    }

    /// Replaces the queue with `tracks` and starts playing at `start_index`.
    pub fn play_queue(&self, tracks: Vec<Track>, start_index: usize) {
        let _ = self.inner.cmd_tx.send(EngineCommand::PlayQueue {
            tracks,
            start_index,
        });
    }

    pub fn play(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Play);
    }

    pub fn pause(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Pause);
    }

    pub fn toggle_play(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::TogglePlay);
    }

    pub fn stop(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Stop);
    }

    pub fn seek(&self, position_ms: u64) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Seek(position_ms));
    }

    pub fn next_track(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Next);
    }

    pub fn prev_track(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Prev);
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::SetVolume(volume.clamp(0.0, 1.0)));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.inner.cmd_tx.send(EngineCommand::SetMuted(muted));
    }

    pub fn toggle_mute(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::ToggleMute);
    }

    pub fn set_shuffle(&self, enabled: bool) {
        let _ = self.inner.cmd_tx.send(EngineCommand::SetShuffle(enabled));
    }

    pub fn toggle_shuffle(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::ToggleShuffle);
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        let _ = self.inner.cmd_tx.send(EngineCommand::SetRepeat(mode));
    }

    pub fn cycle_repeat(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::CycleRepeat);
    }

    pub fn set_quality(&self, quality: Quality) {
        let _ = self.inner.cmd_tx.send(EngineCommand::SetQuality(quality));
    }

    pub fn set_replay_gain_enabled(&self, enabled: bool) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::SetReplayGainEnabled(enabled));
    }

    pub fn set_crossfade_ms(&self, crossfade_ms: u32) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::SetCrossfadeMs(crossfade_ms.min(12_000)));
    }

    pub fn enqueue(&self, track: Track) {
        let _ = self.inner.cmd_tx.send(EngineCommand::Enqueue(track));
    }

    pub fn enqueue_next(&self, track: Track) {
        let _ = self.inner.cmd_tx.send(EngineCommand::EnqueueNext(track));
    }

    pub fn remove_from_queue(&self, index: usize) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::RemoveFromQueue(index));
    }

    pub fn move_queue_item(&self, from: usize, to: usize) {
        let _ = self
            .inner
            .cmd_tx
            .send(EngineCommand::MoveQueueItem { from, to });
    }

    pub fn clear_queue(&self) {
        let _ = self.inner.cmd_tx.send(EngineCommand::ClearQueue);
    }

    /// Cleanly stops audio output and terminates the engine actor thread.
    pub fn shutdown(&self) {
        if !self.inner.shutdown_called.swap(true, Ordering::SeqCst) {
            let _ = self.inner.cmd_tx.send(EngineCommand::Shutdown);
        }
    }
}

struct EngineActor {
    config: AudioEngineConfig,
    output: Box<dyn AudioOutput>,
    output_started: bool,
    output_config: OutputConfig,
    cache: AudioCache,
    remote: RemoteController,
    snapshot: PlaybackSnapshot,
    signals: Arc<EngineSignals>,
    shared: Arc<Mutex<AudioRenderShared>>,
    decoder: Option<Box<dyn AudioDecoder>>,
    stream_resolver: Option<StreamResolver>,
    radio_refill: Option<RadioRefillCallback>,
    shuffle_order: Vec<usize>,
    shuffle_cursor: usize,
    shuffle_seed: u64,
    pcm_scratch: Vec<f32>,
    resampled_scratch: Vec<f32>,
    crossfade_tail: Vec<f32>,
    crossfade_pos: usize,
    last_position_publish: Instant,
    last_prefetched_id: Option<String>,
}

impl EngineActor {
    fn new(
        config: AudioEngineConfig,
        output: Box<dyn AudioOutput>,
        cache: AudioCache,
        remote: RemoteController,
        snapshot: PlaybackSnapshot,
        signals: Arc<EngineSignals>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(AudioRenderShared::new(&config)));
        Self {
            config,
            output,
            output_started: false,
            output_config: OutputConfig::default(),
            cache,
            remote,
            snapshot,
            signals,
            shared,
            decoder: None,
            stream_resolver: None,
            radio_refill: None,
            shuffle_order: Vec::new(),
            shuffle_cursor: 0,
            shuffle_seed: 0x9e37_79b9_7f4a_7c15,
            pcm_scratch: Vec::with_capacity(8192),
            resampled_scratch: Vec::with_capacity(8192),
            crossfade_tail: Vec::new(),
            crossfade_pos: 0,
            last_position_publish: Instant::now(),
            last_prefetched_id: None,
        }
    }

    fn run(&mut self, rx: Receiver<EngineCommand>) {
        loop {
            let timeout = if self.snapshot.status.is_playing() {
                Duration::from_millis(12)
            } else {
                Duration::from_millis(40)
            };

            match rx.recv_timeout(timeout) {
                Ok(cmd) => {
                    if self.handle_command(cmd) {
                        break;
                    }
                    // Drain any queued commands before pumping PCM for snappy UI controls.
                    while let Ok(next_cmd) = rx.try_recv() {
                        if self.handle_command(next_cmd) {
                            let _ = self.output.stop();
                            return;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            self.tick_audio();
        }
        let _ = self.output.stop();
    }

    /// Returns `true` when the engine should shut down.
    fn handle_command(&mut self, cmd: EngineCommand) -> bool {
        match cmd {
            EngineCommand::PlayTrack {
                track,
                explicit_stream,
            } => {
                self.snapshot.queue = vec![track];
                self.snapshot.current_index = Some(0);
                self.rebuild_shuffle_order(Some(0));
                self.load_current_track(explicit_stream);
            }
            EngineCommand::PlayQueue {
                tracks,
                start_index,
            } => {
                if tracks.is_empty() {
                    self.stop_internal();
                    self.snapshot.queue.clear();
                    self.snapshot.current_index = None;
                    self.snapshot.current_track = None;
                    self.publish_snapshot();
                } else {
                    let idx = start_index.min(tracks.len() - 1);
                    self.snapshot.queue = tracks;
                    self.snapshot.current_index = Some(idx);
                    self.rebuild_shuffle_order(Some(idx));
                    self.load_current_track(None);
                }
            }
            EngineCommand::Play => {
                if self.snapshot.status.is_paused() && self.decoder.is_some() {
                    self.resume_internal();
                } else if self.decoder.is_none() {
                    if self.snapshot.current_index.is_none() && !self.snapshot.queue.is_empty() {
                        self.snapshot.current_index = Some(0);
                        self.rebuild_shuffle_order(Some(0));
                    }
                    if self.snapshot.current_index.is_some() {
                        self.load_current_track(None);
                    }
                }
            }
            EngineCommand::Pause => {
                if self.snapshot.status.is_playing() {
                    self.pause_internal();
                }
            }
            EngineCommand::TogglePlay => {
                if self.snapshot.status.is_playing() {
                    self.pause_internal();
                } else if self.snapshot.status.is_paused() && self.decoder.is_some() {
                    self.resume_internal();
                } else if self.snapshot.current_index.is_some() {
                    self.load_current_track(None);
                } else if !self.snapshot.queue.is_empty() {
                    self.snapshot.current_index = Some(0);
                    self.rebuild_shuffle_order(Some(0));
                    self.load_current_track(None);
                }
            }
            EngineCommand::Stop => {
                self.stop_internal();
                self.publish_snapshot();
            }
            EngineCommand::Seek(target_ms) => {
                self.seek_internal(target_ms);
            }
            EngineCommand::Next => {
                self.advance_next(true);
            }
            EngineCommand::Prev => {
                self.advance_prev();
            }
            EngineCommand::SetVolume(vol) => {
                self.snapshot.volume = vol.clamp(0.0, 1.0);
                if let Ok(mut shared) = self.shared.lock() {
                    shared.dsp.set_volume(self.snapshot.volume);
                }
                self.publish_snapshot();
            }
            EngineCommand::SetMuted(muted) => {
                self.snapshot.muted = muted;
                if let Ok(mut shared) = self.shared.lock() {
                    shared.dsp.set_muted(muted);
                }
                self.publish_snapshot();
            }
            EngineCommand::ToggleMute => {
                let next = !self.snapshot.muted;
                self.snapshot.muted = next;
                if let Ok(mut shared) = self.shared.lock() {
                    shared.dsp.set_muted(next);
                }
                self.publish_snapshot();
            }
            EngineCommand::SetShuffle(enabled) => {
                if self.snapshot.shuffle != enabled {
                    self.snapshot.shuffle = enabled;
                    self.rebuild_shuffle_order(self.snapshot.current_index);
                    self.publish_snapshot();
                }
            }
            EngineCommand::ToggleShuffle => {
                self.snapshot.shuffle = !self.snapshot.shuffle;
                self.rebuild_shuffle_order(self.snapshot.current_index);
                self.publish_snapshot();
            }
            EngineCommand::SetRepeat(mode) => {
                self.snapshot.repeat = mode;
                self.publish_snapshot();
            }
            EngineCommand::CycleRepeat => {
                self.snapshot.repeat = match self.snapshot.repeat {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::Off,
                };
                self.publish_snapshot();
            }
            EngineCommand::SetQuality(quality) => {
                self.snapshot.quality = quality;
                self.publish_snapshot();
            }
            EngineCommand::SetReplayGainEnabled(enabled) => {
                self.snapshot.replay_gain_enabled = enabled;
                if let Ok(mut shared) = self.shared.lock() {
                    shared.dsp.set_replay_gain(0.0, 1.0, enabled);
                }
                self.publish_snapshot();
            }
            EngineCommand::SetCrossfadeMs(ms) => {
                self.snapshot.crossfade_ms = ms;
                self.publish_snapshot();
            }
            EngineCommand::Enqueue(track) => {
                self.snapshot.queue.push(track);
                self.rebuild_shuffle_order(self.snapshot.current_index);
                self.publish_snapshot();
            }
            EngineCommand::EnqueueNext(track) => {
                let insert_at = self
                    .snapshot
                    .current_index
                    .map(|i| (i + 1).min(self.snapshot.queue.len()))
                    .unwrap_or(0);
                self.snapshot.queue.insert(insert_at, track);
                self.rebuild_shuffle_order(self.snapshot.current_index);
                self.publish_snapshot();
            }
            EngineCommand::RemoveFromQueue(index) => {
                if index < self.snapshot.queue.len() {
                    self.snapshot.queue.remove(index);
                    match self.snapshot.current_index {
                        Some(cur) if cur == index => {
                            if self.snapshot.queue.is_empty() {
                                self.stop_internal();
                                self.snapshot.current_index = None;
                                self.snapshot.current_track = None;
                            } else {
                                let next_idx = index.min(self.snapshot.queue.len() - 1);
                                self.snapshot.current_index = Some(next_idx);
                                self.rebuild_shuffle_order(Some(next_idx));
                                self.load_current_track(None);
                                return false;
                            }
                        }
                        Some(cur) if index < cur => {
                            self.snapshot.current_index = Some(cur - 1);
                        }
                        _ => {}
                    }
                    self.rebuild_shuffle_order(self.snapshot.current_index);
                    self.publish_snapshot();
                }
            }
            EngineCommand::MoveQueueItem { from, to } => {
                let len = self.snapshot.queue.len();
                if from < len && to < len && from != to {
                    let item = self.snapshot.queue.remove(from);
                    self.snapshot.queue.insert(to, item);
                    if let Some(cur) = self.snapshot.current_index {
                        self.snapshot.current_index = Some(if cur == from {
                            to
                        } else if from < cur && to >= cur {
                            cur - 1
                        } else if from > cur && to <= cur {
                            cur + 1
                        } else {
                            cur
                        });
                    }
                    self.rebuild_shuffle_order(self.snapshot.current_index);
                    self.publish_snapshot();
                }
            }
            EngineCommand::ClearQueue => {
                self.stop_internal();
                self.snapshot.queue.clear();
                self.snapshot.current_index = None;
                self.snapshot.current_track = None;
                self.shuffle_order.clear();
                self.shuffle_cursor = 0;
                self.publish_snapshot();
            }
            EngineCommand::SetStreamResolver(resolver) => {
                self.stream_resolver = resolver;
            }
            EngineCommand::SetRadioRefill(cb) => {
                self.radio_refill = cb;
            }
            EngineCommand::Shutdown => {
                self.stop_internal();
                return true;
            }
        }
        false
    }

    fn ensure_output_started(&mut self, desired: OutputConfig) -> AudioResult<()> {
        if self.output_started {
            let _ = self.output.resume();
            return Ok(());
        }

        let shared = Arc::clone(&self.shared);
        let negotiated = self.output.start(
            desired,
            Box::new(move |out: &mut [f32]| {
                out.fill(0.0);
                let Ok(mut guard) = shared.lock() else {
                    return;
                };
                if !guard.playing {
                    guard.dsp.decay_spectrum(0.84);
                    guard.latest_spectrum = guard.dsp.spectrum();
                    return;
                }

                let mut written = 0usize;
                while written < out.len() {
                    if let Some(sample) = guard.ring.pop_front() {
                        out[written] = sample;
                        written += 1;
                    } else {
                        break;
                    }
                }

                let ch = (guard.output_config.channels as usize).max(1);
                if written > 0 {
                    guard.played_frames_since_seek += (written / ch) as u64;
                    guard.dsp.process_interleaved(out);
                    guard.latest_spectrum = guard.dsp.spectrum();
                } else if guard.decoder_eof {
                    guard.track_ended_flag = true;
                    guard.dsp.decay_spectrum(0.8);
                    guard.latest_spectrum = guard.dsp.spectrum();
                } else {
                    guard.dsp.decay_spectrum(0.9);
                    guard.latest_spectrum = guard.dsp.spectrum();
                }
            }),
        )?;

        self.output_started = true;
        self.output_config = negotiated;
        self.snapshot.output_backend = self.output.backend_name().to_owned();
        if let Ok(mut guard) = self.shared.lock() {
            guard.output_config = negotiated;
            guard
                .dsp
                .configure_stream(negotiated.sample_rate, negotiated.channels);
        }
        Ok(())
    }

    fn load_current_track(&mut self, explicit_stream: Option<StreamInfo>) {
        let Some(idx) = self.snapshot.current_index else {
            self.stop_internal();
            self.publish_snapshot();
            return;
        };
        let Some(track) = self.snapshot.queue.get(idx).cloned() else {
            self.stop_internal();
            self.publish_snapshot();
            return;
        };

        // Capture outgoing PCM tail for smooth equal-power crossfade when enabled.
        self.crossfade_tail.clear();
        self.crossfade_pos = 0;
        if self.snapshot.crossfade_ms > 0
            && self.snapshot.status.is_playing()
            && let Ok(mut guard) = self.shared.lock()
        {
            let max_xfade_samples = ((self.snapshot.crossfade_ms as usize)
                * (guard.output_config.sample_rate as usize)
                / 1000)
                * (guard.output_config.channels as usize).max(1);
            let take_len = guard.ring.len().min(max_xfade_samples);
            self.crossfade_tail.extend(guard.ring.drain(..take_len));
        }

        self.snapshot.current_track = Some(track.clone());
        self.snapshot.status = PlaybackStatus::Loading;
        self.snapshot.position_ms = 0;
        self.snapshot.duration_ms = track.duration.max(0) as u64;
        self.snapshot.buffered_ms = 0;
        self.publish_snapshot();

        let resolved = match explicit_stream {
            Some(stream) => Ok(stream),
            None => self.resolve_track_stream(&track),
        };

        let stream_info = match resolved {
            Ok(info) => info,
            Err(err) => {
                self.decoder = None;
                self.snapshot.status = PlaybackStatus::Error(err);
                if let Ok(mut guard) = self.shared.lock() {
                    guard.playing = false;
                    guard.clear_ring(0);
                }
                self.publish_snapshot();
                return;
            }
        };

        let headers: Vec<(String, String)> = stream_info
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let media_type = if stream_info.media_type.is_empty() {
            None
        } else {
            Some(stream_info.media_type.as_str())
        };
        let duration_hint = if track.duration > 0 {
            Some(track.duration as u64)
        } else {
            None
        };

        let decoder = match open_decoder(
            &self.cache,
            &stream_info.url,
            headers,
            media_type,
            duration_hint,
            self.config.decoder_preference,
        ) {
            Ok(dec) => dec,
            Err(err) => {
                self.decoder = None;
                self.snapshot.status = PlaybackStatus::Error(err.to_string());
                if let Ok(mut guard) = self.shared.lock() {
                    guard.playing = false;
                    guard.clear_ring(0);
                }
                self.publish_snapshot();
                return;
            }
        };

        let spec = decoder.spec();
        if let Err(err) = self.ensure_output_started(OutputConfig {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        }) {
            self.decoder = None;
            self.snapshot.status = PlaybackStatus::Error(err.to_string());
            self.publish_snapshot();
            return;
        }

        let duration_ms = spec
            .duration_ms
            .filter(|&d| d > 0)
            .or(duration_hint)
            .unwrap_or(0);
        self.snapshot.duration_ms = duration_ms;
        self.snapshot.position_ms = 0;
        self.snapshot.buffered_ms = duration_ms;
        self.snapshot.decoder_backend = decoder.backend_name().to_owned();
        self.snapshot.status = PlaybackStatus::Playing;
        self.decoder = Some(decoder);

        if let Ok(mut guard) = self.shared.lock() {
            guard.clear_ring(0);
            guard.dsp.set_replay_gain(
                stream_info.gain as f32,
                if stream_info.peak > 0.0 {
                    stream_info.peak as f32
                } else {
                    1.0
                },
                self.snapshot.replay_gain_enabled,
            );
            guard.dsp.trigger_fade_in(12);
            guard.playing = true;
        }

        // Prime ring buffer immediately before publishing Playing state.
        self.fill_decoder_buffer();
        self.publish_snapshot();
        self.sync_remote_metadata();
        self.maybe_prefetch_next();
        self.maybe_refill_radio();
    }

    fn resolve_track_stream(&self, track: &Track) -> Result<StreamInfo, String> {
        if let Some(ref resolver) = self.stream_resolver {
            match resolver(track, self.snapshot.quality) {
                Ok(info) if !info.url.is_empty() => return Ok(info),
                Ok(_) => {}
                Err(err) => {
                    // Fall back to direct URL / local_path if present, else return error.
                    if track.url.is_empty() && track.local_path.is_empty() {
                        return Err(err.to_string());
                    }
                }
            }
        }

        if !track.local_path.is_empty() {
            return Ok(StreamInfo {
                url: track.local_path.clone(),
                ..StreamInfo::default()
            });
        }
        if !track.url.is_empty() {
            return Ok(StreamInfo {
                url: track.url.clone(),
                ..StreamInfo::default()
            });
        }
        Err(format!(
            "track '{}' has no playable URL and no stream resolver returned a stream",
            track.name
        ))
    }

    fn pause_internal(&mut self) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.playing = false;
        }
        let _ = self.output.pause();
        self.snapshot.status = PlaybackStatus::Paused;
        self.publish_snapshot();
        self.remote.update_playback_state(
            false,
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
        );
    }

    fn resume_internal(&mut self) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.dsp.trigger_fade_in(10);
            guard.playing = true;
        }
        let _ = self.output.resume();
        self.snapshot.status = PlaybackStatus::Playing;
        self.publish_snapshot();
        self.remote.update_playback_state(
            true,
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
        );
    }

    fn stop_internal(&mut self) {
        self.decoder = None;
        self.crossfade_tail.clear();
        self.crossfade_pos = 0;
        if let Ok(mut guard) = self.shared.lock() {
            guard.playing = false;
            guard.clear_ring(0);
            guard.dsp.reset();
            guard.latest_spectrum = [0.0; SPECTRUM_BANDS];
        }
        if self.output_started {
            let _ = self.output.pause();
        }
        self.snapshot.status = PlaybackStatus::Stopped;
        self.snapshot.position_ms = 0;
        let _ = self.signals.spectrum_tx.send_replace([0.0; SPECTRUM_BANDS]);
        self.remote.clear();
    }

    fn seek_internal(&mut self, target_ms: u64) {
        let clamped = if self.snapshot.duration_ms > 0 {
            target_ms.min(self.snapshot.duration_ms)
        } else {
            target_ms
        };

        let Some(ref mut decoder) = self.decoder else {
            self.snapshot.position_ms = clamped;
            self.publish_snapshot();
            return;
        };

        match decoder.seek(clamped) {
            Ok(landed_ms) => {
                self.crossfade_tail.clear();
                self.crossfade_pos = 0;
                if let Ok(mut guard) = self.shared.lock() {
                    guard.clear_ring(landed_ms);
                    guard.dsp.trigger_fade_in(8);
                }
                self.snapshot.position_ms = landed_ms;
                self.fill_decoder_buffer();
                self.publish_snapshot();
                self.remote.update_playback_state(
                    self.snapshot.status.is_playing(),
                    landed_ms,
                    self.snapshot.duration_ms,
                );
            }
            Err(err) => {
                log::warn!("seek to {clamped}ms failed: {err}");
            }
        }
    }

    fn tick_audio(&mut self) {
        if self.snapshot.status.is_playing() {
            self.fill_decoder_buffer();
        }

        let (pos_ms, spectrum, ended) = match self.shared.lock() {
            Ok(mut guard) => {
                let ended = guard.track_ended_flag;
                if ended {
                    guard.track_ended_flag = false;
                }
                (guard.current_position_ms(), guard.latest_spectrum, ended)
            }
            Err(_) => return,
        };

        let _ = self.signals.spectrum_tx.send_replace(spectrum);

        if ended && self.snapshot.status.is_playing() {
            self.on_track_finished();
            return;
        }

        if self.snapshot.status.is_playing()
            && self.last_position_publish.elapsed() >= Duration::from_millis(50)
        {
            self.last_position_publish = Instant::now();
            let clamped = if self.snapshot.duration_ms > 0 {
                pos_ms.min(self.snapshot.duration_ms)
            } else {
                pos_ms
            };
            if clamped != self.snapshot.position_ms {
                self.snapshot.position_ms = clamped;
                self.publish_snapshot();
            }
        }
    }

    fn fill_decoder_buffer(&mut self) {
        let Some(ref mut decoder) = self.decoder else {
            return;
        };
        let target_samples =
            (self.output_config.sample_rate as usize / 2) * (self.output_config.channels as usize).max(1);

        loop {
            let current_len = self
                .shared
                .lock()
                .map(|g| g.ring.len())
                .unwrap_or(target_samples);
            if current_len >= target_samples {
                break;
            }

            self.pcm_scratch.clear();
            let spec = decoder.spec();
            match decoder.read_pcm(&mut self.pcm_scratch) {
                Ok(0) => {
                    if let Ok(mut guard) = self.shared.lock() {
                        guard.decoder_eof = true;
                    }
                    break;
                }
                Ok(_) => {
                    self.resampled_scratch.clear();
                    convert_pcm(
                        &self.pcm_scratch,
                        spec.sample_rate,
                        spec.channels,
                        self.output_config.sample_rate,
                        self.output_config.channels,
                        &mut self.resampled_scratch,
                    );

                    // Apply equal-power crossfade against outgoing tail if active.
                    if self.crossfade_pos < self.crossfade_tail.len() {
                        let tail_rem = self.crossfade_tail.len() - self.crossfade_pos;
                        let overlap = tail_rem.min(self.resampled_scratch.len());
                        let total_xfade = self.crossfade_tail.len().max(1) as f32;
                        let progress = (self.crossfade_pos as f32 + overlap as f32 * 0.5)
                            / total_xfade;
                        let mut blended = vec![0.0f32; overlap];
                        mix_crossfade(
                            &self.crossfade_tail[self.crossfade_pos..self.crossfade_pos + overlap],
                            &self.resampled_scratch[..overlap],
                            &mut blended,
                            progress,
                        );
                        self.resampled_scratch[..overlap].copy_from_slice(&blended);
                        self.crossfade_pos += overlap;
                        if self.crossfade_pos >= self.crossfade_tail.len() {
                            self.crossfade_tail.clear();
                            self.crossfade_pos = 0;
                        }
                    }

                    if let Ok(mut guard) = self.shared.lock() {
                        guard.ring.extend(self.resampled_scratch.iter().copied());
                    }
                }
                Err(err) => {
                    log::warn!("audio decode error: {err}");
                    if let Ok(mut guard) = self.shared.lock() {
                        guard.decoder_eof = true;
                    }
                    break;
                }
            }
        }
    }

    fn on_track_finished(&mut self) {
        if self.snapshot.repeat == RepeatMode::One {
            self.seek_internal(0);
            if let Ok(mut guard) = self.shared.lock() {
                guard.decoder_eof = false;
                guard.track_ended_flag = false;
                guard.playing = true;
            }
            return;
        }
        self.advance_next(false);
    }

    fn next_queue_index(&self, manual_skip: bool) -> Option<usize> {
        let len = self.snapshot.queue.len();
        if len == 0 {
            return None;
        }
        if self.snapshot.shuffle && !self.shuffle_order.is_empty() {
            if self.shuffle_cursor + 1 < self.shuffle_order.len() {
                return Some(self.shuffle_order[self.shuffle_cursor + 1]);
            }
            if self.snapshot.repeat == RepeatMode::All || (manual_skip && self.snapshot.repeat == RepeatMode::One) {
                return self.shuffle_order.first().copied();
            }
            return None;
        }

        let cur = self.snapshot.current_index.unwrap_or(0);
        if cur + 1 < len {
            Some(cur + 1)
        } else if self.snapshot.repeat == RepeatMode::All
            || (manual_skip && self.snapshot.repeat == RepeatMode::One)
        {
            Some(0)
        } else {
            None
        }
    }

    fn advance_next(&mut self, manual_skip: bool) {
        if let Some(next_idx) = self.next_queue_index(manual_skip) {
            if self.snapshot.shuffle
                && !self.shuffle_order.is_empty()
                && let Some(pos) = self.shuffle_order.iter().position(|&i| i == next_idx)
            {
                self.shuffle_cursor = pos;
            }
            self.snapshot.current_index = Some(next_idx);
            self.load_current_track(None);
            return;
        }

        // Attempt radio auto-refill before stopping!
        if self.try_refill_radio_now()
            && let Some(next_idx) = self.next_queue_index(manual_skip)
        {
            if self.snapshot.shuffle
                && let Some(pos) = self.shuffle_order.iter().position(|&i| i == next_idx)
            {
                self.shuffle_cursor = pos;
            }
            self.snapshot.current_index = Some(next_idx);
            self.load_current_track(None);
            return;
        }

        self.stop_internal();
        self.publish_snapshot();
    }

    fn advance_prev(&mut self) {
        // Restart current track if past 3 seconds.
        if self.snapshot.position_ms > 3_000 && self.decoder.is_some() {
            self.seek_internal(0);
            return;
        }

        let len = self.snapshot.queue.len();
        if len == 0 {
            return;
        }

        let prev_idx = if self.snapshot.shuffle && !self.shuffle_order.is_empty() {
            if self.shuffle_cursor > 0 {
                self.shuffle_cursor -= 1;
                self.shuffle_order[self.shuffle_cursor]
            } else if self.snapshot.repeat == RepeatMode::All {
                self.shuffle_cursor = self.shuffle_order.len() - 1;
                self.shuffle_order[self.shuffle_cursor]
            } else {
                self.shuffle_order[0]
            }
        } else {
            let cur = self.snapshot.current_index.unwrap_or(0);
            if cur > 0 {
                cur - 1
            } else if self.snapshot.repeat == RepeatMode::All {
                len - 1
            } else {
                0
            }
        };

        self.snapshot.current_index = Some(prev_idx);
        self.load_current_track(None);
    }

    fn rebuild_shuffle_order(&mut self, anchor: Option<usize>) {
        let len = self.snapshot.queue.len();
        self.shuffle_order = (0..len).collect();
        self.shuffle_cursor = 0;

        if !self.snapshot.shuffle || len <= 1 {
            if let Some(a) = anchor {
                self.shuffle_cursor = a.min(len.saturating_sub(1));
            }
            return;
        }

        // Keep the active track first, then Fisher-Yates shuffle the remaining indices.
        if let Some(a) = anchor.filter(|&i| i < len) {
            self.shuffle_order.swap(0, a);
            for i in (2..len).rev() {
                let j = 1 + (self.next_rand() as usize % i);
                self.shuffle_order.swap(i, j);
            }
            self.shuffle_cursor = 0;
        } else {
            for i in (1..len).rev() {
                let j = self.next_rand() as usize % (i + 1);
                self.shuffle_order.swap(i, j);
            }
            self.shuffle_cursor = 0;
        }
    }

    fn next_rand(&mut self) -> u64 {
        let mut x = self.shuffle_seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.shuffle_seed = x;
        x
    }

    fn maybe_prefetch_next(&mut self) {
        if !self.config.prefetch_next {
            return;
        }
        let Some(next_idx) = self.next_queue_index(false) else {
            return;
        };
        let Some(next_track) = self.snapshot.queue.get(next_idx).cloned() else {
            return;
        };
        if self.last_prefetched_id.as_deref() == Some(next_track.id.as_str()) {
            return;
        }
        self.last_prefetched_id = Some(next_track.id.clone());

        let url_and_headers = if next_track.url.starts_with("http://")
            || next_track.url.starts_with("https://")
        {
            Some((next_track.url.clone(), Vec::new()))
        } else if let Some(ref resolver) = self.stream_resolver {
            resolver(&next_track, self.snapshot.quality)
                .ok()
                .filter(|s| s.url.starts_with("http://") || s.url.starts_with("https://"))
                .map(|s| {
                    (
                        s.url,
                        s.headers.into_iter().collect::<Vec<(String, String)>>(),
                    )
                })
        } else {
            None
        };

        if let Some((url, headers)) = url_and_headers {
            drop(self.cache.prefetch(url, headers, Some(512 * 1024)));
        }
    }

    fn maybe_refill_radio(&mut self) {
        if self.radio_refill.is_none() || self.snapshot.repeat != RepeatMode::Off {
            return;
        }
        let remaining = match self.snapshot.current_index {
            Some(cur) => self.snapshot.queue.len().saturating_sub(cur + 1),
            None => 0,
        };
        if remaining == 0 {
            let _ = self.try_refill_radio_now();
        }
    }

    fn try_refill_radio_now(&mut self) -> bool {
        let Some(ref refill) = self.radio_refill else {
            return false;
        };
        let fresh = refill(self.snapshot.current_track.as_ref());
        if fresh.is_empty() {
            return false;
        }
        self.snapshot.queue.extend(fresh);
        self.rebuild_shuffle_order(self.snapshot.current_index);
        self.publish_snapshot();
        true
    }

    fn sync_remote_metadata(&self) {
        let Some(ref track) = self.snapshot.current_track else {
            self.remote.clear();
            return;
        };
        let can_next = self.next_queue_index(true).is_some() || self.radio_refill.is_some();
        let can_prev = !self.snapshot.queue.is_empty();
        self.remote.set_track(
            track,
            self.snapshot.status.is_playing(),
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
            can_next,
            can_prev,
        );
    }

    fn publish_snapshot(&self) {
        let snap = self.snapshot.clone();
        let _ = self.signals.state_tx.send_replace(snap.clone());
        let callbacks = self
            .signals
            .state_callbacks
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        for cb in callbacks {
            cb(&snap);
        }
    }
}

/// Converts interleaved PCM from `(src_rate, src_ch)` to `(dst_rate, dst_ch)` using
/// channel mapping and linear interpolation resampling when rates differ.
pub(crate) fn convert_pcm(
    input: &[f32],
    src_rate: u32,
    src_ch: u16,
    dst_rate: u32,
    dst_ch: u16,
    out: &mut Vec<f32>,
) {
    let sc = (src_ch as usize).max(1);
    let dc = (dst_ch as usize).max(1);
    let src_frames = input.len() / sc;
    if src_frames == 0 {
        return;
    }

    // Fast path: identical sample rate & channel count.
    if src_rate == dst_rate && sc == dc {
        out.extend_from_slice(input);
        return;
    }

    // Channel conversion first into temporary frame slice if needed.
    let channel_mapped: Vec<f32> = if sc == dc {
        input[..src_frames * sc].to_vec()
    } else {
        let mut mapped = Vec::with_capacity(src_frames * dc);
        for frame in input[..src_frames * sc].chunks_exact(sc) {
            match (sc, dc) {
                (1, 2) => {
                    mapped.push(frame[0]);
                    mapped.push(frame[0]);
                }
                (_, 1) => {
                    let avg = frame.iter().copied().sum::<f32>() / sc as f32;
                    mapped.push(avg);
                }
                (_, 2) => {
                    mapped.push(frame[0]);
                    mapped.push(frame[1]);
                }
                _ => {
                    for c in 0..dc {
                        mapped.push(frame[c % sc]);
                    }
                }
            }
        }
        mapped
    };

    if src_rate == dst_rate || src_rate == 0 || dst_rate == 0 {
        out.extend_from_slice(&channel_mapped);
        return;
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let dst_frames = ((src_frames as f64) / ratio).round().max(1.0) as usize;
    out.reserve(dst_frames * dc);

    for out_f in 0..dst_frames {
        let src_pos = (out_f as f64) * ratio;
        let idx0 = (src_pos.floor() as usize).min(src_frames - 1);
        let idx1 = (idx0 + 1).min(src_frames - 1);
        let frac = (src_pos - idx0 as f64) as f32;
        for c in 0..dc {
            let s0 = channel_mapped[idx0 * dc + c];
            let s1 = channel_mapped[idx1 * dc + c];
            out.push(s0 + (s1 - s0) * frac);
        }
    }
}
