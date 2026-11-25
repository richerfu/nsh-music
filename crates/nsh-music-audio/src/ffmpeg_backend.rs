use serde::{Deserialize, Serialize};

/// Runtime capability report for audio decoding, static FFmpeg availability,
/// virtual plugin shim support, and active output backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioBackendCapabilities {
    pub symphonia_enabled: bool,
    pub ffmpeg_enabled: bool,
    pub ffmpeg_version: String,
    pub virtual_ffmpeg_shim: bool,
    pub active_output_backend: String,
    pub supported_formats: Vec<String>,
}

impl AudioBackendCapabilities {
    /// Serializes backend capabilities as formatted JSON for diagnostics and plugin inspection.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Reports audio backend capabilities compiled into the current binary.
pub fn audio_backend_capabilities() -> AudioBackendCapabilities {
    let mut formats = vec!["demo".to_owned()];

    #[cfg(feature = "audio-symphonia")]
    {
        formats.extend(
            [
                "mp3", "flac", "aac", "m4a", "mp4", "ogg", "opus", "vorbis", "wav", "alac", "aiff",
                "webm", "mkv",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }

    #[cfg(feature = "audio-ffmpeg")]
    {
        for extra in [
            "wma", "ac3", "eac3", "ape", "dsf", "dff", "mka", "ts", "hls",
        ] {
            if !formats.iter().any(|f| f == extra) {
                formats.push(extra.to_owned());
            }
        }
    }

    let active_output_backend = {
        #[cfg(target_env = "ohos")]
        {
            "ohos-audio".to_owned()
        }
        #[cfg(all(not(target_env = "ohos"), not(target_arch = "wasm32")))]
        {
            "cpal".to_owned()
        }
        #[cfg(target_arch = "wasm32")]
        {
            "null".to_owned()
        }
    };

    AudioBackendCapabilities {
        symphonia_enabled: cfg!(feature = "audio-symphonia"),
        ffmpeg_enabled: cfg!(feature = "audio-ffmpeg"),
        ffmpeg_version: embedded_ffmpeg_version(),
        virtual_ffmpeg_shim: true,
        active_output_backend,
        supported_formats: formats,
    }
}

/// Returns the embedded FFmpeg version string when `audio-ffmpeg` is enabled, or
/// the virtual shim banner version otherwise.
pub fn embedded_ffmpeg_version() -> String {
    #[cfg(feature = "audio-ffmpeg")]
    {
        // SAFETY: `av_version_info` returns a static null-terminated C string.
        let ptr = unsafe { ffmpeg_sys_next::av_version_info() };
        if !ptr.is_null() {
            if let Ok(s) = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str() {
                return format!("{s} (static-audio-avio)");
            }
        }
        let ver = unsafe { ffmpeg_sys_next::avformat_version() };
        let major = (ver >> 16) & 0xff;
        let minor = (ver >> 8) & 0xff;
        let micro = ver & 0xff;
        format!("libavformat-{major}.{minor}.{micro} (static-audio-avio)")
    }

    #[cfg(not(feature = "audio-ffmpeg"))]
    {
        "virtual-shim-7.1".to_owned()
    }
}

#[cfg(feature = "audio-ffmpeg")]
mod ffmpeg_impl {
    use std::ffi::{CStr, c_int, c_void};
    use std::io::{Read, Seek, SeekFrom};
    use std::ptr;

    use ffmpeg_sys_next as ff;

    use crate::decoder::{AudioDecoder, AudioSpec};
    use crate::{AudioError, AudioResult};

    const AVIO_BUFFER_SIZE: usize = 32 * 1024;

    /// Trait alias for any seekable Rust byte stream backing a custom FFmpeg `AVIOContext`.
    pub trait ReadSeek: Read + Seek + Send {}
    impl<T: Read + Seek + Send> ReadSeek for T {}

    struct AvioOpaque {
        stream: Box<dyn ReadSeek>,
    }

    unsafe extern "C" fn avio_read_packet(
        opaque: *mut c_void,
        buf: *mut u8,
        buf_size: c_int,
    ) -> c_int {
        if opaque.is_null() || buf.is_null() || buf_size <= 0 {
            return ff::AVERROR_INVALIDDATA;
        }
        let ctx = unsafe { &mut *(opaque as *mut AvioOpaque) };
        let slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
        match ctx.stream.read(slice) {
            Ok(0) => ff::AVERROR_EOF,
            Ok(n) => n as c_int,
            Err(_) => ff::AVERROR_EOF,
        }
    }

    unsafe extern "C" fn avio_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
        if opaque.is_null() {
            return -1;
        }
        let ctx = unsafe { &mut *(opaque as *mut AvioOpaque) };
        if (whence & ff::AVSEEK_SIZE as c_int) != 0 {
            let Ok(cur) = ctx.stream.stream_position() else {
                return -1;
            };
            let len = match ctx.stream.seek(SeekFrom::End(0)) {
                Ok(end) => {
                    let _ = ctx.stream.seek(SeekFrom::Start(cur));
                    end as i64
                }
                Err(_) => -1,
            };
            return len;
        }

        let base_whence = whence & !(ff::AVSEEK_FORCE as c_int);
        let seek_from = match base_whence {
            0 => {
                if offset < 0 {
                    return -1;
                }
                SeekFrom::Start(offset as u64)
            }
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return -1,
        };

        match ctx.stream.seek(seek_from) {
            Ok(new_pos) => new_pos as i64,
            Err(_) => -1,
        }
    }

    fn ff_err_string(code: c_int) -> String {
        let mut buf = [0i8; 128];
        unsafe {
            ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    }

    /// Feature-gated static FFmpeg audio decoder using custom Rust `AVIOContext`
    /// callbacks backed directly by our Rust `AudioCache` / `ProgressiveStream`
    /// (zero C OpenSSL / libavformat HTTP stack dependency for iOS/Android/OHOS).
    pub struct FfmpegDecoder {
        fmt_ctx: *mut ff::AVFormatContext,
        avio_ctx: *mut ff::AVIOContext,
        opaque: *mut AvioOpaque,
        codec_ctx: *mut ff::AVCodecContext,
        swr_ctx: *mut ff::SwrContext,
        packet: *mut ff::AVPacket,
        frame: *mut ff::AVFrame,
        stream_index: c_int,
        time_base: ff::AVRational,
        spec: AudioSpec,
        position_ms: u64,
        eof_sent: bool,
    }

    // SAFETY: All FFmpeg contexts owned by `FfmpegDecoder` are accessed exclusively by
    // `&mut self` on a single owner thread at a time.
    unsafe impl Send for FfmpegDecoder {}

    impl FfmpegDecoder {
        /// Opens a static FFmpeg audio decoder over a custom Rust `Read + Seek` stream.
        pub fn open(stream: Box<dyn ReadSeek>) -> AudioResult<Self> {
            unsafe {
                let avio_buf = ff::av_malloc(AVIO_BUFFER_SIZE) as *mut u8;
                if avio_buf.is_null() {
                    return Err(AudioError::Decode(
                        "av_malloc failed for AVIO buffer".into(),
                    ));
                }

                let opaque = Box::into_raw(Box::new(AvioOpaque { stream }));
                let avio_ctx = ff::avio_alloc_context(
                    avio_buf,
                    AVIO_BUFFER_SIZE as c_int,
                    0,
                    opaque as *mut c_void,
                    Some(avio_read_packet),
                    None,
                    Some(avio_seek),
                );
                if avio_ctx.is_null() {
                    ff::av_free(avio_buf as *mut c_void);
                    drop(Box::from_raw(opaque));
                    return Err(AudioError::Decode("avio_alloc_context failed".into()));
                }

                let mut fmt_ctx = ff::avformat_alloc_context();
                if fmt_ctx.is_null() {
                    let mut avio_ptr = avio_ctx;
                    ff::av_freep(&mut (*avio_ptr).buffer as *mut *mut u8 as *mut c_void);
                    ff::avio_context_free(&mut avio_ptr);
                    drop(Box::from_raw(opaque));
                    return Err(AudioError::Decode("avformat_alloc_context failed".into()));
                }

                (*fmt_ctx).pb = avio_ctx;
                (*fmt_ctx).flags |= ff::AVFMT_FLAG_CUSTOM_IO as c_int;

                let open_ret = ff::avformat_open_input(
                    &mut fmt_ctx,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                );
                if open_ret < 0 {
                    let mut avio_ptr = avio_ctx;
                    if !avio_ptr.is_null() {
                        ff::av_freep(&mut (*avio_ptr).buffer as *mut *mut u8 as *mut c_void);
                        ff::avio_context_free(&mut avio_ptr);
                    }
                    drop(Box::from_raw(opaque));
                    return Err(AudioError::Decode(format!(
                        "avformat_open_input failed: {}",
                        ff_err_string(open_ret)
                    )));
                }

                let mut decoder = Self {
                    fmt_ctx,
                    avio_ctx,
                    opaque,
                    codec_ctx: ptr::null_mut(),
                    swr_ctx: ptr::null_mut(),
                    packet: ptr::null_mut(),
                    frame: ptr::null_mut(),
                    stream_index: -1,
                    time_base: ff::AVRational { num: 1, den: 1000 },
                    spec: AudioSpec::default(),
                    position_ms: 0,
                    eof_sent: false,
                };

                let info_ret = ff::avformat_find_stream_info(decoder.fmt_ctx, ptr::null_mut());
                if info_ret < 0 {
                    return Err(AudioError::Decode(format!(
                        "avformat_find_stream_info failed: {}",
                        ff_err_string(info_ret)
                    )));
                }

                let mut av_codec: *const ff::AVCodec = ptr::null();
                let stream_idx = ff::av_find_best_stream(
                    decoder.fmt_ctx,
                    ff::AVMediaType::AVMEDIA_TYPE_AUDIO,
                    -1,
                    -1,
                    &mut av_codec,
                    0,
                );
                if stream_idx < 0 || av_codec.is_null() {
                    return Err(AudioError::Decode(
                        "no audio stream found by FFmpeg demuxer".into(),
                    ));
                }
                decoder.stream_index = stream_idx;

                let av_stream = *(*decoder.fmt_ctx).streams.add(stream_idx as usize);
                decoder.time_base = (*av_stream).time_base;

                let codec_ctx = ff::avcodec_alloc_context3(av_codec);
                if codec_ctx.is_null() {
                    return Err(AudioError::Decode("avcodec_alloc_context3 failed".into()));
                }
                decoder.codec_ctx = codec_ctx;

                let p_ret = ff::avcodec_parameters_to_context(codec_ctx, (*av_stream).codecpar);
                if p_ret < 0 {
                    return Err(AudioError::Decode(format!(
                        "avcodec_parameters_to_context failed: {}",
                        ff_err_string(p_ret)
                    )));
                }

                let c_ret = ff::avcodec_open2(codec_ctx, av_codec, ptr::null_mut());
                if c_ret < 0 {
                    return Err(AudioError::Decode(format!(
                        "avcodec_open2 failed: {}",
                        ff_err_string(c_ret)
                    )));
                }

                let out_sample_rate = (*codec_ctx).sample_rate.max(8_000);
                let out_channels = (*codec_ctx).ch_layout.nb_channels.clamp(1, 2);

                let mut out_layout: ff::AVChannelLayout = std::mem::zeroed();
                ff::av_channel_layout_default(&mut out_layout, out_channels);

                let mut swr_ctx: *mut ff::SwrContext = ptr::null_mut();
                let swr_ret = ff::swr_alloc_set_opts2(
                    &mut swr_ctx,
                    &out_layout,
                    ff::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                    out_sample_rate,
                    &(*codec_ctx).ch_layout,
                    (*codec_ctx).sample_fmt,
                    out_sample_rate,
                    0,
                    ptr::null_mut(),
                );
                ff::av_channel_layout_uninit(&mut out_layout);
                if swr_ret < 0 || swr_ctx.is_null() {
                    return Err(AudioError::Decode(format!(
                        "swr_alloc_set_opts2 failed: {}",
                        ff_err_string(swr_ret)
                    )));
                }
                decoder.swr_ctx = swr_ctx;

                let swr_init_ret = ff::swr_init(swr_ctx);
                if swr_init_ret < 0 {
                    return Err(AudioError::Decode(format!(
                        "swr_init failed: {}",
                        ff_err_string(swr_init_ret)
                    )));
                }

                decoder.packet = ff::av_packet_alloc();
                decoder.frame = ff::av_frame_alloc();
                if decoder.packet.is_null() || decoder.frame.is_null() {
                    return Err(AudioError::Decode(
                        "failed to allocate AVPacket or AVFrame".into(),
                    ));
                }

                let duration_ms = if (*decoder.fmt_ctx).duration > 0 {
                    Some(((*decoder.fmt_ctx).duration as u64) / 1000)
                } else if (*av_stream).duration > 0 {
                    let ms = ff::av_rescale_q(
                        (*av_stream).duration,
                        (*av_stream).time_base,
                        ff::AVRational { num: 1, den: 1000 },
                    );
                    if ms > 0 { Some(ms as u64) } else { None }
                } else {
                    None
                };

                let codec_name = if !(*av_codec).name.is_null() {
                    CStr::from_ptr((*av_codec).name)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    "ffmpeg".to_owned()
                };

                let bit_rate = if (*codec_ctx).bit_rate > 0 {
                    Some((*codec_ctx).bit_rate as u32)
                } else {
                    None
                };

                decoder.spec = AudioSpec {
                    sample_rate: out_sample_rate as u32,
                    channels: out_channels as u16,
                    duration_ms,
                    bit_rate,
                    codec_name,
                };

                Ok(decoder)
            }
        }
    }

    impl AudioDecoder for FfmpegDecoder {
        fn spec(&self) -> AudioSpec {
            self.spec.clone()
        }

        fn read_pcm(&mut self, out: &mut Vec<f32>) -> AudioResult<usize> {
            unsafe {
                loop {
                    let recv = ff::avcodec_receive_frame(self.codec_ctx, self.frame);
                    if recv == 0 {
                        let nb_samples = (*self.frame).nb_samples;
                        if nb_samples > 0 {
                            let ch = self.spec.channels as usize;
                            let max_out_frames =
                                ff::swr_get_out_samples(self.swr_ctx, nb_samples).max(nb_samples);
                            let start_len = out.len();
                            let out_count = max_out_frames as usize * ch;
                            out.resize(start_len + out_count, 0.0);
                            let mut out_ptr = out[start_len..].as_mut_ptr() as *mut u8;
                            let converted = ff::swr_convert(
                                self.swr_ctx,
                                &mut out_ptr,
                                max_out_frames,
                                (*self.frame).extended_data as *mut *const u8,
                                nb_samples,
                            );
                            if converted < 0 {
                                out.truncate(start_len);
                                ff::av_frame_unref(self.frame);
                                return Err(AudioError::Decode(format!(
                                    "swr_convert failed: {}",
                                    ff_err_string(converted)
                                )));
                            }
                            let written_samples = converted as usize * ch;
                            out.truncate(start_len + written_samples);

                            let pts = (*self.frame).best_effort_timestamp;
                            if pts != ff::AV_NOPTS_VALUE {
                                let ms = ff::av_rescale_q(
                                    pts,
                                    self.time_base,
                                    ff::AVRational { num: 1, den: 1000 },
                                );
                                if ms >= 0 {
                                    self.position_ms = ms as u64;
                                }
                            }
                            ff::av_frame_unref(self.frame);
                            return Ok(written_samples);
                        }
                        ff::av_frame_unref(self.frame);
                        continue;
                    } else if recv == ff::AVERROR_EOF {
                        return Ok(0);
                    } else {
                        // EAGAIN (35 on Darwin/BSDs, 11 on Linux/OHOS/Windows) asks for the next packet.
                        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
                        let is_eagain = recv == ff::AVERROR(35) || recv == ff::AVERROR(11);
                        #[cfg(not(any(
                            target_os = "macos",
                            target_os = "ios",
                            target_os = "freebsd"
                        )))]
                        let is_eagain = recv == ff::AVERROR(11);
                        if !is_eagain {
                            return Err(AudioError::Decode(format!(
                                "avcodec_receive_frame failed: {}",
                                ff_err_string(recv)
                            )));
                        }
                    }

                    if self.eof_sent {
                        return Ok(0);
                    }

                    let read_ret = ff::av_read_frame(self.fmt_ctx, self.packet);
                    if read_ret < 0 {
                        self.eof_sent = true;
                        let _ = ff::avcodec_send_packet(self.codec_ctx, ptr::null());
                        continue;
                    }

                    if (*self.packet).stream_index != self.stream_index {
                        ff::av_packet_unref(self.packet);
                        continue;
                    }

                    let send_ret = ff::avcodec_send_packet(self.codec_ctx, self.packet);
                    ff::av_packet_unref(self.packet);
                    if send_ret < 0 && send_ret != ff::AVERROR_EOF {
                        log::debug!("ffmpeg send_packet warning: {}", ff_err_string(send_ret));
                    }
                }
            }
        }

