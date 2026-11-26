use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{AudioError, AudioResult};

/// Negotiated audio output stream configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
        }
    }
}

/// Hardware or OS-level audio focus / routing events raised by [`AudioOutput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputEvent {
    /// System audio focus requested pause/stop/mute (phone call, another media app, etc.).
    SystemPause,
    /// System audio focus signaled that a transient interruption ended and playback may resume.
    SystemResume,
    /// Active audio output device disconnected (Bluetooth headphones, wired headset, USB DAC).
    /// Playback must pause and NEVER auto-resume.
    DeviceDisconnected,
    /// System requested temporary volume attenuation (navigation/notification prompt).
    Duck,
    /// System ended volume attenuation.
    Unduck,
}

/// Callback invoked by an [`AudioOutput`] backend when hardware or OS audio focus events occur.
pub type OutputEventCallback = Arc<dyn Fn(AudioOutputEvent) + Send + Sync>;

/// Clonable handle allowing headless tests to emit simulated [`AudioOutputEvent`]s into
/// a [`NullOutput`] after it has been moved into [`AudioEngine`](crate::AudioEngine).
#[derive(Clone, Default)]
pub struct OutputEventEmitter {
    cb: Arc<Mutex<Option<OutputEventCallback>>>,
}

impl OutputEventEmitter {
    pub fn emit(&self, event: AudioOutputEvent) {
        if let Some(cb) = self.cb.lock().ok().and_then(|g| g.clone()) {
            cb(event);
        }
    }
}

/// Real-time PCM callback invoked by the audio output device to fill interleaved `f32` samples.
pub type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send + 'static>;

/// Cross-platform audio output sink abstraction.
pub trait AudioOutput: Send {
    /// Starts rendering audio with the requested `spec`, returning the actual hardware
    /// [`OutputConfig`] negotiated with the OS audio stack.
    fn start(&mut self, spec: OutputConfig, render: RenderCallback) -> AudioResult<OutputConfig>;
    fn pause(&mut self) -> AudioResult<()>;
    fn resume(&mut self) -> AudioResult<()>;
    fn stop(&mut self) -> AudioResult<()>;
    fn is_running(&self) -> bool;
    fn backend_name(&self) -> &'static str;
    fn set_event_callback(&mut self, _callback: Option<OutputEventCallback>) {}
}

/// Headless / software-paced audio output for tests, CI environments, and systems without
/// hardware audio devices.
///
/// Blocks on a [`Condvar`] when paused so idle/paused playback consumes 0% CPU.
pub struct NullOutput {
    tick_interval: Duration,
    running: Arc<AtomicBool>,
    pause_state: Arc<(Mutex<bool>, Condvar)>,
    stop_flag: Arc<AtomicBool>,
    frames_rendered: Arc<AtomicU64>,
    last_rms_bits: Arc<AtomicU32>,
    event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
    worker: Option<JoinHandle<()>>,
}

impl Default for NullOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl NullOutput {
    /// Creates a real-time paced `NullOutput` (10 ms buffer cadence).
    pub fn new() -> Self {
        Self::with_tick_interval(Duration::from_millis(10))
    }

    /// Creates a `NullOutput` with a custom pacing interval (e.g., 2 ms for rapid tests).
    pub fn with_tick_interval(tick_interval: Duration) -> Self {
        Self {
            tick_interval: tick_interval.max(Duration::from_millis(1)),
            running: Arc::new(AtomicBool::new(false)),
            pause_state: Arc::new((Mutex::new(false), Condvar::new())),
            stop_flag: Arc::new(AtomicBool::new(false)),
            frames_rendered: Arc::new(AtomicU64::new(0)),
            last_rms_bits: Arc::new(AtomicU32::new(0)),
            event_cb: Arc::new(Mutex::new(None)),
            worker: None,
        }
    }

    /// Returns a clonable emitter handle for injecting simulated OS audio events in tests.
    pub fn event_emitter(&self) -> OutputEventEmitter {
        OutputEventEmitter {
            cb: Arc::clone(&self.event_cb),
        }
    }

    /// Emits a simulated [`AudioOutputEvent`] directly through the registered callback.
    pub fn emit_event(&self, event: AudioOutputEvent) {
        self.event_emitter().emit(event);
    }

