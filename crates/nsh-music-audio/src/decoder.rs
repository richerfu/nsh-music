#[cfg(test)]
use std::f32::consts::TAU;

use crate::AudioResult;
use crate::cache::AudioCache;

/// Decoded audio stream format metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_ms: Option<u64>,
    pub bit_rate: Option<u32>,
    pub codec_name: String,
}

impl Default for AudioSpec {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
            duration_ms: None,
            bit_rate: None,
            codec_name: "pcm".to_owned(),
        }
    }
}

/// Backend selection preference when opening compressed audio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderPreference {
    /// Use Symphonia first and fall back to static FFmpeg when enabled.
    #[default]
    Auto,
    /// Prefer static FFmpeg when enabled, falling back to Symphonia.
    FfmpegFirst,
    /// Use Symphonia only.
    SymphoniaOnly,
}

/// Unified decoder trait producing interleaved `f32` PCM samples (`-1.0..=1.0`).
pub trait AudioDecoder: Send {
    fn spec(&self) -> AudioSpec;

    /// Appends decoded interleaved `f32` PCM samples to `out`, returning the number of
    /// interleaved samples written. Returns `Ok(0)` at clean EOF.
    fn read_pcm(&mut self, out: &mut Vec<f32>) -> AudioResult<usize>;

    /// Seeks to `position_ms`, returning the actual landed playback timestamp in milliseconds.
    fn seek(&mut self, position_ms: u64) -> AudioResult<u64>;

    /// Returns the decoder's current playback timestamp in milliseconds.
    fn position_ms(&self) -> u64;

    /// Returns the backend identifier (`"symphonia"`, `"ffmpeg"`, or `"demo-synth"`).
    fn backend_name(&self) -> &'static str;
}

/// Procedural ambient chord/tone synthesizer used exclusively in audio unit tests.
#[cfg(test)]
pub struct DemoSynthDecoder {
    uri: String,
    sample_rate: u32,
    channels: u16,
    total_frames: u64,
    frame_cursor: u64,
    seed: u64,
    base_hz: f32,
    bpm: f32,
    chord_intervals: [[f32; 4]; 4],
}

#[cfg(test)]
impl DemoSynthDecoder {
    pub const DEFAULT_SAMPLE_RATE: u32 = 44_100;
    pub const DEFAULT_DURATION_MS: u64 = 180_000;

    /// Creates a procedural synth decoder deterministically keyed by a `demo://...` URI.
    pub fn from_uri(uri: &str, duration_hint_ms: Option<u64>) -> Self {
        let mut seed: u64 = 0xcbf29ce484222325;
        for byte in uri.as_bytes() {
            seed ^= *byte as u64;
            seed = seed.wrapping_mul(0x100000001b3);
        }

        // Allow explicit ?duration=<ms> override in URI.
        let parsed_duration = uri
            .split_once("duration=")
            .and_then(|(_, rest)| rest.split('&').next())
            .and_then(|v| v.parse::<u64>().ok());
        let duration_ms = parsed_duration
            .or(duration_hint_ms.filter(|&d| d > 0))
            .unwrap_or(Self::DEFAULT_DURATION_MS);

        // Select a warm musical root frequency (C3 .. A3) and tempo (72 .. 108 BPM).
        const ROOTS_HZ: [f32; 6] = [130.81, 146.83, 164.81, 174.61, 196.00, 220.00];
        let base_hz = ROOTS_HZ[(seed as usize) % ROOTS_HZ.len()];
        let bpm = 74.0 + ((seed >> 8) % 32) as f32;

        // Warm jazz/ambient maj7 / min7 / add9 chord progressions in semitone ratios.
        let progressions: [[[f32; 4]; 4]; 3] = [
            [
                [1.0, 1.25, 1.5, 1.875],     // Imaj7
                [0.8909, 1.1225, 1.3348, 1.6818], // vi7
                [1.3348, 1.6818, 2.0, 2.5198],    // IVmaj7
                [1.1225, 1.4983, 1.7818, 2.2449], // V9sus
            ],
            [
                [1.0, 1.1892, 1.4983, 1.7818],    // im7
                [1.3348, 1.5874, 2.0, 2.3784],    // ivm7
                [1.1892, 1.4983, 1.7818, 2.2449], // bIIImaj7
                [1.4983, 1.7818, 2.2449, 2.6697], // v7
            ],
            [
                [1.0, 1.2599, 1.4983, 2.2449], // Iadd9
                [1.1225, std::f32::consts::SQRT_2, 1.6818, 2.2449], // ii7
                [1.3348, 1.6818, 2.0, 2.3784], // IVmaj7
                [1.4983, 1.8877, 2.2449, 2.9966], // V6/9
            ],
        ];
        let chord_intervals = progressions[((seed >> 16) as usize) % progressions.len()];
        let sample_rate = Self::DEFAULT_SAMPLE_RATE;
        let total_frames = (duration_ms.saturating_mul(sample_rate as u64)) / 1000;

        Self {
            uri: uri.to_owned(),
            sample_rate,
            channels: 2,
            total_frames: total_frames.max(sample_rate as u64 / 10),
            frame_cursor: 0,
            seed,
            base_hz,
            bpm,
            chord_intervals,
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }
}

#[cfg(test)]
impl AudioDecoder for DemoSynthDecoder {
    fn spec(&self) -> AudioSpec {
        let duration_ms = (self.total_frames.saturating_mul(1000)) / self.sample_rate as u64;
        AudioSpec {
            sample_rate: self.sample_rate,
            channels: self.channels,
            duration_ms: Some(duration_ms),
            bit_rate: Some(1_411_200),
            codec_name: "demo-synth".to_owned(),
        }
    }