        fn seek(&mut self, position_ms: u64) -> AudioResult<u64> {
            unsafe {
                let ts = ff::av_rescale_q(
                    position_ms as i64,
                    ff::AVRational { num: 1, den: 1000 },
                    self.time_base,
                );
                let ret = ff::av_seek_frame(
                    self.fmt_ctx,
                    self.stream_index,
                    ts,
                    ff::AVSEEK_FLAG_BACKWARD as c_int,
                );
                if ret < 0 {
                    return Err(AudioError::Seek(format!(
                        "av_seek_frame failed: {}",
                        ff_err_string(ret)
                    )));
                }
                ff::avcodec_flush_buffers(self.codec_ctx);
                self.eof_sent = false;
                self.position_ms = position_ms;
                Ok(position_ms)
            }
        }

        fn position_ms(&self) -> u64 {
            self.position_ms
        }

        fn backend_name(&self) -> &'static str {
            "ffmpeg"
        }
    }

    impl Drop for FfmpegDecoder {
        fn drop(&mut self) {
            unsafe {
                if !self.swr_ctx.is_null() {
                    ff::swr_free(&mut self.swr_ctx);
                }
                if !self.frame.is_null() {
                    ff::av_frame_free(&mut self.frame);
                }
                if !self.packet.is_null() {
                    ff::av_packet_free(&mut self.packet);
                }
                if !self.codec_ctx.is_null() {
                    ff::avcodec_free_context(&mut self.codec_ctx);
                }
                if !self.fmt_ctx.is_null() {
                    // Detach custom AVIO before avformat_close_input so we free it safely below.
                    (*self.fmt_ctx).pb = ptr::null_mut();
                    ff::avformat_close_input(&mut self.fmt_ctx);
                }
                if !self.avio_ctx.is_null() {
                    ff::av_freep(&mut (*self.avio_ctx).buffer as *mut *mut u8 as *mut c_void);
                    ff::avio_context_free(&mut self.avio_ctx);
                }
                if !self.opaque.is_null() {
                    drop(Box::from_raw(self.opaque));
                    self.opaque = ptr::null_mut();
                }
            }
        }
    }
}

#[cfg(feature = "audio-ffmpeg")]
pub use ffmpeg_impl::{FfmpegDecoder, ReadSeek};
