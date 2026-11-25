//! Pure-Rust Symphonia [`Decoder`] implementation for Opus (`CODEC_TYPE_OPUS`)
//! backed by [`opus_pure`] (zero-dependency pure Rust, zero C/FFmpeg dependency).
//!
//! Supports RFC 7845 `OpusHead` headers from WebM/Matroska (`A_OPUS` `CodecPrivate`)
//! and Ogg/Opus containers, including mono/stereo (`mapping_family == 0`), multistream
//! surround (`mapping_family != 0`), `pre_skip` sample trimming, Q7.8 dB `output_gain`,
//! and clean seek resets.

use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder, OpusMSDecoder};
use symphonia::core::audio::{
    AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, Layout, Signal, SignalSpec,
};
use symphonia::core::codecs::{
    CODEC_TYPE_OPUS, CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult,
};
use symphonia::core::errors::{Error, Result};
use symphonia::core::formats::Packet;
use symphonia::core::support_codec;

/// Canonical Opus internal output sample rate (RFC 6716 / RFC 7845).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// RFC 7845 `OpusHead` magic signature.
const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";

enum InnerOpusDecoder {
    Simple(Box<OpusDecoder>),
    Multistream(Box<OpusMSDecoder>),
}

impl InnerOpusDecoder {
    fn decode_float(
        &mut self,
        packet: &[u8],
        pcm: &mut [f32],
    ) -> std::result::Result<usize, opus_pure::Error> {
        match self {
            Self::Simple(dec) => dec.decode(packet, MAX_PACKET_SAMPLES, pcm),
            Self::Multistream(dec) => dec.decode(packet, MAX_PACKET_SAMPLES, pcm),
        }
    }

    fn reset(&mut self) {
        let _ = match self {
            Self::Simple(dec) => dec.reset_state(),
            Self::Multistream(dec) => dec.reset_state(),
        };
    }
}

#[derive(Debug, Clone)]
struct ParsedOpusHead {
    channels: usize,
    pre_skip: u32,
    output_gain_linear: f32,
    mapping_family: u8,
}

fn parse_opus_head(extra: &[u8]) -> Option<ParsedOpusHead> {
    if extra.len() < 19 || &extra[..8] != OPUS_HEAD_MAGIC {
        return None;
    }
    let version = extra[8];
    // RFC 7845 §5.1: major version (upper 4 bits) must be 0.
    if (version & 0xF0) != 0 {
        return None;
    }
    let channels = extra[9] as usize;
    if channels == 0 {
        return None;
    }
    let pre_skip = u16::from_le_bytes([extra[10], extra[11]]) as u32;
    let output_gain_q78 = i16::from_le_bytes([extra[16], extra[17]]);
    let output_gain_linear = if output_gain_q78 == 0 {
        1.0
    } else {
        10.0f32.powf((output_gain_q78 as f32) / (256.0 * 20.0))
    };
    let mapping_family = extra[18];

    if mapping_family == 0 {
        if channels > 2 {
            return None;
        }
        return Some(ParsedOpusHead {
            channels,
            pre_skip,
            output_gain_linear,
            mapping_family,
        });
    }

    if extra.len() < 21 + channels {
        return None;
    }
    let stream_count = extra[19] as usize;
    let coupled_count = extra[20] as usize;
    if stream_count == 0 || coupled_count > stream_count {
        return None;
    }

    Some(ParsedOpusHead {
        channels,
        pre_skip,
        output_gain_linear,
        mapping_family,
    })
}