    fn read_pcm(&mut self, out: &mut Vec<f32>) -> AudioResult<usize> {
        if self.frame_cursor >= self.total_frames {
            return Ok(0);
        }
        let frames_to_render = (1024u64).min(self.total_frames - self.frame_cursor) as usize;
        out.reserve(frames_to_render * self.channels as usize);

        let sr = self.sample_rate as f32;
        let beat_sec = 60.0 / self.bpm;
        let bar_sec = beat_sec * 4.0;
        let shimmer_mod = 1.0 + (((self.seed >> 24) % 5) as f32) * 0.12;

        for i in 0..frames_to_render {
            let frame_idx = self.frame_cursor + i as u64;
            let t = frame_idx as f32 / sr;

            let bar_index = ((t / bar_sec).floor() as usize) % self.chord_intervals.len();
            let bar_phase = (t % bar_sec) / bar_sec;
            let beat_phase = (t % beat_sec) / beat_sec;
            let chord = self.chord_intervals[bar_index];

            // Warm pad envelope across each bar (gentle attack & swell)
            let pad_env = (bar_phase * 8.0).min(1.0) * (1.0 - (bar_phase - 0.88).max(0.0) * 6.0).clamp(0.25, 1.0);
            // Sub-bass warm pulse on each beat
            let bass_env = (-beat_phase * 3.2).exp() * 0.65 + 0.35;
            let bass_hz = self.base_hz * chord[0] * 0.5;
            let bass = (TAU * bass_hz * t).sin() * 0.24 * bass_env
                + (TAU * bass_hz * 2.0 * t).sin() * 0.08 * bass_env;

            // Layered stereo ambient pad chords with subtle chorus detune
            let mut pad_l = 0.0f32;
            let mut pad_r = 0.0f32;
            for (v_idx, &ratio) in chord.iter().enumerate() {
                let freq = self.base_hz * ratio;
                let detune = 1.0 + (v_idx as f32 - 1.5) * 0.0022;
                let l_osc = (TAU * freq * detune * t).sin()
                    + 0.28 * (TAU * freq * 2.0 * detune * t).sin();
                let r_osc = (TAU * freq / detune * t).sin()
                    + 0.28 * (TAU * freq * 2.0 / detune * t).sin();
                pad_l += l_osc;
                pad_r += r_osc;
            }
            pad_l *= 0.11 * pad_env;
            pad_r *= 0.11 * pad_env;

            // Melodic bell arpeggio + airy high harmonics for full 16-band visualizer response
            let step = ((t / (beat_sec * 0.5)).floor() as usize) % 4;
            let step_phase = (t % (beat_sec * 0.5)) / (beat_sec * 0.5);
            let bell_env = (-step_phase * 6.5).exp();
            let bell_hz = self.base_hz * chord[step] * 4.0;
            let bell = (TAU * bell_hz * t).sin() * 0.09 * bell_env
                + (TAU * bell_hz * 2.0 * shimmer_mod * t).sin() * 0.035 * bell_env;

            let pan = 0.5 + 0.35 * (t * 0.65).sin();
            // Gentle overall fade-in/out at track boundaries
            let edge_env = (frame_idx as f32 / (sr * 0.08)).min(1.0)
                * ((self.total_frames - frame_idx) as f32 / (sr * 0.15)).min(1.0);

            let left = ((bass + pad_l + bell * (1.0 - pan)) * edge_env).clamp(-0.95, 0.95);
            let right = ((bass + pad_r + bell * pan) * edge_env).clamp(-0.95, 0.95);
            out.push(left);
            out.push(right);
        }

        self.frame_cursor += frames_to_render as u64;
        Ok(frames_to_render * self.channels as usize)
    }

