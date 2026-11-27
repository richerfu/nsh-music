use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nsh_music_core::model::{Quality, RepeatMode, SourceError, StreamInfo, Track};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::cache::{
    AudioCache, CachedStreamMeta, StreamCacheConfig, cache_key_for_track, is_loopback_url,
};
use crate::decoder::{
    AudioDecoder, AudioSpec, DecoderPreference, infer_extension_hint, open_decoder_with_key,
};
use crate::dsp::{DspConfig, DspProcessor, SPECTRUM_BANDS, mix_crossfade_in_place};
use crate::output::{AudioOutput, AudioOutputEvent, OutputConfig, default_audio_output};
use crate::remote::{RemoteCommand, RemoteController, RemoteLoopMode, RemotePlaybackStatus};
use crate::{AudioError, AudioResult};

/// Fixed power-of-two capacity for the pre-allocated PCM ring buffer (~1.36s stereo @ 48kHz).
const PCM_RING_CAPACITY: usize = 131_072;
/// Maximum number of pre-resolved remote `StreamInfo` entries held in memory.
const RESOLVED_STREAM_CACHE_MAX_ENTRIES: usize = 64;
/// TTL for pre-resolved remote `StreamInfo` entries before re-resolving via `stream_resolver`.
const RESOLVED_STREAM_CACHE_TTL: Duration = Duration::from_secs(480);

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
    pub is_lossless: bool,
    pub stream_format: String,
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
            is_lossless: false,
            stream_format: String::new(),
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
    OutputEvent(AudioOutputEvent),
    Shutdown,
}

/// Pre-allocated fixed-capacity circular buffer supporting bulk `copy_from_slice`
/// transfer with zero heap reallocations during playback.
pub(crate) struct FixedPcmRing {
    buf: Box<[f32]>,
    mask: usize,
    read_idx: usize,
    write_idx: usize,
    len: usize,
}

impl FixedPcmRing {
    pub(crate) fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two().max(1024);
        Self {
            buf: vec![0.0f32; cap].into_boxed_slice(),
            mask: cap - 1,
            read_idx: 0,
            write_idx: 0,
            len: 0,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub(crate) fn free_space(&self) -> usize {
        self.buf.len() - self.len
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.read_idx = 0;
        self.write_idx = 0;
        self.len = 0;
    }

    pub(crate) fn push_slice(&mut self, data: &[f32]) -> usize {
        let to_copy = data.len().min(self.free_space());
        if to_copy == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let first = to_copy.min(cap - self.write_idx);
        self.buf[self.write_idx..self.write_idx + first].copy_from_slice(&data[..first]);
        let second = to_copy - first;
        if second > 0 {
            self.buf[..second].copy_from_slice(&data[first..to_copy]);
        }
        self.write_idx = (self.write_idx + to_copy) & self.mask;
        self.len += to_copy;
        to_copy
    }

    pub(crate) fn pop_slice(&mut self, out: &mut [f32]) -> usize {
        let to_copy = out.len().min(self.len);
        if to_copy == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let first = to_copy.min(cap - self.read_idx);
        out[..first].copy_from_slice(&self.buf[self.read_idx..self.read_idx + first]);
        let second = to_copy - first;
        if second > 0 {
            out[first..to_copy].copy_from_slice(&self.buf[..second]);
        }
        self.read_idx = (self.read_idx + to_copy) & self.mask;
        self.len -= to_copy;
        to_copy
    }

    pub(crate) fn drain_into(&mut self, dst: &mut Vec<f32>, max_samples: usize) {
        let to_copy = max_samples.min(self.len);
        if to_copy == 0 {
            return;
        }
        let start = dst.len();
        dst.resize(start + to_copy, 0.0);
        let popped = self.pop_slice(&mut dst[start..]);
        dst.truncate(start + popped);
    }
}

/// Lock-free atomic bridge shared between the real-time audio render callback,
/// the background `EngineActor`, and UI callers (`spectrum()`, `position_ms()`).
///
/// Keeping all DSP controls, transport flags, playback position, and 16-band
/// spectrum levels in atomics avoids holding mutexes during DSP/FFT or UI polling.
struct AudioAtomics {
    playing: AtomicBool,
    decoder_eof: AtomicBool,
    track_ended: AtomicBool,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    base_position_ms: AtomicU64,
    played_frames_since_seek: AtomicU64,
    ring_len: AtomicUsize,

    dsp_gen: AtomicU64,
    volume_bits: AtomicU32,
    muted: AtomicBool,
    rg_gain_bits: AtomicU32,
    rg_peak_bits: AtomicU32,
    rg_enabled: AtomicBool,
    pending_fade_in_ms: AtomicU32,
    pending_dsp_reset: AtomicBool,

    spectrum_bits: [AtomicU32; SPECTRUM_BANDS],
    spectrum_dirty: AtomicBool,
    snapshot_version: AtomicU64,
}

impl AudioAtomics {
    fn new(config: &AudioEngineConfig) -> Self {
        Self {
            playing: AtomicBool::new(false),
            decoder_eof: AtomicBool::new(false),
            track_ended: AtomicBool::new(false),
            sample_rate: AtomicU32::new(44_100),
            channels: AtomicU32::new(2),
            base_position_ms: AtomicU64::new(0),
            played_frames_since_seek: AtomicU64::new(0),
            ring_len: AtomicUsize::new(0),

            dsp_gen: AtomicU64::new(1),
            volume_bits: AtomicU32::new(config.initial_volume.clamp(0.0, 1.0).to_bits()),
            muted: AtomicBool::new(false),
            rg_gain_bits: AtomicU32::new(0.0f32.to_bits()),
            rg_peak_bits: AtomicU32::new(1.0f32.to_bits()),
            rg_enabled: AtomicBool::new(config.replay_gain_enabled),
            pending_fade_in_ms: AtomicU32::new(0),
            pending_dsp_reset: AtomicBool::new(false),

            spectrum_bits: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            spectrum_dirty: AtomicBool::new(false),
            snapshot_version: AtomicU64::new(1),
        }
    }

    #[inline]
    fn current_position_ms(&self) -> u64 {
        let rate = self.sample_rate.load(Ordering::Relaxed).max(1) as u64;
        let base = self.base_position_ms.load(Ordering::Acquire);
        let frames = self.played_frames_since_seek.load(Ordering::Relaxed);
        base.saturating_add(frames.saturating_mul(1000) / rate)
    }

    #[inline]
    fn read_spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.spectrum_bits[i].load(Ordering::Relaxed)))
    }

    #[inline]
    fn write_spectrum(&self, bands: &[f32; SPECTRUM_BANDS]) {
        for (slot, &val) in self.spectrum_bits.iter().zip(bands.iter()) {
            slot.store(val.to_bits(), Ordering::Relaxed);
        }
        self.spectrum_dirty.store(true, Ordering::Release);
    }

    fn clear_spectrum(&self) {
        let zeros = [0.0f32; SPECTRUM_BANDS];
        self.write_spectrum(&zeros);
    }

    fn set_volume(&self, volume: f32) {
        self.volume_bits
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.dsp_gen.fetch_add(1, Ordering::Release);
    }

    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.dsp_gen.fetch_add(1, Ordering::Release);
    }

    fn set_replay_gain(&self, gain_db: f32, peak: f32, enabled: bool) {
        self.rg_gain_bits
            .store(gain_db.to_bits(), Ordering::Relaxed);
        self.rg_peak_bits.store(peak.to_bits(), Ordering::Relaxed);
        self.rg_enabled.store(enabled, Ordering::Relaxed);
        self.dsp_gen.fetch_add(1, Ordering::Release);
    }

    fn trigger_fade_in(&self, duration_ms: u32) {
        self.pending_fade_in_ms
            .store(duration_ms, Ordering::Release);
    }

    fn request_dsp_reset(&self) {
        self.pending_dsp_reset.store(true, Ordering::Release);
    }

    fn reset_timeline(&self, base_position_ms: u64) {
        self.base_position_ms
            .store(base_position_ms, Ordering::Release);
        self.played_frames_since_seek.store(0, Ordering::Release);
        self.decoder_eof.store(false, Ordering::Release);
        self.track_ended.store(false, Ordering::Release);
    }
}

#[cfg(test)]
type PrefetchTarget = (String, Vec<(String, String)>);
type ResolvedStreamCache = Arc<Mutex<HashMap<String, (StreamInfo, Instant)>>>;

struct EngineSignals {
    state_tx: watch::Sender<PlaybackSnapshot>,
    spectrum_tx: watch::Sender<[f32; SPECTRUM_BANDS]>,
    state_callbacks: Mutex<Vec<StateChangeCallback>>,
    atomics: Arc<AudioAtomics>,
    #[cfg(test)]
    prefetched_targets: Arc<Mutex<Vec<PrefetchTarget>>>,
}

struct AudioEngineInner {
    cmd_tx: Sender<EngineCommand>,
    signals: Arc<EngineSignals>,
    cache: AudioCache,
    resolved_stream_cache: ResolvedStreamCache,
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
        mut output: Box<dyn AudioOutput>,
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
        let event_tx = cmd_tx.clone();
        output.set_event_callback(Some(Arc::new(move |ev| {
            let _ = event_tx.send(EngineCommand::OutputEvent(ev));
        })));
        let (state_tx, _) = watch::channel(initial_snapshot.clone());
        let (spectrum_tx, _) = watch::channel([0.0f32; SPECTRUM_BANDS]);
        let atomics = Arc::new(AudioAtomics::new(&config));
        let signals = Arc::new(EngineSignals {
            state_tx,
            spectrum_tx,
            state_callbacks: Mutex::new(Vec::new()),
            atomics: Arc::clone(&atomics),
            #[cfg(test)]
            prefetched_targets: Arc::new(Mutex::new(Vec::new())),
        });