fn symphonia_channels_for_count(count: usize) -> Option<Channels> {
    match count {
        1 => Some(Layout::Mono.into_channels()),
        2 => Some(Layout::Stereo.into_channels()),
        3 => Some(Channels::FRONT_LEFT | Channels::FRONT_CENTRE | Channels::FRONT_RIGHT),
        4 => Some(
            Channels::FRONT_LEFT
                | Channels::FRONT_RIGHT
                | Channels::REAR_LEFT
                | Channels::REAR_RIGHT,
        ),
        5 => Some(
            Channels::FRONT_LEFT
                | Channels::FRONT_CENTRE
                | Channels::FRONT_RIGHT
                | Channels::REAR_LEFT
                | Channels::REAR_RIGHT,
        ),
        6 => Some(Layout::FivePointOne.into_channels()),
        7 => Some(
            Channels::FRONT_LEFT
                | Channels::FRONT_CENTRE
                | Channels::FRONT_RIGHT
                | Channels::SIDE_LEFT
                | Channels::SIDE_RIGHT
                | Channels::REAR_CENTRE
                | Channels::LFE1,
        ),
        8 => Some(
            Channels::FRONT_LEFT
                | Channels::FRONT_CENTRE
                | Channels::FRONT_RIGHT
                | Channels::SIDE_LEFT
                | Channels::SIDE_RIGHT
                | Channels::REAR_LEFT
                | Channels::REAR_RIGHT
                | Channels::LFE1,
        ),
        _ => None,
    }
}

/// Symphonia [`Decoder`] adapter for pure-Rust Opus decoding.
pub struct SymphoniaOpusDecoder {
    params: CodecParameters,
    inner: InnerOpusDecoder,
    channels: usize,
    output_gain_linear: f32,
    remaining_pre_skip: u32,
    pcm_scratch: Vec<f32>,
    audio_buf: AudioBuffer<f32>,
}

impl Decoder for SymphoniaOpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> Result<Self> {
        if params.codec != CODEC_TYPE_OPUS {
            return Err(Error::Unsupported("not an Opus codec stream"));
        }

        let parsed = params.extra_data.as_deref().and_then(parse_opus_head);