    fn seek(&mut self, position_ms: u64) -> AudioResult<u64> {
        let target_frame = (position_ms.saturating_mul(self.sample_rate as u64)) / 1000;
        self.frame_cursor = target_frame.min(self.total_frames);
        Ok(self.position_ms())
    }

    fn position_ms(&self) -> u64 {
        (self.frame_cursor.saturating_mul(1000)) / self.sample_rate as u64
    }

    fn backend_name(&self) -> &'static str {
        "demo-synth"
    }
}

#[cfg(feature = "audio-symphonia")]
pub mod symphonia_impl {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
    use symphonia::core::io::{MediaSource, MediaSourceStream};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::core::units::{Time, TimeBase};

    use super::{AudioDecoder, AudioSpec};
    use crate::{AudioError, AudioResult};

    /// Pure-Rust Symphonia audio decoder outputting interleaved `f32` PCM.
    pub struct SymphoniaDecoder {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn Decoder>,
        track_id: u32,
        time_base: Option<TimeBase>,
        spec: AudioSpec,
        position_ms: u64,
        decoded_frames: u64,
        sample_buf: Option<SampleBuffer<f32>>,
    }

    impl SymphoniaDecoder {
        pub fn open(
            source: Box<dyn MediaSource>,
            extension_hint: Option<&str>,
            mime_hint: Option<&str>,
        ) -> AudioResult<Self> {
            let mss = MediaSourceStream::new(source, Default::default());
            let mut hint = Hint::new();
            if let Some(ext) = extension_hint {
                let cleaned = ext.trim().trim_start_matches('.');
                if !cleaned.is_empty() {
                    hint.with_extension(cleaned);
                }
            }
            if let Some(mime) = mime_hint {
                let cleaned = mime.split(';').next().unwrap_or(mime).trim();
                if !cleaned.is_empty() {
                    hint.mime_type(cleaned);
                }
            }

            let probed = symphonia::default::get_probe()
                .format(
                    &hint,
                    mss,
                    &FormatOptions {
                        enable_gapless: true,
                        ..Default::default()
                    },
                    &MetadataOptions::default(),
                )
                .map_err(|e| AudioError::Decode(format!("symphonia probe failed: {e}")))?;

            let format = probed.format;
            let track = format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
                .ok_or_else(|| {
                    AudioError::Decode("no supported audio track found in stream".into())
                })?;

            let track_id = track.id;
            let codec_params = track.codec_params.clone();
            let decoder = symphonia::default::get_codecs()
                .make(&codec_params, &DecoderOptions::default())
                .map_err(|e| AudioError::Decode(format!("symphonia codec init failed: {e}")))?;

            let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
            let channels = codec_params
                .channels
                .map(|c| c.count() as u16)
                .unwrap_or(2)
                .max(1);
            let time_base = codec_params.time_base;
            let duration_ms = match (codec_params.n_frames, time_base) {
                (Some(n_frames), Some(tb)) => {
                    let t = tb.calc_time(n_frames);
                    Some(t.seconds.saturating_mul(1000) + (t.frac * 1000.0).round() as u64)
                }
                (Some(n_frames), None) if sample_rate > 0 => {
                    Some(n_frames.saturating_mul(1000) / sample_rate as u64)
                }
                _ => None,
            };

            let spec = AudioSpec {
                sample_rate,
                channels,
                duration_ms,
                bit_rate: None,
                codec_name: format!("{:?}", codec_params.codec),
            };

            Ok(Self {
                format,
                decoder,
                track_id,
                time_base,
                spec,
                position_ms: 0,
                decoded_frames: 0,
                sample_buf: None,
            })
        }
    }