    /// Returns an inspectable counter of total PCM frames rendered by this output.
    pub fn frames_rendered_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.frames_rendered)
    }

    /// Returns the most recent rendered buffer RMS amplitude (useful in headless tests).
    pub fn last_rms(&self) -> f32 {
        f32::from_bits(self.last_rms_bits.load(Ordering::Relaxed))
    }
}

impl AudioOutput for NullOutput {
    fn start(
        &mut self,
        spec: OutputConfig,
        mut render: RenderCallback,
    ) -> AudioResult<OutputConfig> {
        self.stop()?;

        let running = Arc::clone(&self.running);
        let pause_state = Arc::clone(&self.pause_state);
        let stop_flag = Arc::clone(&self.stop_flag);
        let frames_counter = Arc::clone(&self.frames_rendered);
        let last_rms_bits = Arc::clone(&self.last_rms_bits);
        let tick = self.tick_interval;

        stop_flag.store(false, Ordering::SeqCst);
        if let Ok(mut paused) = pause_state.0.lock() {
            *paused = false;
        }
        running.store(true, Ordering::SeqCst);

        let frames_per_tick = ((spec.sample_rate as u64 * 10) / 1000).clamp(64, 4096) as usize;
        let samples_per_tick = frames_per_tick * (spec.channels as usize).max(1);

        let handle = thread::Builder::new()
            .name("nsh-audio-null-out".into())
            .spawn(move || {
                let mut buf = vec![0.0f32; samples_per_tick];
                while !stop_flag.load(Ordering::Relaxed) {
                    // Block cleanly on Condvar while paused (0% CPU).
                    if let Ok(mut paused) = pause_state.0.lock() {
                        while *paused && !stop_flag.load(Ordering::Relaxed) {
                            match pause_state.1.wait(paused) {
                                Ok(next) => paused = next,
                                Err(_) => break,
                            }
                        }
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    buf.fill(0.0);
                    render(&mut buf);
                    frames_counter.fetch_add(frames_per_tick as u64, Ordering::Relaxed);
                    let sum_sq: f32 = buf.iter().map(|s| s * s).sum();
                    let rms = (sum_sq / (buf.len().max(1) as f32)).sqrt();
                    last_rms_bits.store(rms.to_bits(), Ordering::Relaxed);
                    thread::sleep(tick);
                }
                running.store(false, Ordering::SeqCst);
            })
            .map_err(|e| AudioError::Output(format!("failed to spawn NullOutput thread: {e}")))?;

        self.worker = Some(handle);
        Ok(spec)
    }

    fn pause(&mut self) -> AudioResult<()> {
        if let Ok(mut paused) = self.pause_state.0.lock() {
            *paused = true;
        }
        Ok(())
    }

    fn resume(&mut self) -> AudioResult<()> {
        if let Ok(mut paused) = self.pause_state.0.lock() {
            *paused = false;
            self.pause_state.1.notify_all();
        }
        Ok(())
    }

    fn stop(&mut self) -> AudioResult<()> {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut paused) = self.pause_state.0.lock() {
            *paused = false;
            self.pause_state.1.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        let paused = self.pause_state.0.lock().map(|p| *p).unwrap_or(false);
        self.running.load(Ordering::Relaxed) && !paused
    }

    fn backend_name(&self) -> &'static str {
        "null"
    }

    fn set_event_callback(&mut self, callback: Option<OutputEventCallback>) {
        if let Ok(mut guard) = self.event_cb.lock() {
            *guard = callback;
        }
    }
}

impl Drop for NullOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(not(any(target_arch = "wasm32", target_env = "ohos")))]
#[path = "output_monitor.rs"]
mod output_monitor;