        let resolved_stream_cache: ResolvedStreamCache = Arc::new(Mutex::new(HashMap::new()));
        let inner = Arc::new(AudioEngineInner {
            cmd_tx,
            signals: Arc::clone(&signals),
            cache: cache.clone(),
            resolved_stream_cache: Arc::clone(&resolved_stream_cache),
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
                    resolved_stream_cache,
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
                    let cur = engine.position_ms() as i64;
                    engine.seek(cur.saturating_add(delta_ms).max(0) as u64);
                }
                RemoteCommand::SetVolume(pct) => {
                    engine.set_volume((pct.min(100) as f32) / 100.0);
                }
                RemoteCommand::SetLoopMode(mode) => {
                    let (repeat, shuffle) = mode.to_repeat_shuffle();
                    engine.set_repeat(repeat);
                    engine.set_shuffle(shuffle);
                }
                RemoteCommand::ToggleFavorite
                | RemoteCommand::RaiseWindow
                | RemoteCommand::QuitApp => {
                    // Handled by AppStore remote command listener.
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

    /// Removes all cached audio files from disk and clears in-memory resolved stream entries.
    pub fn clear_cache(&self) -> AudioResult<()> {
        clear_resolved_stream_cache(&self.inner.resolved_stream_cache);
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

    /// Lock-free monotonic counter incremented whenever [`snapshot`](Self::snapshot) changes.
    /// UI pollers can compare this before cloning `PlaybackSnapshot`.
    pub fn snapshot_version(&self) -> u64 {
        self.inner
            .signals
            .atomics
            .snapshot_version
            .load(Ordering::Acquire)
    }

    /// Lock-free live playback position in milliseconds directly from audio frame counters.
    pub fn position_ms(&self) -> u64 {
        self.inner.signals.atomics.current_position_ms()
    }

    /// Lock-free read of the latest 16-band spectrum levels (`0.0..=1.0`).
    pub fn spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        self.inner.signals.atomics.read_spectrum()
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

    #[cfg(test)]
    pub(crate) fn effective_volume_for_test(&self) -> f32 {
        f32::from_bits(
            self.inner
                .signals
                .atomics
                .volume_bits
                .load(Ordering::Acquire),
        )
    }

    #[cfg(test)]
    pub(crate) fn prefetched_targets_for_test(&self) -> Vec<PrefetchTarget> {
        self.inner
            .signals
            .prefetched_targets
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn clear_prefetched_targets_for_test(&self) {
        if let Ok(mut guard) = self.inner.signals.prefetched_targets.lock() {
            guard.clear();
        }
    }
}

struct EngineActor {
    config: AudioEngineConfig,
    output: Box<dyn AudioOutput>,
    output_started: bool,
    output_config: OutputConfig,
    cache: AudioCache,
    resolved_stream_cache: ResolvedStreamCache,
    remote: RemoteController,
    snapshot: PlaybackSnapshot,
    signals: Arc<EngineSignals>,
    ring: Arc<Mutex<FixedPcmRing>>,
    atomics: Arc<AudioAtomics>,
    decoder: Option<Box<dyn AudioDecoder>>,
    stream_resolver: Option<StreamResolver>,
    radio_refill: Option<RadioRefillCallback>,
    shuffle_order: Vec<usize>,
    shuffle_cursor: usize,
    shuffle_seed: u64,
    pcm_scratch: Vec<f32>,
    channel_scratch: Vec<f32>,
    resampled_scratch: Vec<f32>,
    overflow_pcm: Vec<f32>,
    overflow_offset: usize,
    crossfade_tail: Vec<f32>,
    crossfade_pos: usize,
    last_position_publish: Instant,
    last_remote_pos_sync: Instant,
    last_published_spectrum: [f32; SPECTRUM_BANDS],
    last_prefetched_id: Option<String>,
    active_prefetch: Option<(String, tokio::task::JoinHandle<AudioResult<PathBuf>>)>,
    paused_by_interrupt: bool,
    pending_pause_after_load: bool,
    ducked: bool,
}

impl EngineActor {
    fn new(
        config: AudioEngineConfig,
        output: Box<dyn AudioOutput>,
        cache: AudioCache,
        resolved_stream_cache: ResolvedStreamCache,
        remote: RemoteController,
        snapshot: PlaybackSnapshot,
        signals: Arc<EngineSignals>,
    ) -> Self {
        let atomics = Arc::clone(&signals.atomics);
        let ring = Arc::new(Mutex::new(FixedPcmRing::new(PCM_RING_CAPACITY)));
        Self {
            config,
            output,
            output_started: false,
            output_config: OutputConfig::default(),
            cache,
            resolved_stream_cache,
            remote,
            snapshot,
            signals,
            ring,
            atomics,
            decoder: None,
            stream_resolver: None,
            radio_refill: None,
            shuffle_order: Vec::new(),
            shuffle_cursor: 0,
            shuffle_seed: 0x9e37_79b9_7f4a_7c15,
            pcm_scratch: Vec::with_capacity(8192),
            channel_scratch: Vec::with_capacity(8192),
            resampled_scratch: Vec::with_capacity(8192),
            overflow_pcm: Vec::with_capacity(4096),
            overflow_offset: 0,
            crossfade_tail: Vec::new(),
            crossfade_pos: 0,
            last_position_publish: Instant::now(),
            last_remote_pos_sync: Instant::now(),
            last_published_spectrum: [0.0; SPECTRUM_BANDS],
            last_prefetched_id: None,
            active_prefetch: None,
            paused_by_interrupt: false,
            pending_pause_after_load: false,
            ducked: false,
        }
    }

    fn effective_volume(&self) -> f32 {
        if self.ducked {
            self.snapshot.volume * 0.25
        } else {
            self.snapshot.volume
        }
    }

    fn run(&mut self, rx: Receiver<EngineCommand>) {
        loop {
            if self.snapshot.status.is_playing() {
                match rx.recv_timeout(Duration::from_millis(16)) {
                    Ok(cmd) => {
                        if self.handle_command(cmd) {
                            break;
                        }
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
            } else if self.last_published_spectrum.iter().any(|&v| v > 0.001) {
                // Smoothly decay spectrum bars to zero after pausing, then park completely.
                match rx.recv_timeout(Duration::from_millis(33)) {
                    Ok(cmd) => {
                        if self.handle_command(cmd) {
                            break;
                        }
                        while let Ok(next_cmd) = rx.try_recv() {
                            if self.handle_command(next_cmd) {
                                let _ = self.output.stop();
                                return;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        for b in &mut self.last_published_spectrum {
                            *b *= 0.72;
                            if *b < 0.002 {
                                *b = 0.0;
                            }
                        }
                        self.atomics.write_spectrum(&self.last_published_spectrum);
                        self.atomics.spectrum_dirty.store(false, Ordering::Relaxed);
                        let _ = self
                            .signals
                            .spectrum_tx
                            .send_replace(self.last_published_spectrum);
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                // Idle/paused with zeroed spectrum: block with 0 wakeups and ~0% CPU.
                match rx.recv() {
                    Ok(cmd) => {
                        if self.handle_command(cmd) {
                            break;
                        }
                        while let Ok(next_cmd) = rx.try_recv() {
                            if self.handle_command(next_cmd) {
                                let _ = self.output.stop();
                                return;
                            }
                        }
                    }
                    Err(_) => break,
                }
                if self.snapshot.status.is_playing() {
                    self.tick_audio();
                }
            }
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
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
                self.snapshot.queue = vec![track];
                self.snapshot.current_index = Some(0);
                self.rebuild_shuffle_order(Some(0));
                self.load_current_track(explicit_stream);
            }
            EngineCommand::PlayQueue {
                tracks,
                start_index,
            } => {
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
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
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
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
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
                if self.snapshot.status.is_playing() {
                    self.pause_internal();
                }
            }
            EngineCommand::TogglePlay => {
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
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
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
                self.stop_internal();
                self.publish_snapshot();
            }
            EngineCommand::Seek(target_ms) => {
                self.seek_internal(target_ms);
            }
            EngineCommand::Next => {
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
                self.advance_next(true);
            }
            EngineCommand::Prev => {
                self.paused_by_interrupt = false;
                self.pending_pause_after_load = false;
                self.advance_prev();
            }
            EngineCommand::SetVolume(vol) => {
                self.snapshot.volume = vol.clamp(0.0, 1.0);
                self.atomics.set_volume(self.effective_volume());
                self.publish_snapshot();
            }
            EngineCommand::SetMuted(muted) => {
                self.snapshot.muted = muted;
                self.atomics.set_muted(muted);
                self.publish_snapshot();
            }
            EngineCommand::ToggleMute => {
                let next = !self.snapshot.muted;
                self.snapshot.muted = next;
                self.atomics.set_muted(next);
                self.publish_snapshot();
            }
            EngineCommand::SetShuffle(enabled) => {
                if self.snapshot.shuffle != enabled {
                    self.snapshot.shuffle = enabled;
                    self.rebuild_shuffle_order(self.snapshot.current_index);
                    self.publish_snapshot();
                    self.remote
                        .update_loop_mode(RemoteLoopMode::from_repeat_shuffle(
                            self.snapshot.repeat,
                            self.snapshot.shuffle,
                        ));
                }
            }
            EngineCommand::ToggleShuffle => {
                self.snapshot.shuffle = !self.snapshot.shuffle;
                self.rebuild_shuffle_order(self.snapshot.current_index);
                self.publish_snapshot();
                self.remote
                    .update_loop_mode(RemoteLoopMode::from_repeat_shuffle(
                        self.snapshot.repeat,
                        self.snapshot.shuffle,
                    ));
            }
            EngineCommand::SetRepeat(mode) => {
                self.snapshot.repeat = mode;
                self.publish_snapshot();
                self.remote
                    .update_loop_mode(RemoteLoopMode::from_repeat_shuffle(
                        self.snapshot.repeat,
                        self.snapshot.shuffle,
                    ));
            }
            EngineCommand::CycleRepeat => {
                self.snapshot.repeat = match self.snapshot.repeat {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::Off,
                };
                self.publish_snapshot();
                self.remote
                    .update_loop_mode(RemoteLoopMode::from_repeat_shuffle(
                        self.snapshot.repeat,
                        self.snapshot.shuffle,
                    ));
            }
            EngineCommand::SetQuality(quality) => {
                if self.snapshot.quality != quality {
                    self.snapshot.quality = quality;
                    clear_resolved_stream_cache(&self.resolved_stream_cache);
                    self.abort_active_prefetch();
                    self.last_prefetched_id = None;
                }
                self.publish_snapshot();
            }
            EngineCommand::SetReplayGainEnabled(enabled) => {
                self.snapshot.replay_gain_enabled = enabled;
                self.atomics.set_replay_gain(0.0, 1.0, enabled);
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
                if self.snapshot.current_track.is_some() {
                    self.sync_remote_metadata();
                }
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
                if self.snapshot.current_track.is_some() {
                    self.sync_remote_metadata();
                }
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
                    if self.snapshot.current_track.is_some() {
                        self.sync_remote_metadata();
                    }
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
            EngineCommand::OutputEvent(ev) => match ev {
                AudioOutputEvent::DeviceDisconnected => {
                    self.paused_by_interrupt = false;
                    if self.snapshot.status.is_playing() {
                        self.pause_internal();
                    } else if self.snapshot.status.is_loading() {
                        self.pending_pause_after_load = true;
                    }
                }
                AudioOutputEvent::SystemPause => {
                    if self.snapshot.status.is_playing() {
                        self.paused_by_interrupt = true;
                        self.pause_internal();
                    } else if self.snapshot.status.is_loading() {
                        self.paused_by_interrupt = true;
                        self.pending_pause_after_load = true;
                    }
                }
                AudioOutputEvent::SystemResume => {
                    if self.paused_by_interrupt
                        && self.snapshot.status.is_paused()
                        && self.decoder.is_some()
                    {
                        self.paused_by_interrupt = false;
                        self.resume_internal();
                    } else if self.paused_by_interrupt && self.snapshot.status.is_loading() {
                        self.paused_by_interrupt = false;
                        self.pending_pause_after_load = false;
                    }
                }
                AudioOutputEvent::Duck => {
                    self.ducked = true;
                    self.atomics.set_volume(self.effective_volume());
                }
                AudioOutputEvent::Unduck => {
                    self.ducked = false;
                    self.atomics.set_volume(self.effective_volume());
                }
            },
            EngineCommand::Shutdown => {
                self.stop_internal();
                return true;
            }
        }
        false
    }

    fn clear_ring_and_timeline(&mut self, base_position_ms: u64) {
        self.overflow_pcm.clear();
        self.overflow_offset = 0;
        if let Ok(mut ring) = self.ring.lock() {
            ring.clear();
        }
        self.atomics.ring_len.store(0, Ordering::Release);
        self.atomics.reset_timeline(base_position_ms);
    }

    fn ensure_output_started(&mut self, desired: OutputConfig) -> AudioResult<()> {
        if self.output_started {
            let _ = self.output.resume();
            return Ok(());
        }

        let ring = Arc::clone(&self.ring);
        let atomics = Arc::clone(&self.atomics);
        let initial_dsp_cfg = DspConfig {
            sample_rate: desired.sample_rate,
            channels: desired.channels,
            volume: self.effective_volume(),
            muted: self.snapshot.muted,
            replay_gain_enabled: self.snapshot.replay_gain_enabled,
            ..DspConfig::default()
        };

        // The audio render callback owns `DspProcessor` exclusively (zero mutex around DSP/FFT),
        // uses non-blocking `try_lock` solely for a sub-microsecond bulk ring memcpy, and
        // communicates all telemetry & controls via lock-free atomics.
        let mut dsp = DspProcessor::new(initial_dsp_cfg);
        let mut seen_dsp_gen = 0u64;
        let mut seen_rate = desired.sample_rate;
        let mut seen_channels = desired.channels;

        let negotiated = self.output.start(
            desired,
            Box::new(move |out: &mut [f32]| {
                out.fill(0.0);

                if atomics.pending_dsp_reset.swap(false, Ordering::AcqRel) {
                    dsp.reset();
                }

                let sr = atomics.sample_rate.load(Ordering::Relaxed).max(8_000);
                let ch_u16 = atomics.channels.load(Ordering::Relaxed).max(1) as u16;
                if sr != seen_rate || ch_u16 != seen_channels {
                    dsp.configure_stream(sr, ch_u16);
                    seen_rate = sr;
                    seen_channels = ch_u16;
                }

                let cur_gen = atomics.dsp_gen.load(Ordering::Acquire);
                if cur_gen != seen_dsp_gen {
                    dsp.set_volume(f32::from_bits(atomics.volume_bits.load(Ordering::Relaxed)));
                    dsp.set_muted(atomics.muted.load(Ordering::Relaxed));
                    dsp.set_replay_gain(
                        f32::from_bits(atomics.rg_gain_bits.load(Ordering::Relaxed)),
                        f32::from_bits(atomics.rg_peak_bits.load(Ordering::Relaxed)),
                        atomics.rg_enabled.load(Ordering::Relaxed),
                    );
                    seen_dsp_gen = cur_gen;
                }

                let fade_ms = atomics.pending_fade_in_ms.swap(0, Ordering::AcqRel);
                if fade_ms > 0 {
                    dsp.trigger_fade_in(fade_ms);
                }

                if !atomics.playing.load(Ordering::Acquire) {
                    return;
                }

                let written = match ring.try_lock() {
                    Ok(mut guard) => {
                        let n = guard.pop_slice(out);
                        atomics.ring_len.store(guard.len(), Ordering::Release);
                        n
                    }
                    Err(_) => {
                        // Producer is copying a chunk (<1us); output silence rather than
                        // blocking CoreAudio/WASAPI/AAudio/OHAudio real-time I/O thread.
                        return;
                    }
                };

                let ch = (ch_u16 as usize).max(1);
                if written > 0 {
                    atomics
                        .played_frames_since_seek
                        .fetch_add((written / ch) as u64, Ordering::Relaxed);
                    dsp.process_interleaved(out);
                    let spec = dsp.spectrum();
                    atomics.write_spectrum(&spec);
                } else if atomics.decoder_eof.load(Ordering::Acquire) {
                    atomics.track_ended.store(true, Ordering::Release);
                    dsp.decay_spectrum(0.8);
                    let spec = dsp.spectrum();
                    atomics.write_spectrum(&spec);
                } else {
                    dsp.decay_spectrum(0.9);
                    let spec = dsp.spectrum();
                    atomics.write_spectrum(&spec);
                }
            }),
        )?;

        self.output_started = true;
        self.output_config = negotiated;
        self.snapshot.output_backend = self.output.backend_name().to_owned();
        self.atomics
            .sample_rate
            .store(negotiated.sample_rate, Ordering::Release);
        self.atomics
            .channels
            .store(negotiated.channels as u32, Ordering::Release);
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
            && let Ok(mut ring) = self.ring.lock()
        {
            let max_xfade_samples = ((self.snapshot.crossfade_ms as usize)
                * (self.output_config.sample_rate as usize)
                / 1000)
                * (self.output_config.channels as usize).max(1);
            ring.drain_into(&mut self.crossfade_tail, max_xfade_samples);
            self.atomics.ring_len.store(ring.len(), Ordering::Release);
        }

        self.snapshot.current_track = Some(track.clone());
        self.snapshot.status = PlaybackStatus::Loading;
        self.snapshot.position_ms = 0;
        self.snapshot.duration_ms = track.duration.max(0) as u64;
        self.snapshot.buffered_ms = 0;
        self.snapshot.is_lossless = false;
        self.snapshot.stream_format.clear();
        self.publish_snapshot();
        // Publish Buffering state to OS media controls immediately before blocking
        // stream resolution so notification bar / Control Center updates right away.
        self.sync_remote_metadata();

        // Abort any still-in-flight background prefetch (immediately closing any loopback
        // `ffmpeg -listen 1` TCP socket so the previous relay exits cleanly without contention).
        self.abort_active_prefetch();

        let duration_hint = if track.duration > 0 {
            Some(track.duration as u64)
        } else {
            None
        };
        let logical_key = cache_key_for_track(&track.source, &track.id, self.snapshot.quality);

        // Tier 1: Logical track disk-cache hit (0ms, zero network / stream_resolver calls).
        let mut opened: Option<(StreamInfo, Box<dyn AudioDecoder>)> = None;
        if explicit_stream.is_none()
            && let Some((cached_path, cached_meta)) =
                self.cache
                    .get_cached_track(&track.source, &track.id, self.snapshot.quality)
        {
            let cached_url = cached_path.to_string_lossy().into_owned();
            let cached_media_type = if cached_meta.media_type.is_empty() {
                None
            } else {
                Some(cached_meta.media_type.as_str())
            };
            match open_decoder_with_key(
                &self.cache,
                &cached_url,
                Vec::new(),
                cached_media_type,
                duration_hint,
                self.config.decoder_preference,
                None,
                None,
            ) {
                Ok(dec) => {
                    log::info!(
                        "audio disk cache hit for '{}' ({}) -> {:?}",
                        track.name,
                        track.id,
                        cached_path
                    );
                    let cached_info = StreamInfo {
                        url: cached_url,
                        media_type: cached_meta.media_type,
                        quality: self.snapshot.quality.as_str().to_owned(),
                        bit_rate: cached_meta.bit_rate as i64,
                        gain: cached_meta.gain,
                        peak: cached_meta.peak,
                        ..StreamInfo::default()
                    };
                    opened = Some((cached_info, dec));
                }
                Err(err) => {
                    log::warn!(
                        "cached audio file {:?} failed to open for '{}' ({}): {err}; falling back to resolver",
                        cached_path,
                        track.name,
                        track.id
                    );
                }
            }
        }

        // Tier 2: Fresh pre-resolved StreamInfo hit from `maybe_prefetch_next` (0ms, skips
        // duplicate `YouTube.js`/`yt-dlp` extraction; falls back transparently if URL expired).
        if opened.is_none()
            && explicit_stream.is_none()
            && let Some(ref lk) = logical_key
            && let Some(cached_info) = get_fresh_resolved_stream(&self.resolved_stream_cache, lk)
        {
            let headers = collect_stream_headers(&cached_info.headers);
            let media_type = if cached_info.media_type.is_empty() {
                None
            } else {
                Some(cached_info.media_type.as_str())
            };
            let meta = stream_meta_from_info(&cached_info);
            match open_decoder_with_key(
                &self.cache,
                &cached_info.url,
                headers,
                media_type,
                duration_hint,
                self.config.decoder_preference,
                Some(lk.as_str()),
                Some(meta),
            ) {
                Ok(dec) => {
                    log::info!(
                        "reused pre-resolved stream for '{}' ({}): target={} media_type={:?}",
                        track.name,
                        track.id,
                        sanitize_url_for_log(&cached_info.url),
                        cached_info.media_type
                    );
                    opened = Some((cached_info, dec));
                }
                Err(err) => {
                    log::warn!(
                        "pre-resolved stream failed for '{}' ({}) target={}: {err}; evicting and re-resolving",
                        track.name,
                        track.id,
                        sanitize_url_for_log(&cached_info.url)
                    );
                    evict_resolved_stream(&self.resolved_stream_cache, lk);
                }
            }
        }

        // Tier 3: Caller-supplied explicit stream or fresh `stream_resolver` lookup.
        let (stream_info, decoder) = if let Some(pair) = opened {
            pair
        } else {
            let resolved = match explicit_stream {
                Some(stream) => Ok(stream),
                None => self.resolve_track_stream(&track),
            };

            let stream_info = match resolved {
                Ok(info) => {
                    log::info!(
                        "resolved stream for '{}' ({}): target={} media_type={:?}",
                        track.name,
                        track.id,
                        sanitize_url_for_log(&info.url),
                        info.media_type
                    );
                    info
                }
                Err(err) => {
                    log::warn!(
                        "failed to resolve stream for '{}' ({}): {err}",
                        track.name,
                        track.id
                    );
                    self.decoder = None;
                    self.snapshot.status = PlaybackStatus::Error(err);
                    self.atomics.playing.store(false, Ordering::Release);
                    self.clear_ring_and_timeline(0);
                    self.publish_snapshot();
                    self.remote.update_playback_full(
                        RemotePlaybackStatus::Stopped,
                        0,
                        self.snapshot.duration_ms,
                        0,
                    );
                    return;
                }
            };

            if let Some(ref lk) = logical_key {
                insert_resolved_stream(
                    &self.resolved_stream_cache,
                    lk.clone(),
                    stream_info.clone(),
                );
            }

            let headers = collect_stream_headers(&stream_info.headers);
            let media_type = if stream_info.media_type.is_empty() {
                None
            } else {
                Some(stream_info.media_type.as_str())
            };
            let meta = logical_key
                .as_ref()
                .map(|_| stream_meta_from_info(&stream_info));

            let decoder = match open_decoder_with_key(
                &self.cache,
                &stream_info.url,
                headers,
                media_type,
                duration_hint,
                self.config.decoder_preference,
                logical_key.as_deref(),
                meta,
            ) {
                Ok(dec) => dec,
                Err(err) => {
                    log::warn!(
                        "failed to open decoder for '{}' ({}) target={}: {err}",
                        track.name,
                        track.id,
                        sanitize_url_for_log(&stream_info.url)
                    );
                    self.decoder = None;
                    self.snapshot.status = PlaybackStatus::Error(err.to_string());
                    self.atomics.playing.store(false, Ordering::Release);
                    self.clear_ring_and_timeline(0);
                    self.publish_snapshot();
                    self.remote.update_playback_full(
                        RemotePlaybackStatus::Stopped,
                        0,
                        self.snapshot.duration_ms,
                        0,
                    );
                    return;
                }
            };
            (stream_info, decoder)
        };

        let spec = decoder.spec();
        if let Err(err) = self.ensure_output_started(OutputConfig {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        }) {
            self.decoder = None;
            self.snapshot.status = PlaybackStatus::Error(err.to_string());
            self.publish_snapshot();
            self.remote.update_playback_full(
                RemotePlaybackStatus::Stopped,
                0,
                self.snapshot.duration_ms,
                0,
            );
            return;
        }

        let (is_lossless, stream_format) =
            classify_stream_format(&stream_info, &spec, self.snapshot.quality);

        let duration_ms = spec
            .duration_ms
            .filter(|&d| d > 0)
            .or(duration_hint)
            .unwrap_or(0);
        self.snapshot.duration_ms = duration_ms;
        self.snapshot.position_ms = 0;
        self.snapshot.buffered_ms = duration_ms;
        self.snapshot.is_lossless = is_lossless;
        self.snapshot.stream_format = stream_format;
        self.snapshot.decoder_backend = decoder.backend_name().to_owned();
        self.decoder = Some(decoder);

        self.atomics.playing.store(false, Ordering::Release);
        self.clear_ring_and_timeline(0);
        self.atomics.set_replay_gain(
            stream_info.gain as f32,
            if stream_info.peak > 0.0 {
                stream_info.peak as f32
            } else {
                1.0
            },
            self.snapshot.replay_gain_enabled,
        );

        if self.pending_pause_after_load {
            self.pending_pause_after_load = false;
            if let Err(err) = self.output.pause() {
                log::warn!("audio output pause failed after deferred interrupt: {err}");
            }
            self.snapshot.status = PlaybackStatus::Paused;
            self.publish_snapshot();
            self.sync_remote_metadata();
            self.maybe_prefetch_next();
            self.maybe_refill_radio();
            return;
        }

        self.snapshot.status = PlaybackStatus::Playing;
        self.atomics.trigger_fade_in(12);

        // Prime ring buffer immediately before enabling playback so first callback never underruns.
        self.fill_decoder_buffer();
        self.atomics.playing.store(true, Ordering::Release);
        self.publish_snapshot();
        self.sync_remote_metadata();
        self.maybe_prefetch_next();
        self.maybe_refill_radio();
    }

    fn resolve_track_stream(&self, track: &Track) -> Result<StreamInfo, String> {
        resolve_track_stream_with_resolver(
            self.stream_resolver.as_ref(),
            track,
            self.snapshot.quality,
        )
    }

    fn pause_internal(&mut self) {
        self.atomics.playing.store(false, Ordering::Release);
        let _ = self.output.pause();
        self.snapshot.status = PlaybackStatus::Paused;
        self.publish_snapshot();
        self.remote.update_playback_full(
            RemotePlaybackStatus::Paused,
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
            self.snapshot.buffered_ms,
        );
        self.last_remote_pos_sync = Instant::now();
    }

    fn resume_internal(&mut self) {
        self.atomics.trigger_fade_in(10);
        self.atomics.playing.store(true, Ordering::Release);
        if let Err(err) = self.output.resume() {
            log::warn!("audio output resume failed: {err}");
        }
        self.snapshot.status = PlaybackStatus::Playing;
        self.publish_snapshot();
        self.remote.update_playback_full(
            RemotePlaybackStatus::Playing,
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
            self.snapshot.buffered_ms,
        );
        self.last_remote_pos_sync = Instant::now();
    }

    fn stop_internal(&mut self) {
        self.abort_active_prefetch();
        self.decoder = None;
        self.crossfade_tail.clear();
        self.crossfade_pos = 0;
        self.atomics.playing.store(false, Ordering::Release);
        self.clear_ring_and_timeline(0);
        self.atomics.request_dsp_reset();
        self.atomics.clear_spectrum();
        self.atomics.spectrum_dirty.store(false, Ordering::Relaxed);
        if self.output_started {
            let _ = self.output.pause();
        }
        self.snapshot.status = PlaybackStatus::Stopped;
        self.snapshot.position_ms = 0;
        self.snapshot.is_lossless = false;
        self.snapshot.stream_format.clear();
        self.last_published_spectrum = [0.0; SPECTRUM_BANDS];
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
            self.atomics.reset_timeline(clamped);
            self.publish_snapshot();
            return;
        };

        match decoder.seek(clamped) {
            Ok(landed_ms) => {
                log::info!("seeked to {clamped}ms -> landed {landed_ms}ms");
                self.crossfade_tail.clear();
                self.crossfade_pos = 0;
                self.clear_ring_and_timeline(landed_ms);
                self.atomics.trigger_fade_in(8);
                self.snapshot.position_ms = landed_ms;
                self.fill_decoder_buffer();
                self.publish_snapshot();
                self.remote.update_playback_full(
                    self.remote_playback_status(),
                    landed_ms,
                    self.snapshot.duration_ms,
                    self.snapshot.buffered_ms,
                );
                self.last_remote_pos_sync = Instant::now();
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

        if self.atomics.spectrum_dirty.swap(false, Ordering::AcqRel) {
            let spectrum = self.atomics.read_spectrum();
            if spectrum != self.last_published_spectrum {
                self.last_published_spectrum = spectrum;
                let _ = self.signals.spectrum_tx.send_replace(spectrum);
            }
        }

        if self.atomics.track_ended.swap(false, Ordering::AcqRel)
            && self.snapshot.status.is_playing()
        {
            self.on_track_finished();
            return;
        }

        if self.snapshot.status.is_playing()
            && self.last_position_publish.elapsed() >= Duration::from_millis(50)
        {
            self.last_position_publish = Instant::now();
            let pos_ms = self.atomics.current_position_ms();
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

        if self.snapshot.status == PlaybackStatus::Playing
            && self.last_remote_pos_sync.elapsed() >= Duration::from_secs(5)
        {
            self.remote.update_playback_full(
                RemotePlaybackStatus::Playing,
                self.snapshot.position_ms,
                self.snapshot.duration_ms,
                self.snapshot.buffered_ms,
            );
            self.last_remote_pos_sync = Instant::now();
        }
    }

    fn flush_overflow_to_ring(&mut self) -> bool {
        if self.overflow_offset >= self.overflow_pcm.len() {
            self.overflow_pcm.clear();
            self.overflow_offset = 0;
            return true;
        }
        let Ok(mut ring) = self.ring.lock() else {
            return false;
        };
        let pushed = ring.push_slice(&self.overflow_pcm[self.overflow_offset..]);
        self.atomics.ring_len.store(ring.len(), Ordering::Release);
        self.overflow_offset += pushed;
        if self.overflow_offset >= self.overflow_pcm.len() {
            self.overflow_pcm.clear();
            self.overflow_offset = 0;
            true
        } else {
            false
        }
    }

    fn fill_decoder_buffer(&mut self) {
        if self.decoder.is_none() {
            return;
        }
        let target_samples = ((self.output_config.sample_rate as usize / 2)
            * (self.output_config.channels as usize).max(1))
        .min(PCM_RING_CAPACITY.saturating_sub(8192));

        // Lock-free fast check: skip decoding if ring is already well-buffered.
        if self.atomics.ring_len.load(Ordering::Acquire) >= target_samples {
            return;
        }

        if !self.flush_overflow_to_ring() {
            return;
        }

        loop {
            if self.atomics.ring_len.load(Ordering::Acquire) >= target_samples {
                break;
            }

            let Some(ref mut decoder) = self.decoder else {
                break;
            };
            self.pcm_scratch.clear();
            let spec = decoder.spec();
            match decoder.read_pcm(&mut self.pcm_scratch) {
                Ok(0) => {
                    self.atomics.decoder_eof.store(true, Ordering::Release);
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
                        &mut self.channel_scratch,
                        &mut self.resampled_scratch,
                    );

                    // Apply zero-allocation in-place equal-power crossfade against outgoing tail.
                    if self.crossfade_pos < self.crossfade_tail.len() {
                        let tail_rem = self.crossfade_tail.len() - self.crossfade_pos;
                        let overlap = tail_rem.min(self.resampled_scratch.len());
                        let total_xfade = self.crossfade_tail.len().max(1) as f32;
                        let progress =
                            (self.crossfade_pos as f32 + overlap as f32 * 0.5) / total_xfade;
                        mix_crossfade_in_place(
                            &self.crossfade_tail[self.crossfade_pos..self.crossfade_pos + overlap],
                            &mut self.resampled_scratch[..overlap],
                            progress,
                        );
                        self.crossfade_pos += overlap;
                        if self.crossfade_pos >= self.crossfade_tail.len() {
                            self.crossfade_tail.clear();
                            self.crossfade_pos = 0;
                        }
                    }

                    let pushed = if let Ok(mut ring) = self.ring.lock() {
                        let n = ring.push_slice(&self.resampled_scratch);
                        self.atomics.ring_len.store(ring.len(), Ordering::Release);
                        n
                    } else {
                        break;
                    };

                    if pushed < self.resampled_scratch.len() {
                        self.overflow_pcm.clear();
                        self.overflow_pcm
                            .extend_from_slice(&self.resampled_scratch[pushed..]);
                        self.overflow_offset = 0;
                        break;
                    }
                }
                Err(err) => {
                    log::warn!("audio decode error: {err}");
                    self.atomics.decoder_eof.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn on_track_finished(&mut self) {
        if self.snapshot.repeat == RepeatMode::One {
            self.seek_internal(0);
            self.atomics.decoder_eof.store(false, Ordering::Release);
            self.atomics.track_ended.store(false, Ordering::Release);
            self.atomics.playing.store(true, Ordering::Release);
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
            if self.snapshot.repeat == RepeatMode::All
                || (manual_skip && self.snapshot.repeat == RepeatMode::One)
            {
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

    fn abort_active_prefetch(&mut self) {
        if let Some((_key, handle)) = self.active_prefetch.take()
            && !handle.is_finished()
        {
            handle.abort();
        }
    }

    fn maybe_prefetch_next(&mut self) {
        if !self.config.prefetch_next || self.cache.max_bytes() == 0 {
            return;
        }
        let Some(next_idx) = self.next_queue_index(false) else {
            return;
        };
        let Some(next_track) = self.snapshot.queue.get(next_idx).cloned() else {
            return;
        };
        if is_local_track_for_prefetch(&next_track) {
            return;
        }
        let quality = self.snapshot.quality;
        // Skip network resolution and prefetch entirely if the next track is already cached on disk.
        if self
            .cache
            .get_cached_track(&next_track.source, &next_track.id, quality)
            .is_some()
        {
            return;
        }

        let logical_key = cache_key_for_track(&next_track.source, &next_track.id, quality);
        let fallback_key = if !next_track.id.is_empty() {
            next_track.id.as_str()
        } else {
            next_track.url.as_str()
        };
        let prefetch_key = logical_key.as_deref().unwrap_or(fallback_key);
        if prefetch_key.is_empty() || self.last_prefetched_id.as_deref() == Some(prefetch_key) {
            return;
        }
        let prefetch_key_owned = prefetch_key.to_owned();
        self.last_prefetched_id = Some(prefetch_key_owned.clone());
        self.abort_active_prefetch();

        let cache = self.cache.clone();
        let resolved_cache = Arc::clone(&self.resolved_stream_cache);
        #[cfg(test)]
        let prefetch_log = Arc::clone(&self.signals.prefetched_targets);

        if let Some(resolver) = self.stream_resolver.clone() {
            if next_track.id.is_empty() {
                return;
            }
            let handle = crate::cache::io_runtime_handle().spawn(async move {
                let resolved = tokio::task::spawn_blocking(move || {
                    resolve_prefetch_stream(Some(&resolver), &next_track, quality)
                })
                .await
                .ok()
                .flatten();
                let Some(info) = resolved else {
                    return Err(AudioError::Other(
                        "prefetch stream resolution skipped".into(),
                    ));
                };
                let headers = collect_stream_headers(&info.headers);
                let meta = stream_meta_from_info(&info);
                if let Some(ref lk) = logical_key {
                    insert_resolved_stream(&resolved_cache, lk.clone(), info.clone());
                }
                #[cfg(test)]
                if let Ok(mut log) = prefetch_log.lock() {
                    log.push((info.url.clone(), headers.clone()));
                }
                cache
                    .prefetch_with_key_async(info.url, headers, None, logical_key, Some(meta))
                    .await
            });
            self.active_prefetch = Some((prefetch_key_owned, handle));
        } else if let Some(info) = resolve_prefetch_stream(None, &next_track, quality) {
            let headers = collect_stream_headers(&info.headers);
            let meta = stream_meta_from_info(&info);
            if let Some(ref lk) = logical_key {
                insert_resolved_stream(&resolved_cache, lk.clone(), info.clone());
            }
            #[cfg(test)]
            if let Ok(mut log) = prefetch_log.lock() {
                log.push((info.url.clone(), headers.clone()));
            }
            let handle = cache.prefetch_with_key(info.url, headers, None, logical_key, Some(meta));
            self.active_prefetch = Some((prefetch_key_owned, handle));
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
        if self.snapshot.current_track.is_some() {
            self.sync_remote_metadata();
        }
        true
    }

    fn remote_playback_status(&self) -> RemotePlaybackStatus {
        match self.snapshot.status {
            PlaybackStatus::Loading => RemotePlaybackStatus::Buffering,
            PlaybackStatus::Playing => RemotePlaybackStatus::Playing,
            PlaybackStatus::Paused => RemotePlaybackStatus::Paused,
            PlaybackStatus::Stopped | PlaybackStatus::Error(_) => RemotePlaybackStatus::Stopped,
        }
    }

    fn sync_remote_metadata(&mut self) {
        let Some(ref track) = self.snapshot.current_track else {
            self.remote.clear();
            return;
        };
        let can_next = self.next_queue_index(true).is_some() || self.radio_refill.is_some();
        let can_prev = !self.snapshot.queue.is_empty();
        let status = self.remote_playback_status();
        let loop_mode =
            RemoteLoopMode::from_repeat_shuffle(self.snapshot.repeat, self.snapshot.shuffle);
        self.remote.set_track_state(
            track,
            status,
            self.snapshot.position_ms,
            self.snapshot.duration_ms,
            self.snapshot.buffered_ms,
            can_next,
            can_prev,
            loop_mode,
        );
        self.last_remote_pos_sync = Instant::now();
    }

    fn publish_snapshot(&self) {
        let snap = self.snapshot.clone();
        self.atomics
            .snapshot_version
            .fetch_add(1, Ordering::Release);
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

/// Normalizes a format/MIME/codec label into a concise lowercase format identifier
/// (e.g. `"flac"`, `"wav"`, `"alac"`, `"mp3"`, `"aac"`, `"opus"`, `"vorbis"`).
pub(crate) fn normalize_stream_format(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    let full_lower = trimmed.to_ascii_lowercase();
    if full_lower.contains("opus") {
        return Some("opus".to_owned());
    }
    let lower = trimmed
        .split(';')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }

    if lower.contains("flac") {
        return Some("flac".to_owned());
    }
    if lower.contains("alac") {
        return Some("alac".to_owned());
    }
    if lower.contains("wav") || lower.starts_with("pcm") {
        return Some("wav".to_owned());
    }
    if lower.contains("aiff") || lower == "aif" {
        return Some("aiff".to_owned());
    }
    if lower.contains("ape") || lower.contains("monkey") {
        return Some("ape".to_owned());
    }
    if lower.contains("wavpack") || lower == "wv" {
        return Some("wavpack".to_owned());
    }
    if lower.contains("dsf") || lower.contains("dff") || lower.contains("dsd") {
        return Some("dsd".to_owned());
    }
    if lower.contains("mp3") || lower.contains("mpeg") {
        return Some("mp3".to_owned());
    }
    if lower.contains("aac") {
        return Some("aac".to_owned());
    }
    if lower.contains("m4a") || lower.contains("mp4") {
        return Some("m4a".to_owned());
    }
    if lower.contains("opus") {
        return Some("opus".to_owned());
    }
    if lower.contains("vorbis") {
        return Some("vorbis".to_owned());
    }
    if lower.contains("ogg") {
        return Some("ogg".to_owned());
    }
    if lower.contains("webm") || lower.contains("matroska") || lower == "mka" {
        return Some("webm".to_owned());
    }
    if lower.contains("wma") {
        return Some("wma".to_owned());
    }
    if lower.contains("eac3") || lower.contains("ac3") {
        return Some("ac3".to_owned());
    }
    if lower == "demo-synth" {
        return Some("demo-synth".to_owned());
    }

    // Drop opaque numeric/debug strings like "0x..." or "CodecType(...)"
    if lower.starts_with("0x") || lower.starts_with("codectype") {
        return None;
    }

    let token = lower.rsplit('/').next().unwrap_or(&lower);
    if !token.is_empty() && token.len() <= 16 {
        Some(token.to_owned())
    } else {
        None
    }
}

fn is_lossless_format(fmt: &str) -> bool {
    matches!(
        fmt,
        "flac" | "wav" | "alac" | "aiff" | "pcm" | "ape" | "wavpack" | "tta" | "dsd"
    )
}

fn is_known_lossy_format(fmt: &str) -> bool {
    matches!(
        fmt,
        "mp3" | "aac" | "m4a" | "opus" | "vorbis" | "ogg" | "webm" | "wma" | "ac3"
    )
}

/// Classifies `(is_lossless, stream_format)` from the resolved [`StreamInfo`],
/// decoded [`AudioSpec`], and active [`Quality`] setting.
pub(crate) fn classify_stream_format(
    stream_info: &StreamInfo,
    spec: &AudioSpec,
    requested_quality: Quality,
) -> (bool, String) {
    let media_fmt = normalize_stream_format(&stream_info.media_type);
    let codec_fmt = normalize_stream_format(&spec.codec_name);
    let url_ext =
        infer_extension_hint(&stream_info.url, None).and_then(|ext| normalize_stream_format(&ext));

    // Prefer codec over generic container (e.g. ALAC inside `.m4a` -> `"alac"`,
    // AAC inside `.m4a` -> `"aac"`), falling back to MIME/media_type and URL extension.
    let stream_format = match (&codec_fmt, &media_fmt, &url_ext) {
        (Some(c), _, _) if is_lossless_format(c) => c.clone(),
        (_, Some(m), _) if is_lossless_format(m) => m.clone(),
        (_, _, Some(u)) if is_lossless_format(u) => u.clone(),
        (Some(c), _, _) if c != "demo-synth" => c.clone(),
        (_, Some(m), _) => m.clone(),
        (_, _, Some(u)) => u.clone(),
        (Some(c), _, _) => c.clone(),
        _ => String::new(),
    };

    let quality_str = stream_info.quality.trim().to_ascii_lowercase();
    let stream_quality_lossless = matches!(
        quality_str.as_str(),
        "lossless" | "flac" | "alac" | "wav" | "hires" | "hi-res" | "sq" | "hr"
    ) || quality_str.contains("lossless");

    let is_lossless = is_lossless_format(&stream_format)
        || stream_quality_lossless
        || (quality_str.is_empty()
            && requested_quality == Quality::Lossless
            && !is_known_lossy_format(&stream_format));

    (is_lossless, stream_format)
}

/// Converts interleaved PCM from `(src_rate, src_ch)` to `(dst_rate, dst_ch)` using
/// channel mapping and linear interpolation resampling when rates differ, reusing
/// `channel_scratch` to avoid per-packet heap allocations.
pub(crate) fn convert_pcm(
    input: &[f32],
    src_rate: u32,
    src_ch: u16,
    dst_rate: u32,
    dst_ch: u16,
    channel_scratch: &mut Vec<f32>,
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
        out.extend_from_slice(&input[..src_frames * sc]);
        return;
    }

    // Zero-copy slice when channel counts match (common 44.1kHz <-> 48kHz stereo on macOS).
    let src_slice: &[f32] = if sc == dc {
        &input[..src_frames * sc]
    } else {
        channel_scratch.clear();
        channel_scratch.reserve(src_frames * dc);
        for frame in input[..src_frames * sc].chunks_exact(sc) {
            match (sc, dc) {
                (1, 2) => {
                    channel_scratch.push(frame[0]);
                    channel_scratch.push(frame[0]);
                }
                (_, 1) => {
                    let avg = frame.iter().copied().sum::<f32>() / sc as f32;
                    channel_scratch.push(avg);
                }
                (_, 2) => {
                    channel_scratch.push(frame[0]);
                    channel_scratch.push(frame[1]);
                }
                _ => {
                    for c in 0..dc {
                        channel_scratch.push(frame[c % sc]);
                    }
                }
            }
        }
        channel_scratch.as_slice()
    };

    if src_rate == dst_rate || src_rate == 0 || dst_rate == 0 {
        out.extend_from_slice(src_slice);
        return;
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let dst_frames = ((src_frames as f64) / ratio).round().max(1.0) as usize;
    out.reserve(dst_frames * dc);

    let last_frame = src_frames - 1;
    if dc == 2 {
        for out_f in 0..dst_frames {
            let src_pos = (out_f as f64) * ratio;
            let idx0 = (src_pos as usize).min(last_frame);
            let idx1 = (idx0 + 1).min(last_frame);
            let frac = (src_pos - idx0 as f64) as f32;
            let base0 = idx0 * 2;
            let base1 = idx1 * 2;
            let l0 = src_slice[base0];
            let r0 = src_slice[base0 + 1];
            out.push(l0 + (src_slice[base1] - l0) * frac);
            out.push(r0 + (src_slice[base1 + 1] - r0) * frac);
        }
    } else {
        for out_f in 0..dst_frames {
            let src_pos = (out_f as f64) * ratio;
            let idx0 = (src_pos as usize).min(last_frame);
            let idx1 = (idx0 + 1).min(last_frame);
            let frac = (src_pos - idx0 as f64) as f32;
            for c in 0..dc {
                let s0 = src_slice[idx0 * dc + c];
                let s1 = src_slice[idx1 * dc + c];
                out.push(s0 + (s1 - s0) * frac);
            }
        }
    }
}

/// Formats a URL for info/warn logs using only scheme + host (+ port), stripping paths,
/// query strings, and credentials so tokens/cookies never leak into logs.
fn sanitize_url_for_log(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty>".to_owned();
    }
    if let Ok(parsed) = url::Url::parse(trimmed) {
        let scheme = parsed.scheme();
        if scheme == "http" || scheme == "https" {
            let host = parsed.host_str().unwrap_or("<unknown>");
            return match parsed.port() {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            };
        }
        return format!("{scheme}://<local>");
    }
    "<local-path>".to_owned()
}

fn is_local_track_for_prefetch(track: &Track) -> bool {
    track.source == "local" || track.url.starts_with("file://") || track.url.starts_with('/')
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn collect_stream_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        if let Some(existing) = out.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(k)) {
            *existing = (k.clone(), v.clone());
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

fn stream_meta_from_info(info: &StreamInfo) -> CachedStreamMeta {
    CachedStreamMeta {
        media_type: info.media_type.clone(),
        bit_rate: info.bit_rate.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        gain: info.gain,
        peak: info.peak,
    }
}

fn is_cacheable_resolved_stream_url(url: &str) -> bool {
    is_http_url(url)
        && !url::Url::parse(url)
            .ok()
            .as_ref()
            .is_some_and(is_loopback_url)
}

fn insert_resolved_stream(
    cache: &Mutex<HashMap<String, (StreamInfo, Instant)>>,
    key: String,
    info: StreamInfo,
) {
    if !is_cacheable_resolved_stream_url(&info.url) {
        return;
    }
    let Ok(mut guard) = cache.lock() else {
        return;
    };
    let now = Instant::now();
    guard.retain(|_, (_, ts)| now.saturating_duration_since(*ts) < RESOLVED_STREAM_CACHE_TTL);
    if !guard.contains_key(&key)
        && guard.len() >= RESOLVED_STREAM_CACHE_MAX_ENTRIES
        && let Some(oldest_key) = guard
            .iter()
            .min_by_key(|(_, (_, ts))| *ts)
            .map(|(k, _)| k.clone())
    {
        guard.remove(&oldest_key);
    }
    guard.insert(key, (info, now));
}

fn get_fresh_resolved_stream(
    cache: &Mutex<HashMap<String, (StreamInfo, Instant)>>,
    key: &str,
) -> Option<StreamInfo> {
    let mut guard = cache.lock().ok()?;
    if let Some((info, ts)) = guard.get(key) {
        if ts.elapsed() < RESOLVED_STREAM_CACHE_TTL {
            return Some(info.clone());
        }
        guard.remove(key);
    }
    None
}

fn evict_resolved_stream(cache: &Mutex<HashMap<String, (StreamInfo, Instant)>>, key: &str) {
    if let Ok(mut guard) = cache.lock() {
        guard.remove(key);
    }
}

fn clear_resolved_stream_cache(cache: &Mutex<HashMap<String, (StreamInfo, Instant)>>) {
    if let Ok(mut guard) = cache.lock() {
        guard.clear();
    }
}

/// Resolves the full [`StreamInfo`] for background prefetching of the next queued track.
///
/// - Skips local tracks (`source == "local"`, `file://`, or `/...`).
/// - When `stream_resolver` is `Some`, calls it first (when `next_track.id` is non-empty)
///   and never falls back to `next_track.url` on error or empty/non-HTTP results (avoiding
///   prefetching HTML watch pages like YouTube Music's `track.url`).
/// - Only uses `next_track.url` directly when `stream_resolver` is `None`.
fn resolve_prefetch_stream(
    stream_resolver: Option<&StreamResolver>,
    next_track: &Track,
    quality: Quality,
) -> Option<StreamInfo> {
    if is_local_track_for_prefetch(next_track) {
        return None;
    }
    if let Some(resolver) = stream_resolver {
        if next_track.id.is_empty() {
            return None;
        }
        let info = resolver(next_track, quality).ok()?;
        if !is_http_url(&info.url) {
            return None;
        }
        return Some(info);
    }
    if is_http_url(&next_track.url) {
        Some(StreamInfo {
            url: next_track.url.clone(),
            ..StreamInfo::default()
        })
    } else {
        None
    }
}

/// Resolves the HTTP(S) prefetch target `(url, headers)` for the next queued track.
#[cfg(test)]
fn resolve_prefetch_target(
    stream_resolver: Option<&StreamResolver>,
    next_track: &Track,
    quality: Quality,
) -> Option<PrefetchTarget> {
    let info = resolve_prefetch_stream(stream_resolver, next_track, quality)?;
    Some((info.url, collect_stream_headers(&info.headers)))
}

fn resolve_track_stream_with_resolver(
    stream_resolver: Option<&StreamResolver>,
    track: &Track,
    quality: Quality,
) -> Result<StreamInfo, String> {
    if let Some(resolver) = stream_resolver {
        match resolver(track, quality) {
            Ok(info) if !info.url.is_empty() => return Ok(info),
            Ok(_) => {}
            Err(SourceError::Unsupported) => {
                // `Unsupported` explicitly allows fallback to `track.local_path` / `track.url`.
            }
            Err(err) => {
                log::warn!(
                    "stream_resolver failed for '{}' ({}): {err}",
                    track.name,
                    track.id
                );
                // Only fall back to a local file path on real resolver errors; NEVER fall back
                // to `track.url` (which for plugins like YouTube Music is an HTML watch page).
                if !track.local_path.is_empty() {
                    return Ok(StreamInfo {
                        url: track.local_path.clone(),
                        ..StreamInfo::default()
                    });
                }
                return Err(err.to_string());
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nsh_music_core::model::{Quality, SourceError, Track};

    use super::{StreamResolver, resolve_track_stream_with_resolver, sanitize_url_for_log};

    #[test]
    fn test_resolve_track_stream_does_not_fallback_to_webpage_url_on_error() {
        let resolver: StreamResolver =
            Arc::new(|_, _| Err(SourceError::Message("upstream failed".into())));
        let track = Track {
            id: "0dGXCvLIffY".to_owned(),
            source: "youtube-music".to_owned(),
            name: "Test Song".to_owned(),
            url: "https://music.youtube.com/watch?v=0dGXCvLIffY".to_owned(),
            local_path: String::new(),
            ..Track::default()
        };

        let err = resolve_track_stream_with_resolver(Some(&resolver), &track, Quality::High)
            .expect_err("non-Unsupported resolver error must NOT fall back to webpage track.url");
        assert_eq!(err, "upstream failed");
    }

    #[test]
    fn test_resolve_track_stream_allows_unsupported_and_local_path_fallbacks() {
        // Case 1: `Err(SourceError::Unsupported)` falls back to `track.url`.
        let unsupported_resolver: StreamResolver = Arc::new(|_, _| Err(SourceError::Unsupported));
        let remote_track = Track {
            id: "direct-1".to_owned(),
            source: "direct".to_owned(),
            name: "Direct Track".to_owned(),
            url: "https://cdn.example.test/song.mp3".to_owned(),
            ..Track::default()
        };
        let resolved_remote = resolve_track_stream_with_resolver(
            Some(&unsupported_resolver),
            &remote_track,
            Quality::High,
        )
        .expect("Unsupported error should fall back to track.url");
        assert_eq!(resolved_remote.url, "https://cdn.example.test/song.mp3");

        // Case 2: Any other resolver error still falls back when `track.local_path` is present.
        let failing_resolver: StreamResolver =
            Arc::new(|_, _| Err(SourceError::Message("network offline".into())));
        let cached_local_track = Track {
            id: "local-1".to_owned(),
            source: "youtube-music".to_owned(),
            name: "Downloaded Track".to_owned(),
            url: "https://music.youtube.com/watch?v=abc".to_owned(),
            local_path: "/tmp/downloaded.flac".to_owned(),
            ..Track::default()
        };
        let resolved_local = resolve_track_stream_with_resolver(
            Some(&failing_resolver),
            &cached_local_track,
            Quality::High,
        )
        .expect("non-empty local_path should still fall back cleanly");
        assert_eq!(resolved_local.url, "/tmp/downloaded.flac");

        // Verify URL sanitizer strips query/path secrets.
        assert_eq!(
            sanitize_url_for_log(
                "https://rr1---sn.googlevideo.com/videoplayback?sig=SECRET&token=123"
            ),
            "https://rr1---sn.googlevideo.com"
        );
        assert_eq!(
            sanitize_url_for_log("http://127.0.0.1:24567/stream?key=abc"),
            "http://127.0.0.1:24567"
        );
    }

    #[test]
    fn test_engine_remote_set_loop_mode_updates_snapshot_and_driver() {
        use std::sync::Mutex;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use nsh_music_core::model::RepeatMode;

        use crate::cache::StreamCacheConfig;
        use crate::output::NullOutput;
        use crate::remote::{
            NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteLoopMode,
            RemotePlaybackStatus,
        };

        use super::{AudioEngine, AudioEngineConfig, PlaybackStatus};

        #[derive(Default)]
        struct RecordingDriver {
            metadata_updates: Mutex<Vec<NowPlayingMetadata>>,
            detail_updates: Mutex<Vec<NowPlayingMetadata>>,
        }

        impl PlatformRemoteDriver for RecordingDriver {
            fn backend(&self) -> RemoteBackend {
                RemoteBackend::HarmonyAvSession
            }

            fn update_metadata(&self, meta: &NowPlayingMetadata) {
                self.metadata_updates.lock().unwrap().push(meta.clone());
            }

            fn update_playback(&self, _is_playing: bool, _position_ms: u64, _duration_ms: u64) {}

            fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
                self.detail_updates.lock().unwrap().push(meta.clone());
            }

            fn clear(&self) {}
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-player-remote-loop-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let engine = AudioEngine::with_output(
            AudioEngineConfig {
                cache: StreamCacheConfig {
                    cache_dir: dir.clone(),
                    max_bytes: 1024 * 1024,
                    ..StreamCacheConfig::default()
                },
                prefetch_next: false,
                ..AudioEngineConfig::default()
            },
            Box::new(NullOutput::with_tick_interval(Duration::from_millis(2))),
        )
        .expect("create engine");

        let driver = Arc::new(RecordingDriver::default());
        engine.remote_controller().set_driver(driver.clone());

        let track = Track {
            id: "demo-loop-1".to_owned(),
            source: "local".to_owned(),
            name: "Loop Test".to_owned(),
            duration: 15_000,
            url: "demo://loop-1?duration=15000".to_owned(),
            ..Track::default()
        };
        engine.play_track(track);

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if engine.snapshot().status == PlaybackStatus::Playing {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(engine.snapshot().status, PlaybackStatus::Playing);

        // Verify Buffering -> Playing metadata lifecycle was pushed to the driver.
        {
            let metas = driver.metadata_updates.lock().unwrap();
            assert!(
                metas
                    .iter()
                    .any(|m| m.status == RemotePlaybackStatus::Buffering),
                "expected Buffering metadata update before Playing"
            );
            assert!(
                metas
                    .iter()
                    .any(|m| m.status == RemotePlaybackStatus::Playing),
                "expected Playing metadata update"
            );
        }

        // Dispatch RemoteCommand::SetLoopMode(Shuffle) -> repeat=All, shuffle=true, driver updated.
        engine
            .remote_controller()
            .dispatch(RemoteCommand::SetLoopMode(RemoteLoopMode::Shuffle));
        let loop_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < loop_deadline {
            let s = engine.snapshot();
            if s.repeat == RepeatMode::All && s.shuffle {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let s = engine.snapshot();
        assert_eq!(s.repeat, RepeatMode::All);
        assert!(s.shuffle);

        let detail_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < detail_deadline {
            if driver
                .detail_updates
                .lock()
                .unwrap()
                .last()
                .is_some_and(|m| m.loop_mode == RemoteLoopMode::Shuffle)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            driver
                .detail_updates
                .lock()
                .unwrap()
                .last()
                .map(|m| m.loop_mode),
            Some(RemoteLoopMode::Shuffle)
        );

        // Dispatch RemoteCommand::SetLoopMode(Single) -> repeat=One, shuffle=false.
        engine
            .remote_controller()
            .dispatch(RemoteCommand::SetLoopMode(RemoteLoopMode::Single));
        let single_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < single_deadline {
            let s2 = engine.snapshot();
            if s2.repeat == RepeatMode::One && !s2.shuffle {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let s2 = engine.snapshot();
        assert_eq!(s2.repeat, RepeatMode::One);
        assert!(!s2.shuffle);
        assert_eq!(
            engine.remote_controller().metadata().loop_mode,
            RemoteLoopMode::Single
        );

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_prefetch_target_prefers_stream_resolver_and_never_falls_back_to_watch_url() {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use nsh_music_core::model::StreamInfo;

        use super::resolve_prefetch_target;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        let resolver: StreamResolver = Arc::new(move |track, quality| {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            assert_eq!(quality, Quality::High);
            match track.id.as_str() {
                "yt-ok" => {
                    let mut headers = HashMap::new();
                    headers.insert("User-Agent".to_owned(), "nsh-audio-test".to_owned());
                    headers.insert(
                        "Referer".to_owned(),
                        "https://music.youtube.com/".to_owned(),
                    );
                    Ok(StreamInfo {
                        url: "https://cdn.example.test/stream/yt-ok.m4a".to_owned(),
                        headers,
                        ..StreamInfo::default()
                    })
                }
                "yt-empty" => Ok(StreamInfo {
                    url: String::new(),
                    ..StreamInfo::default()
                }),
                "yt-file" => Ok(StreamInfo {
                    url: "file:///tmp/not-http.mp3".to_owned(),
                    ..StreamInfo::default()
                }),
                _ => Err(SourceError::Message("upstream unavailable".into())),
            }
        });

        // 1. Plugin track with webpage track.url: must resolve via stream_resolver first.
        let yt_ok = Track {
            id: "yt-ok".to_owned(),
            source: "youtube-music".to_owned(),
            name: "YouTube Track".to_owned(),
            url: "https://music.youtube.com/watch?v=test".to_owned(),
            ..Track::default()
        };
        let (url, headers) = resolve_prefetch_target(Some(&resolver), &yt_ok, Quality::High)
            .expect("should resolve stream URL through stream_resolver");
        assert_eq!(url, "https://cdn.example.test/stream/yt-ok.m4a");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "User-Agent" && v == "nsh-audio-test")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Referer" && v == "https://music.youtube.com/")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 2. Resolver failure, empty URL, or non-HTTP URL must NOT fall back to next_track.url.
        for bad_id in ["yt-fail", "yt-empty", "yt-file"] {
            let track = Track {
                id: bad_id.to_owned(),
                source: "youtube-music".to_owned(),
                url: "https://music.youtube.com/watch?v=test".to_owned(),
                ..Track::default()
            };
            assert!(
                resolve_prefetch_target(Some(&resolver), &track, Quality::High).is_none(),
                "must not fall back to webpage track.url for {bad_id}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        // 3. Empty track.id with stream_resolver set must return None without calling resolver.
        let empty_id_track = Track {
            id: String::new(),
            source: "youtube-music".to_owned(),
            url: "https://music.youtube.com/watch?v=test".to_owned(),
            ..Track::default()
        };
        assert!(resolve_prefetch_target(Some(&resolver), &empty_id_track, Quality::High).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        // 4. Local tracks are always skipped without invoking stream_resolver.
        let local_cases = [
            Track {
                id: "loc-1".to_owned(),
                source: "local".to_owned(),
                url: "https://example.test/should-skip.mp3".to_owned(),
                ..Track::default()
            },
            Track {
                id: "loc-2".to_owned(),
                source: "custom".to_owned(),
                url: "file:///Users/music/song.flac".to_owned(),
                ..Track::default()
            },
            Track {
                id: "loc-3".to_owned(),
                source: "custom".to_owned(),
                url: "/Users/music/song.mp3".to_owned(),
                ..Track::default()
            },
        ];
        for track in &local_cases {
            assert!(resolve_prefetch_target(Some(&resolver), track, Quality::High).is_none());
            assert!(resolve_prefetch_target(None, track, Quality::High).is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        // 5. Only when stream_resolver is None should http(s):// track.url be used directly.
        let direct_track = Track {
            id: "direct-1".to_owned(),
            source: "direct".to_owned(),
            url: "https://cdn.example.test/direct.mp3".to_owned(),
            ..Track::default()
        };
        let (direct_url, direct_headers) =
            resolve_prefetch_target(None, &direct_track, Quality::High)
                .expect("direct HTTP track should prefetch when stream_resolver is None");
        assert_eq!(direct_url, "https://cdn.example.test/direct.mp3");
        assert!(direct_headers.is_empty());

        let demo_track = Track {
            id: "demo-1".to_owned(),
            source: "demo".to_owned(),
            url: "demo://ambient".to_owned(),
            ..Track::default()
        };
        assert!(resolve_prefetch_target(None, &demo_track, Quality::High).is_none());
    }

    #[test]
    fn test_maybe_prefetch_next_resolves_through_stream_resolver_not_watch_url() {
        use std::collections::HashMap;
        use std::sync::Mutex;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use nsh_music_core::model::StreamInfo;

        use crate::cache::StreamCacheConfig;
        use crate::output::NullOutput;

        use super::{AudioEngine, AudioEngineConfig, PlaybackStatus};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-player-prefetch-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let engine = AudioEngine::with_output(
            AudioEngineConfig {
                cache: StreamCacheConfig {
                    cache_dir: dir.clone(),
                    max_bytes: 1024 * 1024,
                    ..StreamCacheConfig::default()
                },
                prefetch_next: true,
                ..AudioEngineConfig::default()
            },
            Box::new(NullOutput::with_tick_interval(Duration::from_millis(2))),
        )
        .expect("create engine");

        let resolved_url = "https://cdn.example.test/resolved-audio-stream.m4a";
        // Pre-seed cache so `cache.prefetch` hits disk cache immediately with zero network traffic.
        engine
            .cache()
            .put_bytes(resolved_url, b"prefetched-audio-bytes")
            .expect("seed cache entry");

        let resolved_calls: Arc<Mutex<Vec<(String, Quality)>>> = Arc::new(Mutex::new(Vec::new()));
        let resolved_calls_ref = Arc::clone(&resolved_calls);
        engine.set_stream_resolver(Some(Arc::new(move |track, quality| {
            resolved_calls_ref
                .lock()
                .unwrap()
                .push((track.id.clone(), quality));
            if track.url.starts_with("demo://") {
                return Err(SourceError::Unsupported);
            }
            if track.id == "yt-next-ok" {
                let mut headers = HashMap::new();
                headers.insert("User-Agent".to_owned(), "nsh-prefetch-test".to_owned());
                headers.insert(
                    "Referer".to_owned(),
                    "https://music.youtube.com/".to_owned(),
                );
                return Ok(StreamInfo {
                    url: resolved_url.to_owned(),
                    headers,
                    ..StreamInfo::default()
                });
            }
            Err(SourceError::Message("upstream 403".into()))
        })));

        let demo_track_1 = Track {
            id: "demo-prefetch-1".to_owned(),
            source: "demo".to_owned(),
            name: "Demo 1".to_owned(),
            duration: 15_000,
            url: "demo://prefetch-1?duration=15000".to_owned(),
            ..Track::default()
        };
        let yt_next_ok = Track {
            id: "yt-next-ok".to_owned(),
            source: "youtube-music".to_owned(),
            name: "YouTube Next".to_owned(),
            url: "https://music.youtube.com/watch?v=test".to_owned(),
            ..Track::default()
        };
        engine.play_queue(vec![demo_track_1, yt_next_ok], 0);

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !engine.prefetched_targets_for_test().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let targets = engine.prefetched_targets_for_test();
        assert_eq!(
            targets.len(),
            1,
            "expected exactly one resolved prefetch target"
        );
        assert_eq!(
            targets[0].0, resolved_url,
            "must prefetch resolved stream URL instead of next_track.url"
        );
        assert!(
            targets[0]
                .1
                .iter()
                .any(|(k, v)| k == "User-Agent" && v == "nsh-prefetch-test")
        );
        assert!(
            targets[0]
                .1
                .iter()
                .any(|(k, v)| k == "Referer" && v == "https://music.youtube.com/")
        );
        assert!(
            resolved_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(id, q)| id == "yt-next-ok" && *q == Quality::High)
        );

        // Next case: resolver fails for a YouTube watch-page track -> must NOT fall back to watch URL.
        engine.clear_prefetched_targets_for_test();
        let demo_track_2 = Track {
            id: "demo-prefetch-2".to_owned(),
            source: "demo".to_owned(),
            name: "Demo 2".to_owned(),
            duration: 15_000,
            url: "demo://prefetch-2?duration=15000".to_owned(),
            ..Track::default()
        };
        let yt_next_fail = Track {
            id: "yt-next-fail".to_owned(),
            source: "youtube-music".to_owned(),
            name: "YouTube Failing".to_owned(),
            url: "https://music.youtube.com/watch?v=test".to_owned(),
            ..Track::default()
        };
        engine.play_queue(vec![demo_track_2, yt_next_fail], 0);

        let fail_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < fail_deadline {
            if resolved_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| id == "yt-next-fail")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            resolved_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| id == "yt-next-fail"),
            "stream_resolver should have been called for yt-next-fail"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            engine.prefetched_targets_for_test().is_empty(),
            "failing stream_resolver must NOT fall back to prefetching next_track.url"
        );

        // Next case: local next track is skipped without calling stream_resolver or prefetch.
        let demo_track_3 = Track {
            id: "demo-prefetch-3".to_owned(),
            source: "demo".to_owned(),
            name: "Demo 3".to_owned(),
            duration: 15_000,
            url: "demo://prefetch-3?duration=15000".to_owned(),
            ..Track::default()
        };
        let local_next = Track {
            id: "local-next-1".to_owned(),
            source: "local".to_owned(),
            name: "Local Next".to_owned(),
            url: "/tmp/local-next.flac".to_owned(),
            ..Track::default()
        };
        engine.play_queue(vec![demo_track_3, local_next], 0);
        engine.set_volume(0.73);
        let local_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < local_deadline {
            let snap = engine.snapshot();
            if snap.status == PlaybackStatus::Playing && (snap.volume - 0.73).abs() < 0.001 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !resolved_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| id == "local-next-1"),
            "local next track must be skipped before invoking stream_resolver"
        );
        assert!(engine.prefetched_targets_for_test().is_empty());

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "audio-symphonia")]
    fn make_test_wav_bytes(sample_rate: u32, channels: u16, num_frames: u32) -> Vec<u8> {
        let data_bytes = num_frames * channels as u32 * 2;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = ((std::f32::consts::TAU * 440.0 * t).sin() * 12_000.0) as i16;
            for _ in 0..channels {
                wav.extend_from_slice(&sample.to_le_bytes());
            }
        }
        wav
    }

    #[cfg(feature = "audio-symphonia")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_plugin_track_replay_with_changing_signed_urls_hits_disk_cache_without_re_resolving()
     {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use nsh_music_core::model::StreamInfo;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use crate::cache::StreamCacheConfig;
        use crate::output::NullOutput;

        use super::{AudioEngine, AudioEngineConfig, PlaybackStatus};

        let wav = Arc::new(make_test_wav_bytes(44_100, 2, 44_100 / 4));
        let http_hits = Arc::new(AtomicUsize::new(0));
        let srv_wav = Arc::clone(&wav);
        let srv_hits = Arc::clone(&http_hits);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                srv_hits.fetch_add(1, Ordering::SeqCst);
                let data = Arc::clone(&srv_wav);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(&data).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-player-signed-replay-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let engine = AudioEngine::with_output(
            AudioEngineConfig {
                cache: StreamCacheConfig {
                    cache_dir: dir.clone(),
                    max_bytes: 8 * 1024 * 1024,
                    ..StreamCacheConfig::default()
                },
                prefetch_next: false,
                ..AudioEngineConfig::default()
            },
            Box::new(NullOutput::with_tick_interval(Duration::from_millis(2))),
        )
        .expect("create engine");

        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let resolve_calls_ref = Arc::clone(&resolve_calls);
        engine.set_stream_resolver(Some(Arc::new(move |_track, _quality| {
            let sig = resolve_calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(StreamInfo {
                url: format!("http://{addr}/videoplayback?sig={sig}&expire=999999"),
                media_type: "audio/wav".to_owned(),
                bit_rate: 1_411_200,
                gain: -3.5,
                peak: 0.88,
                ..StreamInfo::default()
            })
        })));

        let yt_track = Track {
            id: "yt-signed-1".to_owned(),
            source: "youtube-music".to_owned(),
            name: "Signed URL Track".to_owned(),
            url: "https://music.youtube.com/watch?v=yt-signed-1".to_owned(),
            ..Track::default()
        };

        // First play: resolves via `?sig=1` and streams over HTTP, persisting under logical track key.
        engine.play_track(yt_track.clone());
        let cache_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < cache_deadline {
            if engine
                .cache()
                .get_cached_track("youtube-music", "yt-signed-1", Quality::High)
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(
            engine
                .cache()
                .get_cached_track("youtube-music", "yt-signed-1", Quality::High)
                .is_some(),
            "first playback must populate logical track disk cache"
        );
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 1);
        assert_eq!(http_hits.load(Ordering::SeqCst), 1);

        // Stop and replay the same logical track -> must hit disk cache with ZERO resolver/HTTP calls.
        engine.stop();
        let stop_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < stop_deadline {
            if engine.snapshot().status == PlaybackStatus::Stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(engine.snapshot().status, PlaybackStatus::Stopped);

        engine.play_track(yt_track);
        let replay_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < replay_deadline {
            if engine.snapshot().status == PlaybackStatus::Playing {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(engine.snapshot().status, PlaybackStatus::Playing);
        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            1,
            "replay must hit logical track disk cache with 0 second stream_resolver calls"
        );
        assert_eq!(
            http_hits.load(Ordering::SeqCst),
            1,
            "replay must hit logical track disk cache with 0 second HTTP fetches"
        );

        engine.shutdown();
        srv.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "audio-symphonia")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_maybe_prefetch_next_caches_full_large_track_over_512kb_for_zero_network_next() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        use nsh_music_core::model::StreamInfo;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use crate::cache::StreamCacheConfig;
        use crate::output::NullOutput;

        use super::{AudioEngine, AudioEngineConfig, PlaybackStatus};

        // 3.5 seconds @ 44.1kHz stereo 16-bit WAV = 617,444 bytes (> 512 KiB!).
        let large_wav = Arc::new(make_test_wav_bytes(44_100, 2, 154_350));
        assert!(
            large_wav.len() > 512 * 1024,
            "test track must exceed 512 KiB, got {} bytes",
            large_wav.len()
        );
        let expected_len = large_wav.len() as u64;

        let http_hits = Arc::new(AtomicUsize::new(0));
        let srv_wav = Arc::clone(&large_wav);
        let srv_hits = Arc::clone(&http_hits);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                srv_hits.fetch_add(1, Ordering::SeqCst);
                let data = Arc::clone(&srv_wav);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(&data).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-player-prefetch-full-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let engine = AudioEngine::with_output(
            AudioEngineConfig {
                cache: StreamCacheConfig {
                    cache_dir: dir.clone(),
                    max_bytes: 16 * 1024 * 1024,
                    ..StreamCacheConfig::default()
                },
                prefetch_next: true,
                ..AudioEngineConfig::default()
            },
            Box::new(NullOutput::with_tick_interval(Duration::from_millis(2))),
        )
        .expect("create engine");

        let resolve_calls_t2 = Arc::new(AtomicUsize::new(0));
        let resolve_t2_ref = Arc::clone(&resolve_calls_t2);
        engine.set_stream_resolver(Some(Arc::new(move |track, _quality| {
            if track.url.starts_with("demo://") {
                return Err(SourceError::Unsupported);
            }
            let sig = resolve_t2_ref.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(StreamInfo {
                url: format!("http://{addr}/large-next.wav?sig={sig}"),
                media_type: "audio/wav".to_owned(),
                bit_rate: 1_411_200,
                gain: -2.0,
                peak: 0.95,
                ..StreamInfo::default()
            })
        })));

        let track_1 = Track {
            id: "demo-lead-1".to_owned(),
            source: "demo".to_owned(),
            name: "Demo Lead".to_owned(),
            duration: 15_000,
            url: "demo://lead-1?duration=15000".to_owned(),
            ..Track::default()
        };
        let track_2 = Track {
            id: "yt-large-next".to_owned(),
            source: "youtube-music".to_owned(),
            name: "Large Next Track".to_owned(),
            url: "https://music.youtube.com/watch?v=yt-large-next".to_owned(),
            ..Track::default()
        };

        engine.play_queue(vec![track_1, track_2], 0);

        // Wait for `maybe_prefetch_next` to download ALL >512 KiB of track_2 into disk cache.
        let prefetch_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < prefetch_deadline {
            if engine
                .cache()
                .get_cached_track("youtube-music", "yt-large-next", Quality::High)
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let (cached_path, cached_meta) = engine
            .cache()
            .get_cached_track("youtube-music", "yt-large-next", Quality::High)
            .expect("maybe_prefetch_next must cache full >512 KiB track to disk");
        assert_eq!(
            std::fs::metadata(&cached_path)
                .expect("cached metadata")
                .len(),
            expected_len,
            "cached file must contain 100% of the >512 KiB track"
        );
        assert_eq!(cached_meta.media_type, "audio/wav");
        assert_eq!(resolve_calls_t2.load(Ordering::SeqCst), 1);
        assert_eq!(http_hits.load(Ordering::SeqCst), 1);

        // Advance to next track -> must open directly from disk cache with 0 extra resolver/HTTP calls.
        engine.next_track();
        let next_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < next_deadline {
            let snap = engine.snapshot();
            if snap.status == PlaybackStatus::Playing
                && snap
                    .current_track
                    .as_ref()
                    .is_some_and(|t| t.id == "yt-large-next")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snap = engine.snapshot();
        assert_eq!(snap.status, PlaybackStatus::Playing);
        assert_eq!(
            snap.current_track.as_ref().map(|t| t.id.as_str()),
            Some("yt-large-next")
        );
        assert_eq!(
            resolve_calls_t2.load(Ordering::SeqCst),
            1,
            "next() on fully prefetched track must make 0 extra stream_resolver calls"
        );
        assert_eq!(
            http_hits.load(Ordering::SeqCst),
            1,
            "next() on fully prefetched track must make 0 extra HTTP fetches"
        );

        engine.shutdown();
        srv.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