    impl AudioDecoder for SymphoniaDecoder {
        fn spec(&self) -> AudioSpec {
            self.spec.clone()
        }

        fn read_pcm(&mut self, out: &mut Vec<f32>) -> AudioResult<usize> {
            loop {
                let packet = match self.format.next_packet() {
                    Ok(pkt) => pkt,
                    Err(SymphoniaError::IoError(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        return Ok(0);
                    }
                    Err(SymphoniaError::ResetRequired) => {
                        self.decoder.reset();
                        continue;
                    }
                    Err(err) => {
                        return Err(AudioError::Decode(format!("demux error: {err}")));
                    }
                };

                if packet.track_id() != self.track_id {
                    continue;
                }

                let decoded = match self.decoder.decode(&packet) {
                    Ok(buf) => buf,
                    Err(SymphoniaError::DecodeError(msg)) => {
                        log::debug!("skipping corrupt audio frame: {msg}");
                        continue;
                    }
                    Err(SymphoniaError::IoError(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        return Ok(0);
                    }
                    Err(err) => {
                        return Err(AudioError::Decode(format!("decode error: {err}")));
                    }
                };

                let decoded_spec = *decoded.spec();
                self.spec.sample_rate = decoded_spec.rate;
                self.spec.channels = decoded_spec.channels.count().max(1) as u16;

                let needed_capacity = decoded.capacity() as u64;
                let needs_realloc = self
                    .sample_buf
                    .as_ref()
                    .map(|b| b.capacity() < decoded.capacity() * decoded_spec.channels.count())
                    .unwrap_or(true);
                if needs_realloc {
                    self.sample_buf = Some(SampleBuffer::<f32>::new(needed_capacity, decoded_spec));
                }

                let sample_buf = self.sample_buf.as_mut().expect("sample buffer initialized");
                sample_buf.copy_interleaved_ref(decoded);
                let samples = sample_buf.samples();
                if samples.is_empty() {
                    continue;
                }

                out.extend_from_slice(samples);
                let frames = samples.len() / (self.spec.channels as usize).max(1);
                self.decoded_frames += frames as u64;

                if let Some(tb) = self.time_base {
                    let ts = packet.ts().saturating_add(packet.dur());
                    let t = tb.calc_time(ts);
                    self.position_ms =
                        t.seconds.saturating_mul(1000) + (t.frac * 1000.0).round() as u64;
                } else if self.spec.sample_rate > 0 {
                    self.position_ms =
                        self.decoded_frames.saturating_mul(1000) / self.spec.sample_rate as u64;
                }

                return Ok(samples.len());
            }
        }

        fn seek(&mut self, position_ms: u64) -> AudioResult<u64> {
            let secs = position_ms / 1000;
            let frac = (position_ms % 1000) as f64 / 1000.0;
            let seeked = self
                .format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::new(secs, frac),
                        track_id: Some(self.track_id),
                    },
                )
                .map_err(|e| AudioError::Seek(format!("symphonia seek failed: {e}")))?;

            self.decoder.reset();
            let landed_ms = if let Some(tb) = self.time_base {
                let t = tb.calc_time(seeked.actual_ts);
                t.seconds.saturating_mul(1000) + (t.frac * 1000.0).round() as u64
            } else {
                position_ms
            };
            self.position_ms = landed_ms;
            self.decoded_frames =
                (landed_ms.saturating_mul(self.spec.sample_rate as u64)) / 1000;
            Ok(landed_ms)
        }

        fn position_ms(&self) -> u64 {
            self.position_ms
        }

        fn backend_name(&self) -> &'static str {
            "symphonia"
        }
    }
}

#[cfg(feature = "audio-symphonia")]
pub use symphonia_impl::SymphoniaDecoder;