#[cfg(not(any(target_arch = "wasm32", target_env = "ohos")))]
mod cpal_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, Stream, StreamConfig, SupportedStreamConfig};

    use super::output_monitor::OutputMonitor;
    use super::{AudioOutput, AudioOutputEvent, OutputConfig, OutputEventCallback, RenderCallback};
    use crate::{AudioError, AudioResult};

    enum CpalCmd {
        Start {
            spec: OutputConfig,
            render: RenderCallback,
            reply: Sender<AudioResult<OutputConfig>>,
        },
        Pause(Sender<AudioResult<()>>),
        Resume(Sender<AudioResult<()>>),
        Stop(Sender<AudioResult<()>>),
        Shutdown,
    }

    /// Cross-platform hardware audio output powered by `cpal` (macOS CoreAudio,
    /// Windows WASAPI, Linux ALSA/PipeWire, iOS AudioUnit, Android AAudio/Oboe).
    ///
    /// Owns the non-`Send` `cpal::Stream` on a dedicated lightweight control thread so
    /// [`CpalOutput`] is cleanly `Send` on every platform without `unsafe`.
    pub struct CpalOutput {
        tx: Sender<CpalCmd>,
        running: Arc<AtomicBool>,
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
        worker: Option<JoinHandle<()>>,
    }

    impl CpalOutput {
        pub fn new() -> AudioResult<Self> {
            // Probe host & default output device eagerly so headless systems fall back cleanly.
            let host = cpal::default_host();
            let _device = host
                .default_output_device()
                .ok_or_else(|| AudioError::Output("no default audio output device found".into()))?;

            let (tx, rx) = mpsc::channel::<CpalCmd>();
            let running = Arc::new(AtomicBool::new(false));
            let thread_running = Arc::clone(&running);
            let event_cb: Arc<Mutex<Option<OutputEventCallback>>> = Arc::new(Mutex::new(None));
            let thread_event_cb = Arc::clone(&event_cb);

            let worker = thread::Builder::new()
                .name("nsh-audio-cpal".into())
                .spawn(move || {
                    let mut active_stream: Option<Stream> = None;
                    let mut monitor: Option<OutputMonitor> = None;
                    loop {
                        let poll_interval = monitor
                            .as_ref()
                            .and_then(|m| m.poll_interval(thread_running.load(Ordering::Relaxed)));
                        let cmd = if let Some(interval) = poll_interval {
                            match rx.recv_timeout(interval) {
                                Ok(cmd) => cmd,
                                Err(RecvTimeoutError::Timeout) => {
                                    if let Some(ref mut m) = monitor {
                                        m.poll_tick();
                                    }
                                    continue;
                                }
                                Err(RecvTimeoutError::Disconnected) => break,
                            }
                        } else {
                            match rx.recv() {
                                Ok(cmd) => cmd,
                                Err(_) => break,
                            }
                        };

                        match cmd {
                            CpalCmd::Start {
                                spec,
                                render,
                                reply,
                            } => {
                                drop(monitor.take());
                                active_stream = None;
                                thread_running.store(false, Ordering::SeqCst);
                                match build_cpal_stream(spec, render, Arc::clone(&thread_event_cb))
                                {
                                    Ok((stream, negotiated, device_name)) => {
                                        let new_monitor = OutputMonitor::start(
                                            Arc::clone(&thread_event_cb),
                                            device_name,
                                        );
                                        if let Err(err) = stream.play() {
                                            drop(new_monitor);
                                            let _ = reply.send(Err(AudioError::Output(format!(
                                                "cpal stream.play() failed: {err}"
                                            ))));
                                        } else {
                                            active_stream = Some(stream);
                                            monitor = Some(new_monitor);
                                            thread_running.store(true, Ordering::SeqCst);
                                            let _ = reply.send(Ok(negotiated));
                                        }
                                    }
                                    Err(err) => {
                                        let _ = reply.send(Err(err));
                                    }
                                }
                            }
                            CpalCmd::Pause(reply) => {
                                let res = if let Some(ref stream) = active_stream {
                                    stream
                                        .pause()
                                        .map_err(|e| AudioError::Output(format!("cpal pause: {e}")))
                                } else {
                                    Ok(())
                                };
                                if res.is_ok() {
                                    thread_running.store(false, Ordering::SeqCst);
                                    if let Some(ref mut m) = monitor {
                                        m.set_active(false);
                                    }
                                }
                                let _ = reply.send(res);
                            }
                            CpalCmd::Resume(reply) => {
                                let res = if let Some(ref stream) = active_stream {
                                    stream
                                        .play()
                                        .map_err(|e| AudioError::Output(format!("cpal play: {e}")))
                                } else {
                                    Ok(())
                                };
                                if res.is_ok() {
                                    thread_running.store(true, Ordering::SeqCst);
                                    if let Some(ref mut m) = monitor {
                                        m.set_active(true);
                                    }
                                }
                                let _ = reply.send(res);
                            }
                            CpalCmd::Stop(reply) => {
                                drop(monitor.take());
                                active_stream = None;
                                thread_running.store(false, Ordering::SeqCst);
                                let _ = reply.send(Ok(()));
                            }
                            CpalCmd::Shutdown => {
                                drop(monitor.take());
                                drop(active_stream.take());
                                thread_running.store(false, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    drop(monitor.take());
                    drop(active_stream.take());
                })
                .map_err(|e| {
                    AudioError::Output(format!("failed to spawn cpal control thread: {e}"))
                })?;

            Ok(Self {
                tx,
                running,
                event_cb,
                worker: Some(worker),
            })
        }
    }

    impl AudioOutput for CpalOutput {
        fn start(
            &mut self,
            spec: OutputConfig,
            render: RenderCallback,
        ) -> AudioResult<OutputConfig> {
            let (reply_tx, reply_rx) = mpsc::channel();
            self.tx
                .send(CpalCmd::Start {
                    spec,
                    render,
                    reply: reply_tx,
                })
                .map_err(|_| AudioError::Output("cpal control thread stopped".into()))?;
            reply_rx
                .recv()
                .map_err(|_| AudioError::Output("cpal start response dropped".into()))?
        }

        fn pause(&mut self) -> AudioResult<()> {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = self.tx.send(CpalCmd::Pause(reply_tx));
            reply_rx.recv().unwrap_or(Ok(()))
        }

        fn resume(&mut self) -> AudioResult<()> {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = self.tx.send(CpalCmd::Resume(reply_tx));
            reply_rx.recv().unwrap_or(Ok(()))
        }

        fn stop(&mut self) -> AudioResult<()> {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = self.tx.send(CpalCmd::Stop(reply_tx));
            reply_rx.recv().unwrap_or(Ok(()))
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::Relaxed)
        }

        fn backend_name(&self) -> &'static str {
            "cpal"
        }

        fn set_event_callback(&mut self, callback: Option<OutputEventCallback>) {
            if let Ok(mut guard) = self.event_cb.lock() {
                *guard = callback;
            }
        }
    }

    impl Drop for CpalOutput {
        fn drop(&mut self) {
            let _ = self.tx.send(CpalCmd::Shutdown);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn select_best_config(
        device: &cpal::Device,
        requested: OutputConfig,
    ) -> AudioResult<SupportedStreamConfig> {
        if let Ok(configs) = device.supported_output_configs() {
            let mut best: Option<(i64, SupportedStreamConfig)> = None;
            for range in configs {
                let fmt_score = match range.sample_format() {
                    SampleFormat::F32 => 0,
                    SampleFormat::I16 => 100,
                    SampleFormat::U16 => 200,
                    _ => 500,
                };
                let ch_diff = (range.channels() as i64 - requested.channels as i64).abs() * 1000;
                let min_r = range.min_sample_rate().0;
                let max_r = range.max_sample_rate().0;
                let clamped_rate = requested.sample_rate.clamp(min_r, max_r);
                let rate_diff = (clamped_rate as i64 - requested.sample_rate as i64).abs();
                let score = fmt_score + ch_diff + rate_diff;
                let candidate = range.with_sample_rate(cpal::SampleRate(clamped_rate));
                if best.as_ref().map(|(s, _)| score < *s).unwrap_or(true) {
                    best = Some((score, candidate));
                }
            }
            if let Some((_, cfg)) = best {
                return Ok(cfg);
            }
        }

        device
            .default_output_config()
            .map_err(|e| AudioError::Output(format!("no supported output config: {e}")))
    }

    fn build_cpal_stream(
        requested: OutputConfig,
        mut render: RenderCallback,
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
    ) -> AudioResult<(Stream, OutputConfig, Option<String>)> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::Output("no default audio output device".into()))?;
        let device_name = device.name().ok();
        let supported = select_best_config(&device, requested)?;
        let sample_format = supported.sample_format();
        let stream_config: StreamConfig = supported.into();
        let negotiated = OutputConfig {
            sample_rate: stream_config.sample_rate.0,
            channels: stream_config.channels,
        };

        let err_fn = move |err: cpal::StreamError| {
            if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                log::info!("cpal audio output device unavailable -> DeviceDisconnected");
                if let Some(cb) = event_cb.lock().ok().and_then(|g| g.clone()) {
                    cb(AudioOutputEvent::DeviceDisconnected);
                }
            } else {
                log::warn!("cpal audio output stream error: {err}");
            }
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| {
                    render(data);
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => {
                let mut scratch = Vec::<f32>::with_capacity(4096);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _| {
                        scratch.resize(data.len(), 0.0);
                        render(&mut scratch);
                        for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                            *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut scratch = Vec::<f32>::with_capacity(4096);
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _| {
                        scratch.resize(data.len(), 0.0);
                        render(&mut scratch);
                        for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                            let norm = (src.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32;
                            *dst = norm as u16;
                        }
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                return Err(AudioError::Output(format!(
                    "unsupported cpal sample format: {other:?}"
                )));
            }
        }
        .map_err(|e| AudioError::Output(format!("failed to build cpal stream: {e}")))?;

        Ok((stream, negotiated, device_name))
    }
}

#[cfg(not(any(target_arch = "wasm32", target_env = "ohos")))]
pub use cpal_impl::CpalOutput;

#[cfg(target_env = "ohos")]
mod ohos_impl {
    use std::sync::{Arc, Mutex};

    use ohos_audio_binding::{
        AudioDataCallbackResult, AudioRenderer, AudioStreamBuilder, DeviceChangeReason,
        EncodingType, InterruptForceType, InterruptHint, InterruptMode, LatencyMode, SampleFormat,
        StreamUsage,
    };

    use super::{AudioOutput, AudioOutputEvent, OutputConfig, OutputEventCallback, RenderCallback};
    use crate::{AudioError, AudioResult};

    /// HarmonyOS / OpenHarmony Tier-1 audio output wrapping `ohos-audio-binding`
    /// (`OH_AudioRenderer`).
    pub struct OhosAudioOutput {
        renderer: Option<AudioRenderer>,
        running: bool,
        event_cb: Arc<Mutex<Option<OutputEventCallback>>>,
    }

    impl Default for OhosAudioOutput {
        fn default() -> Self {
            Self::new()
        }
    }

    impl OhosAudioOutput {
        pub fn new() -> Self {
            Self {
                renderer: None,
                running: false,
                event_cb: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl AudioOutput for OhosAudioOutput {
        fn start(
            &mut self,
            spec: OutputConfig,
            mut render: RenderCallback,
        ) -> AudioResult<OutputConfig> {
            self.stop()?;

            let mut builder = AudioStreamBuilder::new_renderer()
                .map_err(|e| AudioError::Output(format!("OHAudio builder create failed: {e}")))?;
            builder
                .sampling_rate(spec.sample_rate as i32)
                .map_err(|e| AudioError::Output(format!("OHAudio sampling_rate failed: {e}")))?;
            builder
                .channel_count(spec.channels as i32)
                .map_err(|e| AudioError::Output(format!("OHAudio channel_count failed: {e}")))?;
            builder
                .sample_format(SampleFormat::F32Le)
                .map_err(|e| AudioError::Output(format!("OHAudio sample_format failed: {e}")))?;
            builder
                .encoding_type(EncodingType::Raw)
                .map_err(|e| AudioError::Output(format!("OHAudio encoding_type failed: {e}")))?;
            builder
                .usage(StreamUsage::Music)
                .map_err(|e| AudioError::Output(format!("OHAudio usage failed: {e}")))?;
            builder
                .latency_mode(LatencyMode::Normal)
                .map_err(|e| AudioError::Output(format!("OHAudio latency_mode failed: {e}")))?;

            if let Err(err) = builder.interrupt_mode(InterruptMode::Share) {
                log::warn!("OHAudio interrupt_mode(Share) failed: {err}");
            }

            let device_cb = Arc::clone(&self.event_cb);
            if let Err(err) = builder.output_device_change_callback(move |reason| match reason {
                DeviceChangeReason::OldDeviceUnavailable => {
                    log::info!("OHAudio output device unavailable -> DeviceDisconnected");
                    if let Some(cb) = device_cb.lock().ok().and_then(|g| g.clone()) {
                        cb(AudioOutputEvent::DeviceDisconnected);
                    }
                }
                other => {
                    log::debug!("OHAudio output device changed: {other:?}");
                }
            }) {
                log::warn!("OHAudio output_device_change_callback setup failed: {err}");
            }

            let interrupt_cb = Arc::clone(&self.event_cb);
            if let Err(err) =
                builder.interrupt_callback(move |force: InterruptForceType, hint: InterruptHint| {
                    let event = match hint {
                        InterruptHint::Pause | InterruptHint::Stop | InterruptHint::Mute => {
                            log::info!("OHAudio interrupt ({force:?}, {hint:?}) -> SystemPause");
                            Some(AudioOutputEvent::SystemPause)
                        }
                        InterruptHint::Resume | InterruptHint::Unmute => {
                            log::info!("OHAudio interrupt ({force:?}, {hint:?}) -> SystemResume");
                            Some(AudioOutputEvent::SystemResume)
                        }
                        InterruptHint::Duck => {
                            log::info!("OHAudio interrupt ({force:?}) -> Duck");
                            Some(AudioOutputEvent::Duck)
                        }
                        InterruptHint::Unduck => {
                            log::info!("OHAudio interrupt ({force:?}, {hint:?}) -> Unduck");
                            Some(AudioOutputEvent::Unduck)
                        }
                        InterruptHint::None => {
                            log::debug!("OHAudio interrupt ({force:?}, None)");
                            None
                        }
                    };
                    if let Some(ev) = event
                        && let Some(cb) = interrupt_cb.lock().ok().and_then(|g| g.clone())
                    {
                        cb(ev);
                    }
                })
            {
                log::warn!("OHAudio interrupt_callback setup failed: {err}");
            }

            if let Err(err) = builder.error_callback(|err| {
                log::warn!("OHAudio renderer error: {err}");
            }) {
                log::warn!("OHAudio error_callback setup failed: {err}");
            }

            let mut scratch = Vec::<f32>::with_capacity(4096);
            builder
                .write_data_callback(move |bytes: &mut [u8]| {
                    let sample_count = bytes.len() / 4;
                    if sample_count == 0 {
                        return AudioDataCallbackResult::Invalid;
                    }
                    scratch.resize(sample_count, 0.0);
                    scratch.fill(0.0);
                    render(&mut scratch);
                    for (dst, &sample) in
                        bytes.as_chunks_mut::<4>().0.iter_mut().zip(scratch.iter())
                    {
                        *dst = sample.clamp(-1.0, 1.0).to_le_bytes();
                    }
                    AudioDataCallbackResult::Valid
                })
                .map_err(|e| AudioError::Output(format!("OHAudio callback setup failed: {e}")))?;

            let mut renderer = builder
                .build()
                .map_err(|e| AudioError::Output(format!("OHAudio build failed: {e}")))?;
            let actual_rate = renderer
                .sampling_rate()
                .map(|r| r as u32)
                .unwrap_or(spec.sample_rate);
            let actual_channels = renderer
                .channel_count()
                .map(|c| c as u16)
                .unwrap_or(spec.channels);
            renderer
                .start()
                .map_err(|e| AudioError::Output(format!("OHAudio start failed: {e}")))?;

            self.renderer = Some(renderer);
            self.running = true;
            Ok(OutputConfig {
                sample_rate: actual_rate,
                channels: actual_channels,
            })
        }

        fn pause(&mut self) -> AudioResult<()> {
            if let Some(ref mut renderer) = self.renderer {
                renderer
                    .pause()
                    .map_err(|e| AudioError::Output(format!("OHAudio pause failed: {e}")))?;
            }
            self.running = false;
            Ok(())
        }

        fn resume(&mut self) -> AudioResult<()> {
            if let Some(ref mut renderer) = self.renderer {
                renderer
                    .start()
                    .map_err(|e| AudioError::Output(format!("OHAudio resume failed: {e}")))?;
                self.running = true;
            }
            Ok(())
        }

        fn stop(&mut self) -> AudioResult<()> {
            if let Some(mut renderer) = self.renderer.take() {
                let _ = renderer.stop();
                let _ = renderer.flush();
                let _ = renderer.close();
            }
            self.running = false;
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running
        }

        fn backend_name(&self) -> &'static str {
            "ohos-audio"
        }

        fn set_event_callback(&mut self, callback: Option<OutputEventCallback>) {
            if let Ok(mut guard) = self.event_cb.lock() {
                *guard = callback;
            }
        }
    }

    impl Drop for OhosAudioOutput {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }
}

#[cfg(target_env = "ohos")]
pub use ohos_impl::OhosAudioOutput;

/// Creates the platform's default hardware [`AudioOutput`], falling back gracefully to
/// [`NullOutput`] on headless machines or environments without audio devices.
pub fn default_audio_output() -> Box<dyn AudioOutput> {
    #[cfg(target_env = "ohos")]
    {
        Box::new(OhosAudioOutput::new())
    }

    #[cfg(not(any(target_arch = "wasm32", target_env = "ohos")))]
    {
        match CpalOutput::new() {
            Ok(output) => Box::new(output),
            Err(err) => {
                log::warn!("falling back to NullOutput: {err}");
                Box::new(NullOutput::new())
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    {
        Box::new(NullOutput::new())
    }
}