        let (inner, channels, pre_skip, output_gain_linear) = if let Some(head) = parsed {
            let dec = if head.mapping_family == 0 {
                OpusDecoder::new(OPUS_SAMPLE_RATE as i32, head.channels)
                    .map(|d| InnerOpusDecoder::Simple(Box::new(d)))
                    .map_err(|err| {
                        log::warn!("opus decoder init failed: {err}");
                        Error::DecodeError("invalid opus stream parameters")
                    })?
            } else {
                OpusMSDecoder::new(OPUS_SAMPLE_RATE as i32, head.channels, head.mapping_family)
                    .map(|d| InnerOpusDecoder::Multistream(Box::new(d)))
                    .map_err(|err| {
                        log::warn!("opus multistream decoder init failed: {err}");
                        Error::DecodeError("invalid opus multistream parameters")
                    })?
            };
            let effective_pre_skip = if head.pre_skip > 0 {
                head.pre_skip
            } else {
                params.delay.unwrap_or(0)
            };
            (
                dec,
                head.channels,
                effective_pre_skip,
                head.output_gain_linear,
            )
        } else {
            let inferred_channels = params
                .channels
                .or_else(|| params.channel_layout.map(|l| l.into_channels()))
                .map(|c| c.count())
                .unwrap_or(2)
                .clamp(1, 2);
            let dec = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, inferred_channels)
                .map(|d| InnerOpusDecoder::Simple(Box::new(d)))
                .map_err(|err| {
                    log::warn!("opus fallback decoder init failed: {err}");
                    Error::DecodeError("invalid opus fallback parameters")
                })?;
            let pre_skip = params.delay.unwrap_or(0);
            (dec, inferred_channels, pre_skip, 1.0)
        };

        let symphonia_channels = params
            .channels
            .or_else(|| params.channel_layout.map(|l| l.into_channels()))
            .filter(|c| c.count() == channels)
            .or_else(|| symphonia_channels_for_count(channels))
            .ok_or(Error::Unsupported("unsupported Opus channel count"))?;

        let mut updated_params = params.clone();
        updated_params.with_sample_rate(OPUS_SAMPLE_RATE);
        updated_params.with_channels(symphonia_channels);
        if pre_skip > 0 {
            updated_params.with_delay(pre_skip);
        }

        let spec = SignalSpec::new(OPUS_SAMPLE_RATE, symphonia_channels);
        let audio_buf = AudioBuffer::<f32>::new(MAX_PACKET_SAMPLES as u64, spec);
        let pcm_scratch = vec![0.0f32; MAX_PACKET_SAMPLES * channels];

        Ok(Self {
            params: updated_params,
            inner,
            channels,
            output_gain_linear,
            remaining_pre_skip: pre_skip,
            pcm_scratch,
            audio_buf,
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[support_codec!(
            CODEC_TYPE_OPUS,
            "opus",
            "Opus Audio (Pure Rust)"
        )]
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.audio_buf.clear();
        // Do not re-apply initial stream pre_skip after seeking.
        self.remaining_pre_skip = 0;
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> Result<AudioBufferRef<'_>> {
        // Symphonia contract: clear output buffer before decoding or returning an error.
        self.audio_buf.clear();

        let data = packet.buf();
        if data.is_empty() {
            return Ok(self.audio_buf.as_audio_buffer_ref());
        }

        let decoded_frames = self
            .inner
            .decode_float(data, &mut self.pcm_scratch)
            .map_err(|err| {
                log::debug!("opus packet decode error: {err}");
                Error::DecodeError("opus packet decode error")
            })?;

        if decoded_frames == 0 {
            return Ok(self.audio_buf.as_audio_buffer_ref());
        }

        let skip = decoded_frames.min(self.remaining_pre_skip as usize);
        self.remaining_pre_skip = self.remaining_pre_skip.saturating_sub(skip as u32);
        let keep = decoded_frames.saturating_sub(skip);
        if keep == 0 {
            return Ok(self.audio_buf.as_audio_buffer_ref());
        }

        self.audio_buf.render_reserved(Some(keep));
        let ch = self.channels;
        let gain = self.output_gain_linear;
        if (gain - 1.0).abs() < 1e-6 {
            for c in 0..ch {
                let plane = self.audio_buf.chan_mut(c);
                for (i, sample) in plane.iter_mut().take(keep).enumerate() {
                    *sample = self.pcm_scratch[(skip + i) * ch + c];
                }
            }
        } else {
            for c in 0..ch {
                let plane = self.audio_buf.chan_mut(c);
                for (i, sample) in plane.iter_mut().take(keep).enumerate() {
                    *sample = self.pcm_scratch[(skip + i) * ch + c] * gain;
                }
            }
        }

        Ok(self.audio_buf.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.audio_buf.as_audio_buffer_ref()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use symphonia::core::audio::{AudioBufferRef, Layout, Signal};
    use symphonia::core::codecs::{CODEC_TYPE_OPUS, CodecParameters, Decoder, DecoderOptions};
    use symphonia::core::formats::Packet;

    use super::{OPUS_SAMPLE_RATE, SymphoniaOpusDecoder, parse_opus_head};

    /// Builds a valid RFC 7845 `OpusHead` packet (mapping family 0).
    pub(crate) fn make_opus_head(channels: u8, pre_skip: u16, output_gain_q78: i16) -> Vec<u8> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(channels);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes()); // original input sample rate
        head.extend_from_slice(&output_gain_q78.to_le_bytes());
        head.push(0); // mapping family 0
        head
    }

    /// Canonical 20ms stereo CELT FB Opus frame (960 samples/channel at 48 kHz).
    pub(crate) const SAMPLE_OPUS_20MS_STEREO_PACKET: &[u8] = &[0xFC, 0xFF, 0xFE];

    #[test]
    fn test_parse_opus_head_and_decode_preskip_gain_and_reset() {
        // +256 Q7.8 = +1.0 dB (~1.1220185 linear)
        let head_bytes = make_opus_head(2, 312, 256);
        let parsed = parse_opus_head(&head_bytes).expect("parse OpusHead");
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.pre_skip, 312);
        assert!((parsed.output_gain_linear - 10.0f32.powf(1.0 / 20.0)).abs() < 1e-4);

        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_OPUS)
            .with_extra_data(head_bytes.into_boxed_slice());

        let mut dec = SymphoniaOpusDecoder::try_new(&params, &DecoderOptions::default())
            .expect("create SymphoniaOpusDecoder");
        assert_eq!(dec.codec_params().sample_rate, Some(OPUS_SAMPLE_RATE));
        assert_eq!(dec.codec_params().channels.map(|c| c.count()), Some(2));
        assert_eq!(dec.codec_params().delay, Some(312));

        // 1st packet: 960 frames decoded - 312 pre_skip = 648 frames emitted.
        let pkt1 = Packet::new_from_slice(0, 0, 960, SAMPLE_OPUS_20MS_STEREO_PACKET);
        let buf1 = dec.decode(&pkt1).expect("decode packet 1");
        match buf1 {
            AudioBufferRef::F32(b) => {
                assert_eq!(b.frames(), 960 - 312);
                assert_eq!(b.spec().rate, OPUS_SAMPLE_RATE);
                assert_eq!(b.spec().channels.count(), 2);
            }
            _ => panic!("expected F32 audio buffer"),
        }

        // 2nd packet: full 960 frames emitted.
        let pkt2 = Packet::new_from_slice(0, 960, 960, SAMPLE_OPUS_20MS_STEREO_PACKET);
        let buf2 = dec.decode(&pkt2).expect("decode packet 2");
        assert_eq!(buf2.frames(), 960);

        // After reset() (e.g. seek), pre_skip must stay 0 so full 960 frames are emitted.
        dec.reset();
        let pkt3 = Packet::new_from_slice(0, 1920, 960, SAMPLE_OPUS_20MS_STEREO_PACKET);
        let buf3 = dec.decode(&pkt3).expect("decode packet 3 after reset");
        assert_eq!(buf3.frames(), 960);
    }

    #[test]
    fn test_opus_multistream_head_and_fallback_without_extra_data() {
        // 5.1 surround OpusHead (mapping family 1: 6 channels, 4 streams, 2 coupled).
        let mut head_51 = Vec::with_capacity(27);
        head_51.extend_from_slice(b"OpusHead");
        head_51.push(1);
        head_51.push(6);
        head_51.extend_from_slice(&0u16.to_le_bytes());
        head_51.extend_from_slice(&48_000u32.to_le_bytes());
        head_51.extend_from_slice(&0i16.to_le_bytes());
        head_51.push(1); // mapping family 1
        head_51.push(4); // stream_count
        head_51.push(2); // coupled_count
        head_51.extend_from_slice(&[0, 4, 1, 2, 3, 5]);

        let mut params_51 = CodecParameters::new();
        params_51
            .for_codec(CODEC_TYPE_OPUS)
            .with_extra_data(head_51.into_boxed_slice());
        let dec_51 = SymphoniaOpusDecoder::try_new(&params_51, &DecoderOptions::default())
            .expect("create 5.1 multistream opus decoder");
        assert_eq!(dec_51.codec_params().channels.map(|c| c.count()), Some(6));

        // Fallback when extra_data is absent and only channel_layout is set (MKV style).
        let mut params_fallback = CodecParameters::new();
        params_fallback
            .for_codec(CODEC_TYPE_OPUS)
            .with_channel_layout(Layout::Stereo);
        let mut dec_fb =
            SymphoniaOpusDecoder::try_new(&params_fallback, &DecoderOptions::default())
                .expect("create fallback opus decoder");
        let pkt = Packet::new_from_slice(0, 0, 960, SAMPLE_OPUS_20MS_STEREO_PACKET);
        let buf = dec_fb.decode(&pkt).expect("decode fallback packet");
        assert_eq!(buf.frames(), 960);
    }
}