/// Infers a container extension hint from a URL path or media type string.
pub fn infer_extension_hint(url: &str, media_type: Option<&str>) -> Option<String> {
    if let Some(mt) = media_type {
        let lower = mt.to_ascii_lowercase();
        if lower.contains("flac") {
            return Some("flac".to_owned());
        }
        if lower.contains("mpeg") || lower.contains("mp3") {
            return Some("mp3".to_owned());
        }
        if lower.contains("mp4") || lower.contains("m4a") || lower.contains("aac") {
            return Some("m4a".to_owned());
        }
        if lower.contains("ogg") || lower.contains("opus") || lower.contains("vorbis") {
            return Some("ogg".to_owned());
        }
        if lower.contains("wav") {
            return Some("wav".to_owned());
        }
        if lower.contains("webm") || lower.contains("matroska") {
            return Some("webm".to_owned());
        }
        if !lower.contains('/') && !lower.is_empty() && lower.len() <= 8 {
            return Some(lower);
        }
    }

    let path_part = url.split(['?', '#']).next().unwrap_or(url);
    let (_, ext) = path_part.rsplit_once('.')?;
    if !ext.is_empty() && ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(ext.to_ascii_lowercase());
    }
    None
}

/// Opens the best available [`AudioDecoder`] for a local file or HTTP/HTTPS URI,
/// honoring custom HTTP headers and backend preference with automatic fallback.
pub fn open_decoder(
    cache: &AudioCache,
    url: &str,
    headers: Vec<(String, String)>,
    media_type: Option<&str>,
    duration_hint_ms: Option<u64>,
    preference: DecoderPreference,
) -> AudioResult<Box<dyn AudioDecoder>> {
    #[cfg(test)]
    if url.starts_with("demo://") {
        return Ok(Box::new(DemoSynthDecoder::from_uri(url, duration_hint_ms)));
    }
    let _ = duration_hint_ms;

    let ext_hint = infer_extension_hint(url, media_type);

    #[cfg(feature = "audio-ffmpeg")]
    if preference == DecoderPreference::FfmpegFirst {
        if let Ok(stream) = cache.open_progressive(url, headers.clone()) {
            if let Ok(dec) = crate::ffmpeg_backend::FfmpegDecoder::open(Box::new(stream)) {
                return Ok(Box::new(dec));
            }
        }
    }

    #[cfg(feature = "audio-symphonia")]
    {
        let stream = cache.open_progressive(url, headers.clone())?;
        let content_type = stream.content_type().map(|s| s.to_owned());
        let mime = media_type.or(content_type.as_deref());
        match SymphoniaDecoder::open(Box::new(stream), ext_hint.as_deref(), mime) {
            Ok(decoder) => Ok(Box::new(decoder)),
            Err(symphonia_err) => {
                // Retry without extension/MIME hints in case an HTTP loopback relay
                // forwards raw container bytes (e.g. fMP4/AAC) with a different type label.
                if (ext_hint.is_some() || mime.is_some())
                    && let Ok(retry_stream) = cache.open_progressive(url, headers.clone())
                    && let Ok(decoder) = SymphoniaDecoder::open(Box::new(retry_stream), None, None)
                {
                    return Ok(Box::new(decoder));
                }
                #[cfg(feature = "audio-ffmpeg")]
                if preference != DecoderPreference::SymphoniaOnly {
                    log::info!(
                        "symphonia decode failed ({symphonia_err}); falling back to static FFmpeg"
                    );
                    let fallback_stream = cache.open_progressive(url, headers)?;
                    let dec = crate::ffmpeg_backend::FfmpegDecoder::open(Box::new(fallback_stream))?;
                    return Ok(Box::new(dec));
                }
                let _ = preference;
                Err(symphonia_err)
            }
        }
    }

    #[cfg(all(not(feature = "audio-symphonia"), feature = "audio-ffmpeg"))]
    {
        let _ = (ext_hint, preference);
        let stream = cache.open_progressive(url, headers)?;
        let dec = crate::ffmpeg_backend::FfmpegDecoder::open(Box::new(stream))?;
        return Ok(Box::new(dec));
    }

    #[cfg(not(any(feature = "audio-symphonia", feature = "audio-ffmpeg")))]
    {
        let _ = (cache, headers, ext_hint, preference);
        Err(crate::AudioError::Unsupported(
            "no audio decoder feature enabled (enable `audio-symphonia` or `audio-ffmpeg`)".into(),
        ))
    }
}
