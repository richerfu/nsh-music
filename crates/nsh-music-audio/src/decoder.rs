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
                [1.0, 1.25, 1.5, 1.875],          // Imaj7
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
                [1.0, 1.2599, 1.4983, 2.2449],                      // Iadd9
                [1.1225, std::f32::consts::SQRT_2, 1.6818, 2.2449], // ii7
                [1.3348, 1.6818, 2.0, 2.3784],                      // IVmaj7
                [1.4983, 1.8877, 2.2449, 2.9966],                   // V6/9
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
            let pad_env = (bar_phase * 8.0).min(1.0)
                * (1.0 - (bar_phase - 0.88).max(0.0) * 6.0).clamp(0.25, 1.0);
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
                let l_osc =
                    (TAU * freq * detune * t).sin() + 0.28 * (TAU * freq * 2.0 * detune * t).sin();
                let r_osc =
                    (TAU * freq / detune * t).sin() + 0.28 * (TAU * freq * 2.0 / detune * t).sin();
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
    use std::io::{self, Read, Seek, SeekFrom};
    use std::sync::{Arc, Mutex, OnceLock};

    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{
        CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_EAC3, CODEC_TYPE_FLAC,
        CODEC_TYPE_MONKEYS_AUDIO, CODEC_TYPE_MP1, CODEC_TYPE_MP2, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
        CODEC_TYPE_OPUS, CODEC_TYPE_PCM_ALAW, CODEC_TYPE_PCM_F32BE, CODEC_TYPE_PCM_F32BE_PLANAR,
        CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_F32LE_PLANAR, CODEC_TYPE_PCM_F64BE,
        CODEC_TYPE_PCM_F64BE_PLANAR, CODEC_TYPE_PCM_F64LE, CODEC_TYPE_PCM_F64LE_PLANAR,
        CODEC_TYPE_PCM_MULAW, CODEC_TYPE_PCM_S8, CODEC_TYPE_PCM_S8_PLANAR, CODEC_TYPE_PCM_S16BE,
        CODEC_TYPE_PCM_S16BE_PLANAR, CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S16LE_PLANAR,
        CODEC_TYPE_PCM_S24BE, CODEC_TYPE_PCM_S24BE_PLANAR, CODEC_TYPE_PCM_S24LE,
        CODEC_TYPE_PCM_S24LE_PLANAR, CODEC_TYPE_PCM_S32BE, CODEC_TYPE_PCM_S32BE_PLANAR,
        CODEC_TYPE_PCM_S32LE, CODEC_TYPE_PCM_S32LE_PLANAR, CODEC_TYPE_PCM_U8,
        CODEC_TYPE_PCM_U8_PLANAR, CODEC_TYPE_PCM_U16BE, CODEC_TYPE_PCM_U16BE_PLANAR,
        CODEC_TYPE_PCM_U16LE, CODEC_TYPE_PCM_U16LE_PLANAR, CODEC_TYPE_PCM_U24BE,
        CODEC_TYPE_PCM_U24BE_PLANAR, CODEC_TYPE_PCM_U24LE, CODEC_TYPE_PCM_U24LE_PLANAR,
        CODEC_TYPE_PCM_U32BE, CODEC_TYPE_PCM_U32BE_PLANAR, CODEC_TYPE_PCM_U32LE,
        CODEC_TYPE_PCM_U32LE_PLANAR, CODEC_TYPE_TTA, CODEC_TYPE_VORBIS, CODEC_TYPE_WAVPACK,
        CODEC_TYPE_WMA, CodecRegistry, CodecType, Decoder, DecoderOptions,
    };
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
    use symphonia::core::io::{MediaSource, MediaSourceStream};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::core::units::{Time, TimeBase};

    use super::{AudioDecoder, AudioSpec};
    use crate::{AudioError, AudioResult};

    /// Shared Symphonia codec registry combining all built-in Symphonia codecs with our
    /// pure-Rust [`SymphoniaOpusDecoder`](crate::opus_codec::SymphoniaOpusDecoder).
    fn symphonia_codec_registry() -> &'static CodecRegistry {
        static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let mut registry = CodecRegistry::new();
            symphonia::default::register_enabled_codecs(&mut registry);
            registry.register_all::<crate::opus_codec::SymphoniaOpusDecoder>();
            registry
        })
    }

    fn canonical_symphonia_codec_name(codec: CodecType, ext_hint: Option<&str>) -> String {
        match codec {
            CODEC_TYPE_FLAC => "flac".to_owned(),
            CODEC_TYPE_ALAC => "alac".to_owned(),
            CODEC_TYPE_WAVPACK => "wavpack".to_owned(),
            CODEC_TYPE_MONKEYS_AUDIO => "ape".to_owned(),
            CODEC_TYPE_TTA => "tta".to_owned(),
            CODEC_TYPE_MP1 | CODEC_TYPE_MP2 | CODEC_TYPE_MP3 => "mp3".to_owned(),
            CODEC_TYPE_AAC => "aac".to_owned(),
            CODEC_TYPE_VORBIS => "vorbis".to_owned(),
            CODEC_TYPE_OPUS => "opus".to_owned(),
            CODEC_TYPE_EAC3 => "eac3".to_owned(),
            CODEC_TYPE_WMA => "wma".to_owned(),
            CODEC_TYPE_PCM_S32LE
            | CODEC_TYPE_PCM_S32LE_PLANAR
            | CODEC_TYPE_PCM_S32BE
            | CODEC_TYPE_PCM_S32BE_PLANAR
            | CODEC_TYPE_PCM_S24LE
            | CODEC_TYPE_PCM_S24LE_PLANAR
            | CODEC_TYPE_PCM_S24BE
            | CODEC_TYPE_PCM_S24BE_PLANAR
            | CODEC_TYPE_PCM_S16LE
            | CODEC_TYPE_PCM_S16LE_PLANAR
            | CODEC_TYPE_PCM_S16BE
            | CODEC_TYPE_PCM_S16BE_PLANAR
            | CODEC_TYPE_PCM_S8
            | CODEC_TYPE_PCM_S8_PLANAR
            | CODEC_TYPE_PCM_U32LE
            | CODEC_TYPE_PCM_U32LE_PLANAR
            | CODEC_TYPE_PCM_U32BE
            | CODEC_TYPE_PCM_U32BE_PLANAR
            | CODEC_TYPE_PCM_U24LE
            | CODEC_TYPE_PCM_U24LE_PLANAR
            | CODEC_TYPE_PCM_U24BE
            | CODEC_TYPE_PCM_U24BE_PLANAR
            | CODEC_TYPE_PCM_U16LE
            | CODEC_TYPE_PCM_U16LE_PLANAR
            | CODEC_TYPE_PCM_U16BE
            | CODEC_TYPE_PCM_U16BE_PLANAR
            | CODEC_TYPE_PCM_U8
            | CODEC_TYPE_PCM_U8_PLANAR
            | CODEC_TYPE_PCM_F32LE
            | CODEC_TYPE_PCM_F32LE_PLANAR
            | CODEC_TYPE_PCM_F32BE
            | CODEC_TYPE_PCM_F32BE_PLANAR
            | CODEC_TYPE_PCM_F64LE
            | CODEC_TYPE_PCM_F64LE_PLANAR
            | CODEC_TYPE_PCM_F64BE
            | CODEC_TYPE_PCM_F64BE_PLANAR
            | CODEC_TYPE_PCM_ALAW
            | CODEC_TYPE_PCM_MULAW => match ext_hint {
                Some("aif") | Some("aiff") => "aiff".to_owned(),
                _ => "wav".to_owned(),
            },
            other => format!("{other}"),
        }
    }

    struct SharedMediaSourceInner {
        source: Box<dyn MediaSource>,
        head_bytes: Vec<u8>,
    }

    #[derive(Clone)]
    struct SharedMediaSource(Arc<Mutex<SharedMediaSourceInner>>);

    impl SharedMediaSource {
        fn new(source: Box<dyn MediaSource>) -> Self {
            Self(Arc::new(Mutex::new(SharedMediaSourceInner {
                source,
                head_bytes: Vec::with_capacity(16),
            })))
        }

        fn rewind_to_start(&self) -> io::Result<()> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| io::Error::other("media source mutex poisoned"))?;
            guard.source.seek(SeekFrom::Start(0))?;
            Ok(())
        }

        fn head_bytes(&self) -> Vec<u8> {
            self.0
                .lock()
                .map(|mut g| {
                    if g.head_bytes.is_empty() && g.source.seek(SeekFrom::Start(0)).is_ok() {
                        let mut peek = [0u8; 16];
                        if let Ok(n) = g.source.read(&mut peek) {
                            g.head_bytes.extend_from_slice(&peek[..n]);
                        }
                    }
                    g.head_bytes.clone()
                })
                .unwrap_or_default()
        }

        fn head_preview(&self) -> (String, String) {
            let bytes = self.head_bytes();
            let hex = bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = bytes
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            (hex, ascii)
        }
    }

    fn format_probe_failure_error(
        base_msg: &str,
        cleaned_mime: Option<&str>,
        byte_len: Option<u64>,
        head_bytes: &[u8],
    ) -> String {
        if head_bytes.is_empty() {
            return "empty audio stream (0 bytes)".to_owned();
        }
        let trimmed = head_bytes
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map(|i| &head_bytes[i..])
            .unwrap_or(head_bytes);
        let looks_textual = matches!(trimmed.first(), Some(b'<' | b'{' | b'['))
            || head_bytes
                .iter()
                .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace());
        let head_summary = if looks_textual {
            head_bytes
                .iter()
                .take(16)
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect::<String>()
        } else {
            head_bytes
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mime_label = cleaned_mime.unwrap_or("unknown");
        format!("{base_msg} [mime={mime_label}, len={byte_len:?}, head={head_summary}]")
    }

    impl Read for SharedMediaSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| io::Error::other("media source mutex poisoned"))?;
            let n = guard.source.read(buf)?;
            if guard.head_bytes.len() < 16 && n > 0 {
                let take = (16 - guard.head_bytes.len()).min(n);
                guard.head_bytes.extend_from_slice(&buf[..take]);
            }
            Ok(n)
        }
    }

    impl Seek for SharedMediaSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| io::Error::other("media source mutex poisoned"))?;
            guard.source.seek(pos)
        }
    }

    impl MediaSource for SharedMediaSource {
        fn is_seekable(&self) -> bool {
            self.0
                .lock()
                .map(|g| g.source.is_seekable())
                .unwrap_or(false)
        }

        fn byte_len(&self) -> Option<u64> {
            self.0.lock().ok().and_then(|g| g.source.byte_len())
        }
    }

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
            let mut hint = Hint::new();
            let mut has_hint = false;
            let cleaned_ext = extension_hint
                .map(|ext| ext.trim().trim_start_matches('.'))
                .filter(|ext| !ext.is_empty());
            if let Some(ext) = cleaned_ext {
                hint.with_extension(ext);
                has_hint = true;
            }
            let cleaned_mime = mime_hint
                .map(|mime| mime.split(';').next().unwrap_or(mime).trim())
                .filter(|mime| !mime.is_empty());
            if let Some(mime) = cleaned_mime {
                hint.mime_type(mime);
                has_hint = true;
            }

            let format_opts = FormatOptions {
                enable_gapless: true,
                ..Default::default()
            };
            let meta_opts = MetadataOptions::default();

            let shared = SharedMediaSource::new(source);
            let is_seekable = shared.is_seekable();
            let byte_len = shared.byte_len();

            let (probed, used_ext_hint) = if !has_hint {
                let mss = MediaSourceStream::new(Box::new(shared.clone()), Default::default());
                match symphonia::default::get_probe().format(&hint, mss, &format_opts, &meta_opts) {
                    Ok(res) => (res, None),
                    Err(err) => {
                        let head_bytes = shared.head_bytes();
                        let (head_hex, head_ascii) = shared.head_preview();
                        let err_msg = format_probe_failure_error(
                            &format!("symphonia probe failed: {err}"),
                            cleaned_mime,
                            byte_len,
                            &head_bytes,
                        );
                        log::warn!(
                            "{err_msg} (ext_hint=None, mime_hint=None, seekable={is_seekable}, byte_len={byte_len:?}, head_hex=[{head_hex}], head_ascii={head_ascii:?})"
                        );
                        return Err(AudioError::Decode(err_msg));
                    }
                }
            } else {
                let hinted_mss =
                    MediaSourceStream::new(Box::new(shared.clone()), Default::default());
                match symphonia::default::get_probe().format(
                    &hint,
                    hinted_mss,
                    &format_opts,
                    &meta_opts,
                ) {
                    Ok(res) => (res, cleaned_ext),
                    Err(first_err) => {
                        // Retry probing with an empty `Hint::new()` on the rewound stream so
                        // Symphonia auto-detects the true container format (WebM/M4A/MP4/AAC/FLAC/WAV/Ogg)
                        // by magic bytes when a plugin declared a mismatched type (e.g. "mp3").
                        if shared.rewind_to_start().is_ok() {
                            let fallback_mss = MediaSourceStream::new(
                                Box::new(shared.clone()),
                                Default::default(),
                            );
                            match symphonia::default::get_probe().format(
                                &Hint::new(),
                                fallback_mss,
                                &format_opts,
                                &meta_opts,
                            ) {
                                Ok(res) => (res, None),
                                Err(fallback_err) => {
                                    let head_bytes = shared.head_bytes();
                                    let (head_hex, head_ascii) = shared.head_preview();
                                    let first_msg = first_err.to_string();
                                    let fallback_msg = fallback_err.to_string();
                                    let base = if first_msg == fallback_msg {
                                        format!("symphonia probe failed: {first_msg}")
                                    } else {
                                        format!(
                                            "symphonia probe failed: {first_msg} (hintless retry: {fallback_msg})"
                                        )
                                    };
                                    let combined = format_probe_failure_error(
                                        &base,
                                        cleaned_mime,
                                        byte_len,
                                        &head_bytes,
                                    );
                                    log::warn!(
                                        "{combined} (ext_hint={cleaned_ext:?}, mime_hint={cleaned_mime:?}, seekable={is_seekable}, byte_len={byte_len:?}, head_hex=[{head_hex}], head_ascii={head_ascii:?})"
                                    );
                                    return Err(AudioError::Decode(combined));
                                }
                            }
                        } else {
                            let head_bytes = shared.head_bytes();
                            let (head_hex, head_ascii) = shared.head_preview();
                            let err_msg = format_probe_failure_error(
                                &format!("symphonia probe failed: {first_err}"),
                                cleaned_mime,
                                byte_len,
                                &head_bytes,
                            );
                            log::warn!(
                                "{err_msg} (rewind unsupported, ext_hint={cleaned_ext:?}, mime_hint={cleaned_mime:?}, seekable={is_seekable}, byte_len={byte_len:?}, head_hex=[{head_hex}], head_ascii={head_ascii:?})"
                            );
                            return Err(AudioError::Decode(err_msg));
                        }
                    }
                }
            };

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
            let decoder = symphonia_codec_registry()
                .make(&codec_params, &DecoderOptions::default())
                .map_err(|e| AudioError::Decode(format!("symphonia codec init failed: {e}")))?;

            let active_params = decoder.codec_params();
            let sample_rate = active_params
                .sample_rate
                .or(codec_params.sample_rate)
                .unwrap_or(44_100);
            let channels = active_params
                .channels
                .or_else(|| active_params.channel_layout.map(|l| l.into_channels()))
                .or(codec_params.channels)
                .or_else(|| codec_params.channel_layout.map(|l| l.into_channels()))
                .map(|c| c.count() as u16)
                .unwrap_or(2)
                .max(1);
            let time_base = active_params.time_base.or(codec_params.time_base);
            let n_frames = active_params.n_frames.or(codec_params.n_frames);
            let duration_ms = match (n_frames, time_base) {
                (Some(n_frames), Some(tb)) => {
                    let t = tb.calc_time(n_frames);
                    Some(t.seconds.saturating_mul(1000) + (t.frac * 1000.0).round() as u64)
                }
                (Some(n_frames), None) if sample_rate > 0 => {
                    Some(n_frames.saturating_mul(1000) / sample_rate as u64)
                }
                _ => None,
            };

            let active_codec = if active_params.codec != CODEC_TYPE_NULL {
                active_params.codec
            } else {
                codec_params.codec
            };

            let spec = AudioSpec {
                sample_rate,
                channels,
                duration_ms,
                bit_rate: None,
                codec_name: canonical_symphonia_codec_name(active_codec, used_ext_hint),
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
            self.decoded_frames = (landed_ms.saturating_mul(self.spec.sample_rate as u64)) / 1000;
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
        // Check container formats before codec names (e.g. `audio/webm; codecs="opus"` -> `webm`,
        // `audio/mp4; codecs="mp4a.40.2"` -> `m4a`, raw `audio/aac` -> `aac`).
        if lower.contains("webm") || lower.contains("matroska") {
            return Some("webm".to_owned());
        }
        if lower.contains("mp4") || lower.contains("m4a") {
            return Some("m4a".to_owned());
        }
        if lower.contains("aac") {
            return Some("aac".to_owned());
        }
        if lower.contains("mpeg") || lower.contains("mp3") {
            return Some("mp3".to_owned());
        }
        if lower.contains("ogg") || lower.contains("opus") || lower.contains("vorbis") {
            return Some("ogg".to_owned());
        }
        if lower.contains("wav") {
            return Some("wav".to_owned());
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
        if let Some(ref ct) = content_type {
            let base_mime = ct
                .split(';')
                .next()
                .unwrap_or(ct)
                .trim()
                .to_ascii_lowercase();
            let path_lower = url
                .split(['?', '#'])
                .next()
                .unwrap_or(url)
                .to_ascii_lowercase();
            let url_is_hls = path_lower.ends_with(".m3u8")
                || path_lower.contains("/hls_")
                || path_lower.contains("/manifest/hls");
            let is_non_audio = base_mime.starts_with("text/html")
                || base_mime.starts_with("application/xhtml")
                || base_mime.starts_with("application/json")
                || (base_mime.starts_with("text/plain") && !url_is_hls);
            if is_non_audio {
                log::warn!(
                    "rejecting non-audio HTTP response ({base_mime}) before Symphonia probe"
                );
                return Err(crate::AudioError::Decode(format!(
                    "non-audio HTTP response ({base_mime})"
                )));
            }
        }
        if stream.byte_len() == Some(0) {
            log::warn!("rejecting empty audio stream (0 bytes) before Symphonia probe");
            return Err(crate::AudioError::Decode(
                "empty audio stream (0 bytes)".into(),
            ));
        }

        let http_mime = content_type.as_deref().and_then(|ct| {
            let base = ct.split(';').next().unwrap_or(ct).trim();
            if base.is_empty()
                || base.eq_ignore_ascii_case("application/octet-stream")
                || base.eq_ignore_ascii_case("binary/octet-stream")
            {
                None
            } else {
                Some(ct)
            }
        });
        let mime = http_mime.or_else(|| media_type.filter(|m| m.contains('/')));
        // When the HTTP response advertises an explicit container MIME (e.g. `audio/webm`
        // or `audio/mp4` or `audio/aac`), derive the extension hint from it rather than
        // keeping a contradictory plugin `media_type` such as `"mp3"`.
        let effective_ext_hint = http_mime
            .and_then(|ct| infer_extension_hint("", Some(ct)))
            .or(ext_hint);
        match SymphoniaDecoder::open(Box::new(stream), effective_ext_hint.as_deref(), mime) {
            Ok(decoder) => Ok(Box::new(decoder)),
            Err(symphonia_err) => {
                #[cfg(feature = "audio-ffmpeg")]
                if preference != DecoderPreference::SymphoniaOnly {
                    log::info!(
                        "symphonia decode failed ({symphonia_err}); falling back to static FFmpeg"
                    );
                    let fallback_stream = cache.open_progressive(url, headers)?;
                    let dec =
                        crate::ffmpeg_backend::FfmpegDecoder::open(Box::new(fallback_stream))?;
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

#[cfg(all(test, feature = "audio-symphonia"))]
mod tests {
    use std::f32::consts::TAU;
    use std::io::Cursor;

    use super::{AudioDecoder, SymphoniaDecoder, infer_extension_hint};

    fn make_wav_bytes(sample_rate: u32, channels: u16, num_frames: u32) -> Vec<u8> {
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
            let sample = ((TAU * 440.0 * t).sin() * 14_000.0) as i16;
            for _ in 0..channels {
                wav.extend_from_slice(&sample.to_le_bytes());
            }
        }
        wav
    }

    fn ebml_vint_size(len: usize) -> Vec<u8> {
        if len < 0x7F {
            vec![0x80 | (len as u8)]
        } else if len < 0x3FFF {
            vec![0x40 | ((len >> 8) as u8), (len & 0xFF) as u8]
        } else {
            vec![
                0x20 | ((len >> 16) as u8),
                ((len >> 8) & 0xFF) as u8,
                (len & 0xFF) as u8,
            ]
        }
    }

    fn ebml_element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(id.len() + 4 + payload.len());
        out.extend_from_slice(id);
        out.extend_from_slice(&ebml_vint_size(payload.len()));
        out.extend_from_slice(payload);
        out
    }

    /// Synthesizes a minimal valid WebM/Matroska container carrying an `A_OPUS` track.
    fn make_webm_opus_bytes(packets: &[&[u8]]) -> Vec<u8> {
        let opus_head = crate::opus_codec::tests::make_opus_head(2, 0, 0);

        // EBML Header
        let mut ebml_body = Vec::new();
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0x86], &[1])); // EBMLVersion
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0xF7], &[1])); // EBMLReadVersion
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0xF2], &[4])); // EBMLMaxIDLength
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0xF3], &[8])); // EBMLMaxSizeLength
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0x82], b"webm")); // DocType
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0x87], &[4])); // DocTypeVersion
        ebml_body.extend_from_slice(&ebml_element(&[0x42, 0x85], &[2])); // DocTypeReadVersion
        let ebml_hdr = ebml_element(&[0x1A, 0x45, 0xDF, 0xA3], &ebml_body);

        // Segment -> Info (TimestampScale = 1_000_000 ns = 1 ms)
        let mut info_body = Vec::new();
        info_body.extend_from_slice(&ebml_element(
            &[0x2A, 0xD7, 0xB1],
            &1_000_000u32.to_be_bytes(),
        ));
        let info = ebml_element(&[0x15, 0x49, 0xA9, 0x66], &info_body);

        // Segment -> Tracks -> TrackEntry (A_OPUS, 48kHz, 2ch, CodecPrivate = OpusHead)
        let mut audio_body = Vec::new();
        audio_body.extend_from_slice(&ebml_element(&[0xB5], &48_000.0f32.to_be_bytes())); // SamplingFrequency
        audio_body.extend_from_slice(&ebml_element(&[0x9F], &[2])); // Channels
        let audio = ebml_element(&[0xE1], &audio_body);

        let mut track_entry_body = Vec::new();
        track_entry_body.extend_from_slice(&ebml_element(&[0xD7], &[1])); // TrackNumber = 1
        track_entry_body.extend_from_slice(&ebml_element(&[0x73, 0xC5], &[1])); // TrackUID = 1
        track_entry_body.extend_from_slice(&ebml_element(&[0x83], &[2])); // TrackType = 2 (audio)
        track_entry_body.extend_from_slice(&ebml_element(&[0x86], b"A_OPUS")); // CodecID
        track_entry_body.extend_from_slice(&ebml_element(&[0x63, 0xA2], &opus_head)); // CodecPrivate
        track_entry_body.extend_from_slice(&audio);
        let track_entry = ebml_element(&[0xAE], &track_entry_body);
        let tracks = ebml_element(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);

        // Segment -> Cluster with SimpleBlocks
        let mut cluster_body = Vec::new();
        cluster_body.extend_from_slice(&ebml_element(&[0xE7], &[0])); // Cluster Timestamp = 0
        for (i, pkt) in packets.iter().enumerate() {
            let rel_ts = (i as i16) * 20;
            let mut block = Vec::with_capacity(4 + pkt.len());
            block.push(0x81); // Track number 1 as VINT
            block.extend_from_slice(&rel_ts.to_be_bytes());
            block.push(0x80); // Keyframe flag
            block.extend_from_slice(pkt);
            cluster_body.extend_from_slice(&ebml_element(&[0xA3], &block));
        }
        let cluster = ebml_element(&[0x1F, 0x43, 0xB6, 0x75], &cluster_body);

        let mut seg_body = Vec::new();
        seg_body.extend_from_slice(&info);
        seg_body.extend_from_slice(&tracks);
        seg_body.extend_from_slice(&cluster);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &seg_body);

        let mut webm = Vec::with_capacity(ebml_hdr.len() + segment.len());
        webm.extend_from_slice(&ebml_hdr);
        webm.extend_from_slice(&segment);
        webm
    }

    fn ogg_crc32(data: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &byte in data {
            crc ^= (byte as u32) << 24;
            for _ in 0..8 {
                if (crc & 0x8000_0000) != 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc
    }

    fn make_ogg_page(
        header_type: u8,
        granule_pos: u64,
        serial: u32,
        seq: u32,
        packets: &[&[u8]],
    ) -> Vec<u8> {
        let mut lacing = Vec::new();
        let mut body = Vec::new();
        for pkt in packets {
            let mut rem = pkt.len();
            while rem >= 255 {
                lacing.push(255u8);
                rem -= 255;
            }
            lacing.push(rem as u8);
            body.extend_from_slice(pkt);
        }
        let mut page = Vec::with_capacity(27 + lacing.len() + body.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // version
        page.push(header_type);
        page.extend_from_slice(&granule_pos.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&seq.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        page.extend_from_slice(&body);
        let crc = ogg_crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    fn make_ogg_opus_bytes(packets: &[&[u8]]) -> Vec<u8> {
        let opus_head = crate::opus_codec::tests::make_opus_head(2, 0, 0);
        let mut opus_tags = Vec::new();
        opus_tags.extend_from_slice(b"OpusTags");
        opus_tags.extend_from_slice(&3u32.to_le_bytes());
        opus_tags.extend_from_slice(b"nsh");
        opus_tags.extend_from_slice(&0u32.to_le_bytes());

        let mut ogg = Vec::new();
        ogg.extend_from_slice(&make_ogg_page(0x02, 0, 1, 0, &[&opus_head])); // BOS
        ogg.extend_from_slice(&make_ogg_page(0x00, 0, 1, 1, &[&opus_tags]));
        let granule = (packets.len() as u64) * 960;
        ogg.extend_from_slice(&make_ogg_page(0x04, granule, 1, 2, packets)); // EOS
        ogg
    }

    #[test]
    fn test_symphonia_hintless_fallback_probing_with_mismatched_mp3_hint() {
        // Simulate a plugin declaring type="mp3" when the actual bytes relayed by
        // VirtualFfmpegShim are another container (here WAV PCM) detected by magic bytes.
        let wav_bytes = make_wav_bytes(44_100, 2, 4_410);
        let mut dec = SymphoniaDecoder::open(
            Box::new(Cursor::new(wav_bytes)),
            Some("mp3"),
            Some("audio/mpeg"),
        )
        .expect("SymphoniaDecoder::open should auto-detect WAV even with mismatched mp3 hint");

        let spec = dec.spec();
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.codec_name, "wav");

        let mut pcm = Vec::new();
        let n = dec.read_pcm(&mut pcm).expect("decode pcm");
        assert!(n > 0);
        let peak = pcm.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak > 0.25, "expected audible PCM signal, peak={peak}");
    }

    #[test]
    fn test_symphonia_webm_opus_decode_with_mismatched_mp3_hint() {
        let pkt = crate::opus_codec::tests::SAMPLE_OPUS_20MS_STEREO_PACKET;
        let webm = make_webm_opus_bytes(&[pkt, pkt, pkt]);
        let mut dec =
            SymphoniaDecoder::open(Box::new(Cursor::new(webm)), Some("mp3"), Some("audio/mpeg"))
                .expect(
                    "WebM/Opus must probe and decode via SymphoniaOpusDecoder even with mp3 hint",
                );

        let spec = dec.spec();
        assert_eq!(spec.codec_name, "opus");
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 2);

        let mut total_samples = 0usize;
        let mut pcm = Vec::new();
        while let Ok(n) = dec.read_pcm(&mut pcm) {
            if n == 0 {
                break;
            }
            total_samples += n;
        }
        assert_eq!(total_samples, 3 * 960 * 2);
    }

    #[test]
    fn test_symphonia_ogg_opus_decode() {
        let pkt = crate::opus_codec::tests::SAMPLE_OPUS_20MS_STEREO_PACKET;
        let ogg = make_ogg_opus_bytes(&[pkt, pkt]);
        let mut dec = SymphoniaDecoder::open(Box::new(Cursor::new(ogg)), None, None)
            .expect("Ogg/Opus must probe and decode via SymphoniaOpusDecoder");

        let spec = dec.spec();
        assert_eq!(spec.codec_name, "opus");
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 2);

        let mut pcm = Vec::new();
        let mut total_samples = 0usize;
        while let Ok(n) = dec.read_pcm(&mut pcm) {
            if n == 0 {
                break;
            }
            total_samples += n;
        }
        assert_eq!(total_samples, 2 * 960 * 2);
    }

    #[test]
    fn test_infer_extension_hint_container_priority() {
        assert_eq!(
            infer_extension_hint(
                "http://127.0.0.1:9000/",
                Some("audio/webm; codecs=\"opus\"")
            )
            .as_deref(),
            Some("webm")
        );
        assert_eq!(
            infer_extension_hint(
                "http://127.0.0.1:9000/",
                Some("audio/mp4; codecs=\"mp4a.40.2\"")
            )
            .as_deref(),
            Some("m4a")
        );
        assert_eq!(
            infer_extension_hint("http://127.0.0.1:9000/", Some("audio/aac")).as_deref(),
            Some("aac")
        );
        assert_eq!(
            infer_extension_hint("http://127.0.0.1:9000/", Some("audio/ogg; codecs=\"opus\""))
                .as_deref(),
            Some("ogg")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_open_decoder_rejects_text_html_and_empty_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let srv = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    if path == "/html" {
                        let body = b"<!DOCTYPE html><html><body>YouTube Music</body></html>";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    } else {
                        let hdr = "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(hdr.as_bytes()).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });

        let dir =
            std::env::temp_dir().join(format!("nsh-decoder-non-audio-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cache = crate::AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");

        let html_err = match crate::open_decoder(
            &cache,
            &format!("http://{addr}/html"),
            Vec::new(),
            Some("mp3"),
            None,
            crate::DecoderPreference::SymphoniaOnly,
        ) {
            Ok(_) => panic!("text/html response must be rejected before Symphonia probe"),
            Err(err) => err,
        };
        assert!(
            html_err
                .to_string()
                .contains("non-audio HTTP response (text/html)"),
            "unexpected error: {html_err}"
        );

        let empty_err = match crate::open_decoder(
            &cache,
            &format!("http://{addr}/empty"),
            Vec::new(),
            Some("mp3"),
            None,
            crate::DecoderPreference::SymphoniaOnly,
        ) {
            Ok(_) => panic!("0-byte audio response must be rejected before Symphonia probe"),
            Err(err) => err,
        };
        assert!(
            empty_err
                .to_string()
                .contains("empty audio stream (0 bytes)"),
            "unexpected error: {empty_err}"
        );

        let _ = srv.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_symphonia_probe_failure_diagnostics_empty_html_and_binary() {
        let empty_err = match SymphoniaDecoder::open(Box::new(Cursor::new(Vec::new())), None, None)
        {
            Ok(_) => panic!("empty stream must fail"),
            Err(err) => err.to_string(),
        };
        assert!(
            empty_err.contains("empty audio stream (0 bytes)"),
            "unexpected empty error: {empty_err}"
        );

        let html_err = match SymphoniaDecoder::open(
            Box::new(Cursor::new(b"<!DOCTYPE html><html></html>".to_vec())),
            Some("mp3"),
            Some("audio/mpeg"),
        ) {
            Ok(_) => panic!("HTML bytes must fail"),
            Err(err) => err.to_string(),
        };
        assert!(
            html_err.contains("[mime=audio/mpeg, len=Some(28), head=<!DOCTYPE html><]"),
            "expected diagnostic suffix with HTML head preview, got: {html_err}"
        );

        let bin_err = match SymphoniaDecoder::open(
            Box::new(Cursor::new(vec![
                0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05,
            ])),
            None,
            None,
        ) {
            Ok(_) => panic!("unknown binary bytes must fail"),
            Err(err) => err.to_string(),
        };
        assert!(
            bin_err.contains("[mime=unknown, len=Some(9), head=de ad be ef 01 02 03 04]"),
            "expected diagnostic suffix with 8-byte hex head, got: {bin_err}"
        );
    }
}
