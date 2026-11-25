//! Cross-platform Rust audio engine for `nsh-music`.
//!
//! Supports macOS, Windows, Linux, iOS, Android, and HarmonyOS (Tier-1) with:
//! - Progressive HTTP/HTTPS streaming with redirect-preserving `requestHeaders` and LRU disk cache ([`cache`])
//! - Unified PCM decoding via Symphonia, procedural offline `demo://` synth, and feature-gated static FFmpeg over custom Rust `AVIOContext` ([`decoder`], [`ffmpeg_backend`])
//! - Click-free DSP chain: volume ramp, ReplayGain, fade/crossfade, soft limiter, and 16-band FFT spectrum analyzer ([`dsp`])
//! - Cross-platform audio output via `cpal`, HarmonyOS `OH_AudioRenderer`, and headless `NullOutput` ([`output`])
//! - Thread-safe `Clone` [`AudioEngine`] with queue, shuffle, repeat, radio refill, prefetch, and watch subscriptions ([`player`])
//! - OS media controls abstraction ([`remote`])
//! - In-process [`VirtualFfmpegShim`] intercepting plugin `ffmpeg` CLI probes and loopback HTTP relays ([`virtual_ffmpeg`])

pub mod cache;
pub mod decoder;
pub mod dsp;
pub mod ffmpeg_backend;
pub mod output;
pub mod player;
pub mod remote;
pub mod virtual_ffmpeg;

use thiserror::Error;

/// Unified error type for `nsh-music-audio`.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("audio output error: {0}")]
    Output(String),
    #[error("seek error: {0}")]
    Seek(String),
    #[error("invalid audio input: {0}")]
    InvalidInput(String),
    #[error("virtual ffmpeg shim error: {0}")]
    VirtualFfmpeg(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error("{0}")]
    Other(String),
}

pub type AudioResult<T> = Result<T, AudioError>;

// Convenient top-level re-exports.
pub use cache::{
    AudioCache, ProgressiveStream, RedirectedResponse, StreamCacheConfig,
    send_with_preserved_headers,
};
pub use decoder::{AudioDecoder, AudioSpec, DecoderPreference, open_decoder};
#[cfg(test)]
pub use decoder::DemoSynthDecoder;
#[cfg(feature = "audio-symphonia")]
pub use decoder::SymphoniaDecoder;
pub use dsp::{
    DspConfig, DspProcessor, FadeEnvelope, SPECTRUM_BANDS, SoftLimiter, SpectrumAnalyzer,
    compute_replay_gain_linear, db_to_linear, mix_crossfade,
};
#[cfg(feature = "audio-ffmpeg")]
pub use ffmpeg_backend::{FfmpegDecoder, ReadSeek};
pub use ffmpeg_backend::{
    AudioBackendCapabilities, audio_backend_capabilities, embedded_ffmpeg_version,
};
#[cfg(not(any(target_arch = "wasm32", target_env = "ohos")))]
pub use output::CpalOutput;
#[cfg(target_env = "ohos")]
pub use output::OhosAudioOutput;
pub use output::{AudioOutput, NullOutput, OutputConfig, RenderCallback, default_audio_output};
pub use player::{
    AudioEngine, AudioEngineConfig, PlaybackSnapshot, PlaybackStatus, RadioRefillCallback,
    StateChangeCallback, StreamResolver,
};
pub use remote::{
    DefaultPlatformRemoteDriver, NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend,
    RemoteCommand, RemoteCommandHandler, RemoteController,
};
pub use virtual_ffmpeg::{
    FfmpegInvocation, HttpRelaySpec, VirtualFfmpegShim, VirtualProcessHandle, is_ffmpeg_command,
};

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use nsh_music_core::model::{Artist, RepeatMode, Track};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-music-audio-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_demo_track(id: &str, name: &str, duration_ms: i64) -> Track {
        Track {
            id: id.to_owned(),
            source: "local".to_owned(),
            name: name.to_owned(),
            artists: vec![Artist {
                id: "a1".to_owned(),
                name: "NSH Ensemble".to_owned(),
                ..Artist::default()
            }],
            duration: duration_ms,
            url: format!("demo://{id}?duration={duration_ms}"),
            ..Track::default()
        }
    }

    #[test]
    fn test_backend_capabilities_and_is_ffmpeg_command() {
        let caps = audio_backend_capabilities();
        assert!(caps.virtual_ffmpeg_shim);
        assert_eq!(caps.symphonia_enabled, cfg!(feature = "audio-symphonia"));
        assert_eq!(caps.ffmpeg_enabled, cfg!(feature = "audio-ffmpeg"));
        assert!(!caps.ffmpeg_version.is_empty());
        let json = caps.to_json_pretty().expect("serialize capabilities");
        assert!(json.contains("virtualFfmpegShim"));

        assert!(is_ffmpeg_command("ffmpeg"));
        assert!(is_ffmpeg_command("FFMPEG.EXE"));
        assert!(is_ffmpeg_command("/usr/local/bin/ffmpeg"));
        assert!(is_ffmpeg_command("C:\\tools\\ffmpeg\\bin\\ffmpeg.exe"));
        assert!(!is_ffmpeg_command("ffprobe"));
        assert!(!is_ffmpeg_command("yt-dlp"));
        assert!(!is_ffmpeg_command(""));
    }

    #[test]
    fn test_virtual_ffmpeg_probes() {
        let ver = VirtualFfmpegShim::spawn_str(&["-version"]).expect("spawn -version");
        assert!(!ver.is_running());
        assert_eq!(ver.wait(), 0);
        assert!(ver.stdout().contains("ffmpeg version"));

        let help = VirtualFfmpegShim::spawn_str(&["-hide_banner", "-h", "protocol=http"])
            .expect("spawn protocol=http help");
        assert!(!help.is_running());
        assert_eq!(help.wait(), 0);
        assert!(
            help.stdout().contains("-request_size"),
            "protocol=http help must advertise -request_size"
        );
        assert!(help.stdout().contains("-headers"));
        assert!(help.stdout().contains("-listen"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_virtual_ffmpeg_loopback_http_relay_preserves_headers() {
        // Spin up a mock upstream CDN requiring Referer + User-Agent headers.
        let upstream_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let payload: Vec<u8> = (0..16_384).map(|i| (i % 251) as u8).collect();
        let server_payload = payload.clone();

        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream_listener.accept().await.expect("accept upstream");
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.expect("read request");
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let lower = req.to_ascii_lowercase();
            assert!(
                lower.contains("referer: https://www.bilibili.com"),
                "expected Referer header in upstream request, got:\n{req}"
            );
            assert!(
                lower.contains("user-agent: nsh-test-agent/1.0"),
                "expected User-Agent header in upstream request, got:\n{req}"
            );
            assert!(
                lower.contains("cookie: sessdata=abc123"),
                "expected Cookie header in upstream request, got:\n{req}"
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                server_payload.len()
            );
            socket.write_all(header.as_bytes()).await.expect("write header");
            socket.write_all(&server_payload).await.expect("write body");
            let _ = socket.shutdown().await;
        });

        // Reserve a free loopback port by binding & dropping a temporary listener.
        let free_port = {
            let tmp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
            tmp.local_addr().expect("local addr").port()
        };
        let relay_url = format!("http://127.0.0.1:{free_port}/");
        let upstream_url = format!("http://{upstream_addr}/audio.m4s");

        let proc = VirtualFfmpegShim::spawn_str(&[
            "-headers",
            "Referer: https://www.bilibili.com\r\nCookie: SESSDATA=abc123\r\n",
            "-user_agent",
            "nsh-test-agent/1.0",
            "-reconnect",
            "1",
            "-i",
            &upstream_url,
            "-vn",
            "-c:a",
            "copy",
            "-f",
            "mp4",
            "-listen",
            "1",
            &relay_url,
        ])
        .expect("spawn relay");

        assert!(proc.is_running());
        assert!(
            proc.error_output().contains("Input #0"),
            "plugins poll errorOutput().includes('Input #0'), got: {}",
            proc.error_output()
        );
        assert_eq!(proc.listen_addr().map(|a| a.port()), Some(free_port));

        // Fetch relayed audio from the virtual FFmpeg loopback HTTP server.
        let resp = reqwest::get(&relay_url).await.expect("GET loopback relay");
        assert!(resp.status().is_success());
        let received = resp.bytes().await.expect("read relayed body");
        assert_eq!(received.as_ref(), payload.as_slice());

        proc.kill();
        assert!(!proc.is_running());
        assert_eq!(proc.wait(), 0);
        upstream_task.await.expect("upstream task ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cache_preserves_headers_across_302_redirect_and_lru_eviction() {
        // Mock CDN target server verifying custom headers survived the 302 redirect.
        let cdn_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind cdn");
        let cdn_addr = cdn_listener.local_addr().expect("cdn addr");
        let cdn_body: Vec<u8> = (0..8192).map(|i| ((i * 7) % 256) as u8).collect();
        let cdn_body_srv = cdn_body.clone();

        let cdn_task = tokio::spawn(async move {
            // Accept initial GET and optional Range seek request.
            for _ in 0..2 {
                let Ok((mut socket, _)) = cdn_listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let lower = req.to_ascii_lowercase();
                assert!(
                    lower.contains("referer: https://music.example.test/"),
                    "Referer lost across redirect: {req}"
                );
                assert!(
                    lower.contains("cookie: token=xyz"),
                    "Cookie lost across redirect: {req}"
                );
                assert!(
                    lower.contains("user-agent: nsh-music-agent"),
                    "User-Agent lost across redirect: {req}"
                );
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    cdn_body_srv.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&cdn_body_srv).await;
                let _ = socket.shutdown().await;
                break;
            }
        });

        // Mock API server issuing HTTP 302 redirect to the CDN server on a different port.
        let api_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind api");
        let api_addr = api_listener.local_addr().expect("api addr");
        let api_task = tokio::spawn(async move {
            let (mut socket, _) = api_listener.accept().await.expect("accept api");
            let mut buf = vec![0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{cdn_addr}/track.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(resp.as_bytes()).await.expect("write 302");
            let _ = socket.shutdown().await;
        });

        let dir = unique_temp_dir("cache");
        let cache = AudioCache::with_dir(dir.clone(), 20_000).expect("create cache");
        let headers = vec![
            ("Referer".to_owned(), "https://music.example.test/".to_owned()),
            ("Cookie".to_owned(), "token=xyz".to_owned()),
            ("User-Agent".to_owned(), "nsh-music-agent".to_owned()),
        ];

        let url = format!("http://{api_addr}/resolve");
        let mut stream = cache
            .open_progressive(&url, headers)
            .expect("open progressive stream");
        assert_eq!(stream.byte_len(), Some(cdn_body.len() as u64));

        let mut all = Vec::new();
        stream.read_to_end(&mut all).expect("read progressive stream");
        assert_eq!(all, cdn_body);

        // Seek backwards inside the progressive file.
        stream.seek(SeekFrom::Start(1234)).expect("seek start");
        let mut slice = [0u8; 32];
        stream.read_exact(&mut slice).expect("read after seek");
        assert_eq!(&slice, &cdn_body[1234..1266]);

        api_task.await.expect("api task");
        cdn_task.await.expect("cdn task");

        // Verify LRU byte eviction: insert entries totaling > 20_000 bytes.
        cache.put_bytes("https://lru.test/1", &vec![1u8; 9_000]).unwrap();
        cache.put_bytes("https://lru.test/2", &vec![2u8; 9_000]).unwrap();
        // Touch entry 1 so entry 2 becomes least recently used.
        assert!(cache.get_cached_path("https://lru.test/1").is_some());
        cache.put_bytes("https://lru.test/3", &vec![3u8; 9_000]).unwrap();
        assert!(cache.total_bytes() <= 20_000);
        assert!(cache.contains("https://lru.test/1"));
        assert!(cache.contains("https://lru.test/3"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dsp_volume_replaygain_limiter_and_spectrum() {
        let mut dsp = DspProcessor::new(DspConfig {
            sample_rate: 44_100,
            channels: 2,
            volume: 1.0,
            muted: false,
            replay_gain_db: 6.0,
            replay_gain_peak: 0.5,
            replay_gain_enabled: true,
            ramp_ms: 5.0,
            limiter_threshold: 0.85,
            limiter_ceiling: 0.985,
        });

        // Feed a 1 kHz stereo sine wave and verify soft limiter keeps peak < 1.0
        // and spectrum analyzer detects strong mid-band energy.
        let frames = 2048;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / 44_100.0;
            let s = (TAU * 1000.0 * t).sin() * 0.95;
            samples.push(s);
            samples.push(s);
        }
        dsp.process_interleaved(&mut samples);

        let max_abs = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(max_abs <= 0.99, "soft limiter should prevent clipping, peak={max_abs}");

        let bands = dsp.spectrum();
        let max_band = bands.iter().copied().fold(0.0f32, f32::max);
        assert!(
            max_band > 0.3,
            "1 kHz sine should produce measurable spectrum energy, bands={bands:?}"
        );
        // Peak bin for 1 kHz lies in the middle bands, not the very highest band.
        let peak_idx = bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            (4..=12).contains(&peak_idx),
            "expected 1 kHz peak in mid bands, got index {peak_idx} ({bands:?})"
        );

        // Equal-power crossfade sanity check.
        let a = [1.0f32; 4];
        let b = [0.0f32; 4];
        let mut out = [0.0f32; 4];
        mix_crossfade(&a, &b, &mut out, 0.5);
        assert!((out[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01);
    }

    #[test]
    fn test_demo_synth_decoder_and_seek() {
        let mut dec = DemoSynthDecoder::from_uri("demo://midnight-vinyl?duration=5000", None);
        let spec = dec.spec();
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.duration_ms, Some(5_000));

        let mut buf = Vec::new();
        let n = dec.read_pcm(&mut buf).expect("read demo pcm");
        assert!(n > 0);
        let energy: f32 = buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32;
        assert!(energy > 1e-4, "demo synth should emit real audible signal");

        let landed = dec.seek(2_500).expect("seek demo synth");
        assert_eq!(landed, 2_500);
        assert_eq!(dec.position_ms(), 2_500);
    }

    #[test]
    fn test_audio_engine_null_output_queue_seek_repeat_radio_and_remote() {
        let dir = unique_temp_dir("engine");
        let null_out = NullOutput::with_tick_interval(Duration::from_millis(2));
        let frames_counter = null_out.frames_rendered_counter();

        let engine = AudioEngine::with_output(
            AudioEngineConfig {
                cache: StreamCacheConfig {
                    cache_dir: dir.clone(),
                    max_bytes: 10 * 1024 * 1024,
                    ..StreamCacheConfig::default()
                },
                prefetch_next: false,
                ..AudioEngineConfig::default()
            },
            Box::new(null_out),
        )
        .expect("create audio engine");

        let t1 = make_demo_track("demo-1", "Midnight Lounge", 30_000);
        let t2 = make_demo_track("demo-2", "Amber Drift", 25_000);
        let radio_calls = Arc::new(AtomicUsize::new(0));
        let radio_calls_cb = Arc::clone(&radio_calls);
        engine.set_radio_refill(Some(Arc::new(move |_| {
            let n = radio_calls_cb.fetch_add(1, Ordering::SeqCst);
            vec![make_demo_track(
                &format!("radio-{n}"),
                "Endless FM",
                20_000,
            )]
        })));

        engine.play_queue(vec![t1.clone(), t2.clone()], 0);

        // Wait briefly for engine thread to begin Playing, render audio frames, and publish spectrum.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let snap = engine.snapshot();
            if snap.status == PlaybackStatus::Playing
                && frames_counter.load(Ordering::Relaxed) > 1000
                && engine.spectrum().iter().sum::<f32>() > 0.0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let snap = engine.snapshot();
        assert_eq!(snap.status, PlaybackStatus::Playing);
        assert_eq!(snap.current_track.as_ref().map(|t| t.id.as_str()), Some("demo-1"));
        assert_eq!(snap.output_backend, "null");
        assert_eq!(snap.decoder_backend, "demo-synth");
        assert!(frames_counter.load(Ordering::Relaxed) > 0);
        let spec_energy: f32 = engine.spectrum().iter().sum();
        assert!(spec_energy > 0.0, "spectrum should be active while playing demo track");

        // Test seek.
        engine.seek(12_000);
        let seek_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < seek_deadline {
            if engine.snapshot().position_ms >= 11_500 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(engine.snapshot().position_ms >= 11_500);

        // Test RemoteController dispatch -> Pause & Next!
        engine.remote_controller().dispatch(RemoteCommand::Pause);
        let pause_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < pause_deadline {
            if engine.snapshot().status == PlaybackStatus::Paused {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(engine.snapshot().status, PlaybackStatus::Paused);

        // Advance to track 2 via OS RemoteCommand::Next.
        engine.remote_controller().dispatch(RemoteCommand::Next);
        let next_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < next_deadline {
            if engine
                .snapshot()
                .current_track
                .as_ref()
                .map(|t| t.id.as_str())
                == Some("demo-2")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            engine
                .snapshot()
                .current_track
                .as_ref()
                .map(|t| t.id.as_str()),
            Some("demo-2")
        );

        // Because demo-2 was the last track and RepeatMode::Off, radio refill should have appended a track!
        let refill_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < refill_deadline {
            if engine.snapshot().queue.len() >= 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(engine.snapshot().queue.len() >= 3);
        assert!(radio_calls.load(Ordering::SeqCst) >= 1);

        // Test repeat cycling and volume/mute.
        engine.set_repeat(RepeatMode::All);
        engine.set_volume(0.5);
        engine.set_muted(true);
        let state_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < state_deadline {
            let s = engine.snapshot();
            if s.repeat == RepeatMode::All && s.muted && (s.volume - 0.5).abs() < 0.01 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let final_snap = engine.snapshot();
        assert_eq!(final_snap.repeat, RepeatMode::All);
        assert!(final_snap.muted);
        assert!((final_snap.volume - 0.5).abs() < 0.01);

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "audio-symphonia")]
    #[test]
    fn test_symphonia_decoder_wav_decode_and_seek() {
        // Synthesize a valid 1-second 44.1kHz stereo 16-bit PCM WAV file in memory.
        let sample_rate: u32 = 44_100;
        let channels: u16 = 2;
        let num_frames: u32 = sample_rate;
        let data_bytes = num_frames * channels as u32 * 2;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = ((TAU * 440.0 * t).sin() * 16_000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
            wav.extend_from_slice(&sample.to_le_bytes());
        }

        let dir = unique_temp_dir("symphonia-wav");
        let wav_path = dir.join("tone.wav");
        std::fs::write(&wav_path, &wav).expect("write wav");
        let cache = AudioCache::with_dir(dir.join("cache"), 1024 * 1024).expect("cache");
        let url = format!("file://{}", wav_path.display());

        let mut dec = open_decoder(
            &cache,
            &url,
            Vec::new(),
            Some("audio/wav"),
            None,
            DecoderPreference::Auto,
        )
        .expect("open wav via symphonia");
        assert_eq!(dec.backend_name(), "symphonia");
        assert_eq!(dec.spec().sample_rate, 44_100);
        assert_eq!(dec.spec().channels, 2);

        let mut pcm = Vec::new();
        let n = dec.read_pcm(&mut pcm).expect("read wav pcm");
        assert!(n > 0);
        let peak = pcm.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak > 0.3, "expected 440 Hz tone energy, peak={peak}");

        let landed = dec.seek(500).expect("seek wav to 500ms");
        assert!((450..=550).contains(&landed), "landed={landed}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
