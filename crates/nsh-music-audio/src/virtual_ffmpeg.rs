use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use reqwest::Method;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use url::Url;

use crate::cache::{
    MAX_REDIRECTS, RedirectedResponse, build_loopback_no_redirect_client, build_no_redirect_client,
    io_runtime_handle, is_loopback_url, send_with_preserved_headers,
};
use crate::{AudioError, AudioResult};

static NEXT_VIRTUAL_PID: AtomicU32 = AtomicU32::new(70_000);

/// Returns `true` if `cmd` refers to an `ffmpeg` executable (e.g. `"ffmpeg"`,
/// `"ffmpeg.exe"`, `"/usr/bin/ffmpeg"`, `"/opt/homebrew/bin/ffmpeg"`,
/// `"C:\\ffmpeg\\bin\\ffmpeg.exe"`).
pub fn is_ffmpeg_command(cmd: &str) -> bool {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return false;
    }
    let file_name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("ffmpeg")
}

/// Parsed HTTP loopback relay/remux specification extracted from FFmpeg CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRelaySpec {
    pub input_url: String,
    pub headers: Vec<(String, String)>,
    pub reconnect: bool,
    pub request_size: Option<u64>,
    pub format: Option<String>,
    pub audio_codec: Option<String>,
    pub disable_video: bool,
    pub listen: bool,
    pub listen_host: String,
    pub listen_port: u16,
    pub output_url: String,
}

/// Classified FFmpeg CLI invocation parsed by [`VirtualFfmpegShim`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfmpegInvocation {
    ProbeVersion,
    ProbeHelp {
        topic: String,
    },
    ProbeProtocols,
    ProbeFormats,
    HttpRelay(HttpRelaySpec),
    StdoutPipe {
        input_url: String,
        headers: Vec<(String, String)>,
        format: Option<String>,
        reconnect: bool,
    },
    Unsupported {
        args: Vec<String>,
    },
}

struct VirtualProcessState {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

struct VirtualProcessInner {
    pid: u32,
    listen_addr: Option<SocketAddr>,
    running: AtomicBool,
    state: Mutex<VirtualProcessState>,
    done_cvar: Condvar,
    kill_notify: Arc<Notify>,
}

/// Process handle returned by [`VirtualFfmpegShim::spawn`] mirroring the `mygo-music`
/// spawned process API (`stdout()`, `stderr()`, `error_output()`, `wait()`, `kill()`, `is_running()`).
#[derive(Clone)]
pub struct VirtualProcessHandle {
    inner: Arc<VirtualProcessInner>,
}

impl std::fmt::Debug for VirtualProcessHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualProcessHandle")
            .field("pid", &self.inner.pid)
            .field("listen_addr", &self.inner.listen_addr)
            .field("is_running", &self.is_running())
            .field("exit_code", &self.exit_code())
            .finish()
    }
}

impl VirtualProcessHandle {
    fn completed(stdout: String, stderr: String, exit_code: i32) -> Self {
        Self {
            inner: Arc::new(VirtualProcessInner {
                pid: NEXT_VIRTUAL_PID.fetch_add(1, Ordering::Relaxed),
                listen_addr: None,
                running: AtomicBool::new(false),
                state: Mutex::new(VirtualProcessState {
                    stdout,
                    stderr,
                    exit_code: Some(exit_code),
                }),
                done_cvar: Condvar::new(),
                kill_notify: Arc::new(Notify::new()),
            }),
        }
    }

    fn running(listen_addr: Option<SocketAddr>, initial_stderr: String) -> Self {
        Self {
            inner: Arc::new(VirtualProcessInner {
                pid: NEXT_VIRTUAL_PID.fetch_add(1, Ordering::Relaxed),
                listen_addr,
                running: AtomicBool::new(true),
                state: Mutex::new(VirtualProcessState {
                    stdout: String::new(),
                    stderr: initial_stderr,
                    exit_code: None,
                }),
                done_cvar: Condvar::new(),
                kill_notify: Arc::new(Notify::new()),
            }),
        }
    }

    fn append_stdout(&self, chunk: &str) {
        if let Ok(mut guard) = self.inner.state.lock() {
            guard.stdout.push_str(chunk);
        }
    }

    fn append_stderr(&self, chunk: &str) {
        const MAX_STDERR_BYTES: usize = 64 * 1024;
        if let Ok(mut guard) = self.inner.state.lock() {
            if guard.stderr.len() + chunk.len() > MAX_STDERR_BYTES {
                // Preserve the initial `Input #0` header block and trim older progress lines.
                let keep_prefix = guard.stderr.len().min(1024);
                let tail_keep =
                    (MAX_STDERR_BYTES / 2).min(guard.stderr.len().saturating_sub(keep_prefix));
                let tail_start = guard.stderr.len().saturating_sub(tail_keep);
                if tail_start > keep_prefix
                    && guard.stderr.is_char_boundary(keep_prefix)
                    && guard.stderr.is_char_boundary(tail_start)
                {
                    let mut compacted = String::with_capacity(MAX_STDERR_BYTES);
                    compacted.push_str(&guard.stderr[..keep_prefix]);
                    compacted.push_str("\n...\n");
                    compacted.push_str(&guard.stderr[tail_start..]);
                    guard.stderr = compacted;
                }
            }
            guard.stderr.push_str(chunk);
        }
    }

    fn finish(&self, code: i32) {
        self.inner.running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.state.lock() {
            if guard.exit_code.is_none() {
                guard.exit_code = Some(code);
            }
            self.inner.done_cvar.notify_all();
        }
    }

    /// Returns captured virtual stdout text.
    pub fn stdout(&self) -> String {
        self.inner
            .state
            .lock()
            .map(|s| s.stdout.clone())
            .unwrap_or_default()
    }

    /// Returns captured virtual stderr text.
    pub fn stderr(&self) -> String {
        self.inner
            .state
            .lock()
            .map(|s| s.stderr.clone())
            .unwrap_or_default()
    }

    /// Alias for [`stderr`](Self::stderr), matching `mygo-music` plugins calling
    /// `proc.errorOutput().includes("Input #0")`.
    pub fn error_output(&self) -> String {
        self.stderr()
    }

    /// Returns whether the virtual process / loopback relay is still active.
    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
    }

    /// Returns the exit code if the process has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.inner.state.lock().ok().and_then(|s| s.exit_code)
    }

    /// Synthetic process identifier for diagnostics.
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Bound loopback socket address when running in `-listen 1` HTTP relay mode.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.inner.listen_addr
    }

    /// Blocks until the virtual process exits (or is killed) and returns its exit code.
    pub fn wait(&self) -> i32 {
        let Ok(mut guard) = self.inner.state.lock() else {
            return -1;
        };
        while guard.exit_code.is_none() {
            match self.inner.done_cvar.wait(guard) {
                Ok(next) => guard = next,
                Err(_) => return -1,
            }
        }
        guard.exit_code.unwrap_or(0)
    }

    /// Terminates the running loopback relay or stream task cleanly.
    pub fn kill(&self) {
        self.inner.kill_notify.notify_waiters();
        self.finish(0);
    }
}

/// In-process virtual FFmpeg shim intercepting `ffmpeg` CLI invocations from
/// `mygo-music` plugins (`youtube-music`, `bilibili`, etc.) on desktop AND mobile
/// (iOS, Android, HarmonyOS) without requiring an external `ffmpeg` binary on `PATH`.
pub struct VirtualFfmpegShim;

impl VirtualFfmpegShim {
    /// Returns `true` if `cmd` targets an `ffmpeg` executable.
    pub fn is_ffmpeg_command(cmd: &str) -> bool {
        is_ffmpeg_command(cmd)
    }

    /// Parses FFmpeg CLI arguments into a structured [`FfmpegInvocation`].
    pub fn parse_args(args: &[String]) -> FfmpegInvocation {
        if args.is_empty() {
            return FfmpegInvocation::Unsupported {
                args: args.to_vec(),
            };
        }

        let mut i = 0usize;
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut input_url: Option<String> = None;
        let mut output_target: Option<String> = None;
        let mut format: Option<String> = None;
        let mut audio_codec: Option<String> = None;
        let mut disable_video = false;
        let mut listen = false;
        let mut reconnect = false;
        let mut request_size: Option<u64> = None;

        while i < args.len() {
            let arg = args[i].as_str();
            match arg {
                "-version" | "--version" => return FfmpegInvocation::ProbeVersion,
                "-protocols" => return FfmpegInvocation::ProbeProtocols,
                "-formats" => return FfmpegInvocation::ProbeFormats,
                "-h" | "-help" | "--help" => {
                    let topic = args.get(i + 1).cloned().unwrap_or_default();
                    return FfmpegInvocation::ProbeHelp { topic };
                }
                "-hide_banner" | "-nostdin" | "-nostats" | "-y" | "-n" => {
                    i += 1;
                }
                "-vn" => {
                    disable_video = true;
                    i += 1;
                }
                "-headers" => {
                    if let Some(raw) = args.get(i + 1) {
                        parse_ffmpeg_headers_block(raw, &mut headers);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-user_agent" => {
                    if let Some(ua) = args.get(i + 1) {
                        upsert_header(&mut headers, "User-Agent", ua);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-referer" => {
                    if let Some(r) = args.get(i + 1) {
                        upsert_header(&mut headers, "Referer", r);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-cookies" => {
                    if let Some(c) = args.get(i + 1) {
                        upsert_header(&mut headers, "Cookie", c);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-reconnect" | "-reconnect_streamed" | "-reconnect_at_eof" => {
                    if let Some(val) = args.get(i + 1) {
                        if val == "1" || val.eq_ignore_ascii_case("true") {
                            reconnect = true;
                        }
                        i += 2;
                    } else {
                        reconnect = true;
                        i += 1;
                    }
                }
                "-request_size" => {
                    if let Some(val) = args.get(i + 1) {
                        request_size = val.parse::<u64>().ok();
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-listen" => {
                    if let Some(val) = args.get(i + 1) {
                        listen = val == "1" || val == "2" || val.eq_ignore_ascii_case("true");
                        i += 2;
                    } else {
                        listen = true;
                        i += 1;
                    }
                }
                "-i" => {
                    if let Some(url) = args.get(i + 1) {
                        input_url = Some(url.clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-f" => {
                    if let Some(fmt) = args.get(i + 1) {
                        format = Some(fmt.clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-c:a" | "-acodec" | "-c" => {
                    if let Some(codec) = args.get(i + 1) {
                        audio_codec = Some(codec.clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                // Consume common key-value flags whose values are not the output URL.
                "-loglevel"
                | "-v"
                | "-reconnect_delay_max"
                | "-reconnect_on_network_error"
                | "-reconnect_on_http_error"
                | "-rw_timeout"
                | "-timeout"
                | "-seekable"
                | "-ss"
                | "-t"
                | "-to"
                | "-movflags"
                | "-map"
                | "-b:a"
                | "-ar"
                | "-ac"
                | "-q:a"
                | "-qscale:a"
                | "-id3v2_version"
                | "-write_xing"
                | "-write_id3v1"
                | "-compression_level"
                | "-metadata"
                | "-af"
                | "-filter:a"
                | "-threads"
                | "-strict"
                | "-max_muxing_queue_size"
                | "-avoid_negative_ts"
                | "-content_type"
                | "-probesize"
                | "-analyzeduration"
                | "-fflags"
                | "-multiple_requests"
                | "-http_persistent"
                | "-icy" => {
                    i += 2;
                }
                other => {
                    if let Some(topic) = other.strip_prefix("-h=") {
                        return FfmpegInvocation::ProbeHelp {
                            topic: topic.to_owned(),
                        };
                    }
                    if !other.starts_with('-') || other == "-" {
                        output_target = Some(other.to_owned());
                    }
                    i += 1;
                }
            }
        }

        if let (Some(input), Some(output)) = (input_url, output_target) {
            if output == "-" || output.starts_with("pipe:") {
                return FfmpegInvocation::StdoutPipe {
                    input_url: input,
                    headers,
                    format,
                    reconnect,
                };
            }
            if let Ok(parsed) = Url::parse(&output)
                && parsed.scheme() == "http"
            {
                let host = parsed.host_str().unwrap_or("127.0.0.1").to_owned();
                let is_loopback = host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost");
                if listen || is_loopback {
                    let port = parsed.port().unwrap_or(0);
                    return FfmpegInvocation::HttpRelay(HttpRelaySpec {
                        input_url: input,
                        headers,
                        reconnect,
                        request_size,
                        format,
                        audio_codec,
                        disable_video,
                        listen: true,
                        listen_host: if host.eq_ignore_ascii_case("localhost") {
                            "127.0.0.1".to_owned()
                        } else {
                            host
                        },
                        listen_port: port,
                        output_url: output,
                    });
                }
            }
        }

        FfmpegInvocation::Unsupported {
            args: args.to_vec(),
        }
    }

    /// Convenience helper accepting string slices.
    pub fn spawn_str<S: AsRef<str>>(args: &[S]) -> AudioResult<VirtualProcessHandle> {
        let owned: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
        Self::spawn(&owned)
    }

    /// Executes an intercepted `ffmpeg` invocation in-process and returns a [`VirtualProcessHandle`].
    pub fn spawn(args: &[String]) -> AudioResult<VirtualProcessHandle> {
        match Self::parse_args(args) {
            FfmpegInvocation::ProbeVersion => Ok(VirtualProcessHandle::completed(
                format!(
                    "ffmpeg version 7.1-nsh-embedded Copyright (c) 2000-2024 the FFmpeg developers\n\
                     built with nsh-music VirtualFfmpegShim ({})\n\
                     configuration: --enable-static --enable-protocol=http,https --disable-programs\n\
                     libavutil      59. 39.100 / 59. 39.100\n\
                     libavcodec     61. 19.100 / 61. 19.100\n\
                     libavformat    61.  7.100 / 61.  7.100\n\
                     libswresample   5.  3.100 /  5.  3.100\n",
                    crate::ffmpeg_backend::embedded_ffmpeg_version()
                ),
                String::new(),
                0,
            )),
            FfmpegInvocation::ProbeHelp { topic } => {
                let lower = topic.to_ascii_lowercase();
                let out = if lower.contains("http") {
                    "http AVOptions:\n\
                       -seekable          <boolean>    .D......... control seekability of connection (default auto)\n\
                       -headers           <string>     .D......... set custom HTTP headers, can override built in default headers\n\
                       -user_agent        <string>     .D......... override User-Agent header\n\
                       -reconnect         <boolean>    .D......... auto reconnect after disconnect before EOF (default false)\n\
                       -reconnect_streamed <boolean>   .D......... auto reconnect streamed / non seekable streams (default false)\n\
                       -reconnect_delay_max <int>      .D......... max reconnect delay in seconds (default 120)\n\
                       -listen            <int>        .D......... listen on HTTP (from 0 to 2) (default 0)\n\
                       -request_size      <int>        .D......... maximum HTTP range request size in bytes (default 0)\n"
                        .to_owned()
                } else {
                    format!("nsh-music VirtualFfmpegShim help for '{topic}': supported\n")
                };
                Ok(VirtualProcessHandle::completed(out, String::new(), 0))
            }
            FfmpegInvocation::ProbeProtocols => Ok(VirtualProcessHandle::completed(
                "Supported file protocols:\nInput:\n  file\n  http\n  https\n  pipe\nOutput:\n  http\n  pipe\n"
                    .to_owned(),
                String::new(),
                0,
            )),
            FfmpegInvocation::ProbeFormats => Ok(VirtualProcessHandle::completed(
                "File formats:\n DE mp3             MP2/3 (MPEG audio layer 3)\n DE mp4             MP4 (MPEG-4 Part 14)\n DE flac            raw FLAC\n DE ogg             Ogg\n DE webm            WebM\n DE matroska        Matroska\n DE adts            ADTS AAC\n DE mpegts          MPEG-TS\n"
                    .to_owned(),
                String::new(),
                0,
            )),
            FfmpegInvocation::HttpRelay(spec) => spawn_http_relay(spec),
            FfmpegInvocation::StdoutPipe {
                input_url,
                headers,
                format,
                reconnect,
            } => spawn_stdout_pipe(input_url, headers, format, reconnect),
            FfmpegInvocation::Unsupported { args } => Ok(VirtualProcessHandle::completed(
                String::new(),
                format!(
                    "nsh-music VirtualFfmpegShim: unsupported ffmpeg invocation: {}\n",
                    args.join(" ")
                ),
                1,
            )),
        }
    }
}

fn parse_ffmpeg_headers_block(raw: &str, out: &mut Vec<(String, String)>) {
    // Support both real CRLF/LF newlines and escaped `\r\n` literals from JS plugins.
    let normalized = raw.replace("\\r\\n", "\n").replace("\\n", "\n");
    for line in normalized.split(['\r', '\n']) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            upsert_header(out, name.trim(), value.trim());
        }
    }
}

fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if name.is_empty() {
        return;
    }
    if let Some(existing) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        existing.1 = value.to_owned();
    } else {
        headers.push((name.to_owned(), value.to_owned()));
    }
}

fn content_type_for_format(fmt: Option<&str>, upstream_ct: Option<&str>) -> String {
    if let Some(ct) = upstream_ct
        && !ct.is_empty()
        && !ct.eq_ignore_ascii_case("application/octet-stream")
    {
        return ct.to_owned();
    }
    match fmt.unwrap_or("").to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "mp4" | "m4a" | "ipod" | "ismv" => "audio/mp4",
        "adts" | "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "webm" => "audio/webm",
        "matroska" | "mka" => "audio/x-matroska",
        "wav" => "audio/wav",
        "mpegts" => "video/mp2t",
        _ => upstream_ct.unwrap_or("application/octet-stream"),
    }
    .to_owned()
}

fn sanitize_relay_url(raw: &str) -> String {
    if let Ok(parsed) = Url::parse(raw) {
        let scheme = parsed.scheme();
        let host = parsed.host_str().unwrap_or("<unknown>");
        return match parsed.port() {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
        };
    }
    "<invalid-url>".to_owned()
}

fn is_rejected_non_audio_mime(ct: Option<&str>) -> Option<String> {
    let ct = ct?;
    let base = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    if base.starts_with("text/html")
        || base.starts_with("application/xhtml")
        || base.starts_with("application/json")
    {
        Some(base)
    } else {
        None
    }
}

enum PrefetchedUpstream {
    Hls {
        playlist_url: Url,
        playlist_text: String,
    },
    Stream {
        upstream: Box<RedirectedResponse>,
        chunked_range_mode: bool,
        chunked_req_size: Option<u64>,
        chunked_total_size: Option<u64>,
        first_chunk: Vec<u8>,
    },
}

/// Shared initial upstream GET helper used by both eager preflight (non-loopback relays)
/// and on-demand local `GET /` connections (loopback relays or subsequent requests),
/// handling `-request_size` chunked Ranges, 416 fallback, HLS detection, and first-chunk peek.
async fn open_and_classify_initial_upstream(
    client: &reqwest::Client,
    spec: &HttpRelaySpec,
    kill_notify: &Notify,
    require_audio: bool,
) -> AudioResult<PrefetchedUpstream> {
    let initial_url_is_hls = is_hls_url(&spec.input_url);
    let chunked_req_size = if !initial_url_is_hls {
        spec.request_size.filter(|&sz| sz > 0)
    } else {
        None
    };
    let initial_range_owned: Option<String> =
        chunked_req_size.map(|req_sz| format!("bytes=0-{}", req_sz.saturating_sub(1)));

    let mut upstream = match send_with_preserved_headers(
        client,
        Method::GET,
        &spec.input_url,
        &spec.headers,
        initial_range_owned.as_deref(),
        MAX_REDIRECTS,
    )
    .await
    {
        Ok(resp)
            if resp.status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE
                && chunked_req_size.is_some() =>
        {
            send_with_preserved_headers(
                client,
                Method::GET,
                &spec.input_url,
                &spec.headers,
                None,
                MAX_REDIRECTS,
            )
            .await?
        }
        Ok(resp) => resp,
        Err(err) => return Err(err),
    };

    if !upstream.status.is_success() {
        let target = sanitize_relay_url(upstream.final_url.as_str());
        log::warn!(
            "VirtualFfmpegShim upstream returned HTTP {} for {target}",
            upstream.status
        );
        if require_audio {
            return Err(AudioError::Http(format!(
                "upstream HTTP {} for {target}",
                upstream.status
            )));
        }
        return Ok(PrefetchedUpstream::Stream {
            upstream: Box::new(upstream),
            chunked_range_mode: false,
            chunked_req_size: None,
            chunked_total_size: None,
            first_chunk: Vec::new(),
        });
    }

    let chunked_range_mode =
        chunked_req_size.is_some() && upstream.status == reqwest::StatusCode::PARTIAL_CONTENT;
    let chunked_total_size = if chunked_range_mode {
        upstream.total_size
    } else {
        None
    };

    let upstream_is_hls = initial_url_is_hls
        || is_hls_url(upstream.final_url.as_str())
        || is_hls_content_type(upstream.content_type.as_deref());

    if upstream_is_hls {
        let playlist_url = upstream.final_url.clone();
        let playlist_text = read_text_body_capped(&mut upstream.response, kill_notify).await?;
        if require_audio && !looks_like_hls_manifest(playlist_text.as_bytes()) {
            return Err(AudioError::Http(
                "upstream HLS URL did not return a valid #EXTM3U playlist".into(),
            ));
        }
        return Ok(PrefetchedUpstream::Hls {
            playlist_url,
            playlist_text,
        });
    }

    if require_audio
        && let Some(bad_mime) = is_rejected_non_audio_mime(upstream.content_type.as_deref())
    {
        return Err(AudioError::Http(format!(
            "upstream returned non-audio MIME ({bad_mime})"
        )));
    }

    // Peek the first non-empty chunk so opaque HLS playlists (`#EXTM3U`) and 0-byte bodies
    // are classified before advertising `Input #0` or sending HTTP response headers.
    let mut first_chunk = Vec::new();
    loop {
        let chunk_res = tokio::select! {
            _ = kill_notify.notified() => {
                return Err(AudioError::VirtualFfmpeg("relay killed during upstream peek".into()));
            }
            chunk = upstream.response.chunk() => chunk,
        };
        match chunk_res {
            Ok(Some(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                if looks_like_hls_manifest(&chunk) {
                    let playlist_url = upstream.final_url.clone();
                    let mut buf = chunk.to_vec();
                    let rest = read_bytes_capped(
                        &mut upstream.response,
                        kill_notify,
                        HLS_MANIFEST_MAX_BYTES.saturating_sub(buf.len()),
                    )
                    .await?;
                    buf.extend_from_slice(&rest);
                    let playlist_text = String::from_utf8_lossy(&buf).into_owned();
                    return Ok(PrefetchedUpstream::Hls {
                        playlist_url,
                        playlist_text,
                    });
                }
                first_chunk = chunk.to_vec();
                break;
            }
            Ok(None) => {
                if require_audio {
                    return Err(AudioError::Http(
                        "upstream returned empty 0-byte audio body".into(),
                    ));
                }
                break;
            }
            Err(err) => {
                return Err(AudioError::Http(format!(
                    "upstream stream read error: {err}"
                )));
            }
        }
    }

    Ok(PrefetchedUpstream::Stream {
        upstream: Box::new(upstream),
        chunked_range_mode,
        chunked_req_size,
        chunked_total_size,
        first_chunk,
    })
}

fn spawn_http_relay(spec: HttpRelaySpec) -> AudioResult<VirtualProcessHandle> {
    // Perform eager upstream preflight for real remote upstreams so HTTP 403/4xx/HTML/0-byte
    // responses exit with code 1 WITHOUT emitting `Input #0` (allowing plugins like
    // `youtube-music` to fall back to `throughYtdlp(id)`). Keep loopback upstreams lazy so
    // existing synchronous local mock tests continue working unchanged.
    let eager_preflight = !Url::parse(&spec.input_url)
        .ok()
        .as_ref()
        .is_some_and(is_loopback_url);
    spawn_http_relay_inner(spec, eager_preflight)
}

fn spawn_http_relay_inner(
    spec: HttpRelaySpec,
    eager_preflight: bool,
) -> AudioResult<VirtualProcessHandle> {
    let bind_addr = format!("{}:{}", spec.listen_host, spec.listen_port);
    let rt = io_runtime_handle();

    let (bind_tx, bind_rx) =
        std::sync::mpsc::channel::<Result<(TcpListener, SocketAddr), String>>();
    let bind_addr_clone = bind_addr.clone();
    rt.spawn(async move {
        let res = async {
            let listener = TcpListener::bind(&bind_addr_clone).await.map_err(|e| {
                format!("failed to bind virtual ffmpeg relay on {bind_addr_clone}: {e}")
            })?;
            let local_addr = listener
                .local_addr()
                .map_err(|e| format!("failed to query relay local_addr: {e}"))?;
            Ok((listener, local_addr))
        }
        .await;
        let _ = bind_tx.send(res);
    });

    let (listener, local_addr) = bind_rx
        .recv()
        .map_err(|_| AudioError::VirtualFfmpeg("relay bind task dropped".into()))?
        .map_err(AudioError::VirtualFfmpeg)?;

    let fmt_label = spec
        .format
        .clone()
        .unwrap_or_else(|| "mov,mp4,m4a,3gp,3g2,mj2".to_owned());
    let codec_label = spec
        .audio_codec
        .clone()
        .filter(|c| c != "copy")
        .unwrap_or_else(|| "aac".to_owned());

    let banner_stderr = format!(
        "ffmpeg version 7.1-nsh-embedded Copyright (c) 2000-2024 the FFmpeg developers\n\
         Input #0, {fmt_label}, from '{}':\n\
           Metadata:\n\
             encoder         : nsh-music VirtualFfmpegShim\n\
           Duration: N/A, start: 0.000000, bitrate: N/A\n\
           Stream #0:0: Audio: {codec_label}, 44100 Hz, stereo, fltp\n\
         Output #0, {fmt_label}, to 'http://{local_addr}/':\n\
           Stream #0:0: Audio: {codec_label}, 44100 Hz, stereo\n",
        spec.input_url
    );

    let initial_stderr = if eager_preflight {
        String::new()
    } else {
        banner_stderr.clone()
    };

    let handle = VirtualProcessHandle::running(Some(local_addr), initial_stderr);
    let task_handle = handle.clone();
    let kill_notify = Arc::clone(&handle.inner.kill_notify);

    rt.spawn(async move {
        let upstream_is_loopback = Url::parse(&spec.input_url)
            .ok()
            .as_ref()
            .is_some_and(is_loopback_url);
        let client_res = if upstream_is_loopback {
            build_loopback_no_redirect_client(Duration::from_secs(15))
        } else {
            build_no_redirect_client(Duration::from_secs(15))
        };
        let client = match client_res {
            Ok(c) => c,
            Err(err) => {
                log::warn!("VirtualFfmpegShim HTTP client error: {err}");
                task_handle.append_stderr(&format!("VirtualFfmpegShim HTTP client error: {err}\n"));
                task_handle.finish(1);
                return;
            }
        };

        let stashed_upstream: Arc<tokio::sync::Mutex<Option<PrefetchedUpstream>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        if eager_preflight {
            let preflight_res = tokio::select! {
                _ = kill_notify.notified() => {
                    task_handle.finish(0);
                    return;
                }
                res = open_and_classify_initial_upstream(&client, &spec, &kill_notify, true) => res,
            };
            match preflight_res {
                Ok(prefetched) => {
                    *stashed_upstream.lock().await = Some(prefetched);
                    task_handle.append_stderr(&banner_stderr);
                }
                Err(err) => {
                    log::warn!(
                        "VirtualFfmpegShim eager preflight failed for {}: {err}",
                        sanitize_relay_url(&spec.input_url)
                    );
                    task_handle
                        .append_stderr(&format!("VirtualFfmpegShim preflight error: {err}\n"));
                    task_handle.finish(1);
                    return;
                }
            }
        }

        loop {
            tokio::select! {
                _ = kill_notify.notified() => {
                    break;
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((socket, _peer)) => {
                            let req_client = client.clone();
                            let req_spec = spec.clone();
                            let req_handle = task_handle.clone();
                            let req_kill = Arc::clone(&kill_notify);
                            let req_stashed = Arc::clone(&stashed_upstream);
                            tokio::spawn(async move {
                                if let Err(err) = handle_relay_connection(
                                    socket,
                                    &req_client,
                                    &req_spec,
                                    &req_handle,
                                    req_kill,
                                    req_stashed,
                                )
                                .await
                                {
                                    log::warn!("VirtualFfmpegShim relay warning: {err}");
                                    req_handle.append_stderr(&format!(
                                        "VirtualFfmpegShim relay warning: {err}\n"
                                    ));
                                }
                            });
                        }
                        Err(err) => {
                            task_handle.append_stderr(&format!(
                                "VirtualFfmpegShim accept error: {err}\n"
                            ));
                            break;
                        }
                    }
                }
            }
        }

        task_handle.finish(0);
    });

    Ok(handle)
}

async fn handle_relay_connection(
    mut socket: TcpStream,
    client: &reqwest::Client,
    spec: &HttpRelaySpec,
    handle: &VirtualProcessHandle,
    kill_notify: Arc<Notify>,
    stashed_upstream: Arc<tokio::sync::Mutex<Option<PrefetchedUpstream>>>,
) -> AudioResult<()> {
    let _ = socket.set_nodelay(true);
    let mut req_buf = Vec::with_capacity(1024);
    let mut temp = [0u8; 1024];
    let header_end;

    loop {
        let n = tokio::time::timeout(Duration::from_secs(15), socket.read(&mut temp))
            .await
            .map_err(|_| AudioError::Http("loopback relay client header timeout".into()))??;
        if n == 0 {
            return Ok(());
        }
        req_buf.extend_from_slice(&temp[..n]);
        if let Some(pos) = req_buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if req_buf.len() > 32 * 1024 {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n",
                )
                .await;
            return Ok(());
        }
    }

    let req_text = String::from_utf8_lossy(&req_buf[..header_end]);
    let mut lines = req_text.lines();
    let request_line = lines.next().unwrap_or("GET / HTTP/1.1");
    let method_str = request_line.split_whitespace().next().unwrap_or("GET");
    let is_head = method_str.eq_ignore_ascii_case("HEAD");

    let mut client_range: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("Range")
        {
            client_range = Some(v.trim().to_owned());
        }
    }

    let (
        mut upstream,
        chunked_range_mode,
        chunked_req_size,
        chunked_total_size,
        prefetched_first_chunk,
    ) = if !is_head && client_range.is_none() {
        let maybe_stashed = stashed_upstream.lock().await.take();
        let prefetched = match maybe_stashed {
            Some(stashed) => stashed,
            None => {
                match open_and_classify_initial_upstream(client, spec, &kill_notify, false).await {
                    Ok(p) => p,
                    Err(err) => {
                        log::warn!(
                            "VirtualFfmpegShim upstream open failed for {}: {err}",
                            sanitize_relay_url(&spec.input_url)
                        );
                        let body = format!("VirtualFfmpegShim upstream error: {err}");
                        let header = format!(
                            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                        return Err(err);
                    }
                }
            }
        };
        match prefetched {
            PrefetchedUpstream::Hls {
                playlist_url,
                playlist_text,
            } => {
                return handle_hls_relay_from_text(
                    socket,
                    client,
                    spec,
                    handle,
                    kill_notify,
                    playlist_url,
                    playlist_text,
                )
                .await;
            }
            PrefetchedUpstream::Stream {
                upstream,
                chunked_range_mode,
                chunked_req_size,
                chunked_total_size,
                first_chunk,
            } => (
                *upstream,
                chunked_range_mode,
                chunked_req_size,
                chunked_total_size,
                if first_chunk.is_empty() {
                    None
                } else {
                    Some(first_chunk)
                },
            ),
        }
    } else {
        let upstream_method = if is_head { Method::HEAD } else { Method::GET };
        let resp = match send_with_preserved_headers(
            client,
            upstream_method,
            &spec.input_url,
            &spec.headers,
            client_range.as_deref(),
            MAX_REDIRECTS,
        )
        .await
        {
            Ok(resp) => {
                if !resp.status.is_success() {
                    log::warn!(
                        "VirtualFfmpegShim upstream returned HTTP {} for {}",
                        resp.status,
                        sanitize_relay_url(resp.final_url.as_str())
                    );
                }
                resp
            }
            Err(err) => {
                log::warn!(
                    "VirtualFfmpegShim upstream request failed for {}: {err}",
                    sanitize_relay_url(&spec.input_url)
                );
                let body = format!("VirtualFfmpegShim upstream error: {err}");
                let header = format!(
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                return Err(err);
            }
        };
        (resp, false, None, None, None)
    };

    // When VirtualFfmpegShim injected `Range: bytes=0-{request_size-1}` on behalf of a
    // non-Range client GET (e.g. YouTube Music `-request_size 10485760` anti-throttling),
    // present a clean `200 OK` full-length response to the local player.
    let (status_code, reason) = if chunked_range_mode {
        (200u16, "OK")
    } else {
        (
            upstream.status.as_u16(),
            upstream.status.canonical_reason().unwrap_or("OK"),
        )
    };
    let content_type =
        content_type_for_format(spec.format.as_deref(), upstream.content_type.as_deref());
    let advertised_len = if chunked_range_mode {
        chunked_total_size.or(upstream.content_length)
    } else {
        upstream.content_length
    };

    let mut resp_header = format!(
        "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nConnection: close\r\n"
    );
    if let Some(len) = advertised_len {
        resp_header.push_str("Accept-Ranges: bytes\r\n");
        resp_header.push_str(&format!("Content-Length: {len}\r\n"));
    }
    if !chunked_range_mode
        && let Some(cr) = upstream
            .response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
    {
        resp_header.push_str(&format!("Content-Range: {cr}\r\n"));
    }
    resp_header.push_str("\r\n");

    socket.write_all(resp_header.as_bytes()).await?;
    if is_head {
        let _ = socket.shutdown().await;
        return Ok(());
    }

    let mut bytes_sent = 0u64;
    if let Some(first) = prefetched_first_chunk
        && !first.is_empty()
    {
        if socket.write_all(&first).await.is_err() {
            let _ = socket.shutdown().await;
            return Ok(());
        }
        bytes_sent += first.len() as u64;
    }
    let mut retries_left = if spec.reconnect { 3u8 } else { 0u8 };

    loop {
        tokio::select! {
            _ = kill_notify.notified() => {
                break;
            }
            chunk_res = upstream.response.chunk() => {
                match chunk_res {
                    Ok(Some(chunk)) => {
                        if socket.write_all(&chunk).await.is_err() {
                            break;
                        }
                        bytes_sent += chunk.len() as u64;
                    }
                    Ok(None) => {
                        // In `-request_size` chunked mode, seamlessly fetch the next Range chunk
                        // until the full upstream `total_size` has been relayed.
                        if chunked_range_mode
                            && let (Some(req_sz), Some(total)) = (chunked_req_size, chunked_total_size)
                            && bytes_sent < total
                        {
                            let next_end = bytes_sent
                                .saturating_add(req_sz)
                                .saturating_sub(1)
                                .min(total.saturating_sub(1));
                            let next_range = format!("bytes={bytes_sent}-{next_end}");
                            match send_with_preserved_headers(
                                client,
                                Method::GET,
                                &spec.input_url,
                                &spec.headers,
                                Some(&next_range),
                                MAX_REDIRECTS,
                            )
                            .await
                            {
                                Ok(next_resp)
                                    if next_resp.status == reqwest::StatusCode::PARTIAL_CONTENT =>
                                {
                                    upstream = next_resp;
                                    continue;
                                }
                                Ok(next_resp) => {
                                    handle.append_stderr(&format!(
                                        "VirtualFfmpegShim next chunk HTTP {} at offset {bytes_sent}\n",
                                        next_resp.status
                                    ));
                                }
                                Err(err) => {
                                    handle.append_stderr(&format!(
                                        "VirtualFfmpegShim next chunk error at offset {bytes_sent}: {err}\n"
                                    ));
                                }
                            }
                        }
                        break;
                    }
                    Err(err) => {
                        if retries_left > 0 && bytes_sent > 0 && client_range.is_none() {
                            retries_left -= 1;
                            let resume_range = match (chunked_req_size, chunked_total_size) {
                                (Some(req_sz), Some(total)) if bytes_sent < total => {
                                    let next_end = bytes_sent
                                        .saturating_add(req_sz)
                                        .saturating_sub(1)
                                        .min(total.saturating_sub(1));
                                    format!("bytes={bytes_sent}-{next_end}")
                                }
                                _ => format!("bytes={bytes_sent}-"),
                            };
                            if let Ok(reconnected) = send_with_preserved_headers(
                                client,
                                Method::GET,
                                &spec.input_url,
                                &spec.headers,
                                Some(&resume_range),
                                MAX_REDIRECTS,
                            )
                            .await
                            {
                                upstream = reconnected;
                                continue;
                            }
                        }
                        return Err(AudioError::Http(format!("upstream stream read error: {err}")));
                    }
                }
            }
        }
    }

    let _ = socket.shutdown().await;
    handle.append_stderr(&format!(
        "size={:>8}kB bitrate=N/A speed=1.0x\n",
        bytes_sent / 1024
    ));
    Ok(())
}

const HLS_MANIFEST_MAX_BYTES: usize = 4 * 1024 * 1024;
const HLS_MAX_MASTER_HOPS: usize = 3;

fn is_hls_url(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".m3u8") || path.contains("/hls_") || path.contains("/manifest/hls")
}

fn is_hls_content_type(ct: Option<&str>) -> bool {
    let Some(ct) = ct else {
        return false;
    };
    let lower = ct.to_ascii_lowercase();
    lower.contains("mpegurl") || lower.contains("vnd.apple.mpegurl")
}

fn looks_like_hls_manifest(chunk: &[u8]) -> bool {
    let trimmed = chunk.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(chunk);
    let trimmed = trimmed
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|idx| &trimmed[idx..])
        .unwrap_or(&[]);
    trimmed.starts_with(b"#EXTM3U")
}

async fn read_bytes_capped(
    resp: &mut reqwest::Response,
    kill_notify: &Notify,
    max_bytes: usize,
) -> AudioResult<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        tokio::select! {
            _ = kill_notify.notified() => break,
            chunk_res = resp.chunk() => {
                match chunk_res {
                    Ok(Some(chunk)) => {
                        let remaining = max_bytes.saturating_sub(buf.len());
                        if remaining == 0 {
                            break;
                        }
                        let take = remaining.min(chunk.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    Ok(None) => break,
                    Err(err) => {
                        return Err(AudioError::Http(format!("HLS upstream read error: {err}")));
                    }
                }
            }
        }
    }
    Ok(buf)
}

async fn read_text_body_capped(
    resp: &mut reqwest::Response,
    kill_notify: &Notify,
) -> AudioResult<String> {
    let bytes = read_bytes_capped(resp, kill_notify, HLS_MANIFEST_MAX_BYTES).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsSegment {
    url: String,
    byte_range: Option<(u64, u64)>, // inclusive (start, end)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsMediaPlaylist {
    init_segment: Option<HlsSegment>,
    segments: Vec<HlsSegment>,
}

fn parse_hls_attributes(input: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            continue;
        }
        let key = input[key_start..i].trim().to_ascii_uppercase();
        i += 1; // skip '='
        let val = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let mut s = String::new();
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    s.push(bytes[i + 1] as char);
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    s.push(bytes[i] as char);
                    i += 1;
                }
            }
            s
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            input[val_start..i].trim().to_owned()
        };
        if !key.is_empty() {
            attrs.push((key, val));
        }
    }
    attrs
}

fn attr_value<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn parse_hls_byterange(spec: &str, last_end: &mut u64) -> Option<(u64, u64)> {
    let trimmed = spec.trim().trim_matches('"');
    let (len_str, offset_opt) = match trimmed.split_once('@') {
        Some((l, o)) => (l.trim(), Some(o.trim())),
        None => (trimmed, None),
    };
    let len = len_str.parse::<u64>().ok()?;
    if len == 0 {
        return None;
    }
    let start = match offset_opt {
        Some(o) => o.parse::<u64>().ok()?,
        None => *last_end,
    };
    let end = start.saturating_add(len).saturating_sub(1);
    *last_end = end.saturating_add(1);
    Some((start, end))
}

/// If `playlist` is an HLS master playlist, returns the resolved URL of the best audio
/// media playlist (preferring `#EXT-X-MEDIA:TYPE=AUDIO` with `URI="..."`, then audio-only
/// or lowest-bandwidth `#EXT-X-STREAM-INF`). Returns `None` if `playlist` is already a
/// media playlist.
fn select_hls_master_variant(playlist: &str, base_url: &Url) -> Option<Url> {
    struct StreamVariant {
        audio_only: bool,
        is_aac_lc: bool,
        bandwidth: u64,
        uri: String,
    }

    let mut audio_candidates: Vec<(u8, String, usize, String)> = Vec::new();
    let mut aac_lc_groups: Vec<String> = Vec::new();
    let mut stream_variants: Vec<StreamVariant> = Vec::new();

    let lines: Vec<&str> = playlist.lines().map(str::trim).collect();
    let mut idx = 0usize;
    while idx < lines.len() {
        let line = lines[idx];
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_hls_attributes(rest);
            let is_audio =
                attr_value(&attrs, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"));
            if is_audio
                && let Some(uri) = attr_value(&attrs, "URI")
                && !uri.is_empty()
            {
                let is_default =
                    attr_value(&attrs, "DEFAULT").is_some_and(|v| v.eq_ignore_ascii_case("YES"));
                let is_autoselect =
                    attr_value(&attrs, "AUTOSELECT").is_some_and(|v| v.eq_ignore_ascii_case("YES"));
                let group_raw = attr_value(&attrs, "GROUP-ID").unwrap_or("");
                let group = group_raw.to_ascii_lowercase();
                let name = attr_value(&attrs, "NAME")
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_ac3 = group.contains("ec-3")
                    || group.contains("ac-3")
                    || name.contains("dolby")
                    || name.contains("ac-3");
                let mut score = 0u8;
                if !is_ac3 {
                    score += 8;
                }
                if is_default {
                    score += 4;
                }
                if is_autoselect {
                    score += 2;
                }
                audio_candidates.push((score, group_raw.to_owned(), idx, uri.to_owned()));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = parse_hls_attributes(rest);
            let bandwidth = attr_value(&attrs, "BANDWIDTH")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            let codecs = attr_value(&attrs, "CODECS")
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_aac_lc = codecs.contains("mp4a.40.2");
            if is_aac_lc
                && let Some(audio_group) = attr_value(&attrs, "AUDIO")
                && !aac_lc_groups.iter().any(|g| g == audio_group)
            {
                aac_lc_groups.push(audio_group.to_owned());
            }
            let has_video = codecs.contains("avc")
                || codecs.contains("hev")
                || codecs.contains("hvc")
                || codecs.contains("vp0")
                || codecs.contains("vp9")
                || codecs.contains("av01");
            let has_audio = codecs.contains("mp4a")
                || codecs.contains("opus")
                || codecs.contains("mp3")
                || codecs.contains("flac");
            let audio_only = has_audio && !has_video;

            let mut j = idx + 1;
            while j < lines.len() {
                let candidate = lines[j];
                if !candidate.is_empty() && !candidate.starts_with('#') {
                    stream_variants.push(StreamVariant {
                        audio_only,
                        is_aac_lc,
                        bandwidth,
                        uri: candidate.to_owned(),
                    });
                    idx = j;
                    break;
                }
                j += 1;
            }
        }
        idx += 1;
    }

    if !audio_candidates.is_empty() {
        // Prefer standard AAC-LC (`mp4a.40.2`, e.g. YouTube audio group 234 @ 128 kbps)
        // over lower-bitrate HE-AAC (`mp4a.40.5`, group 233 @ 48 kbps) when both are DEFAULT=YES.
        for cand in &mut audio_candidates {
            if aac_lc_groups.iter().any(|g| g == &cand.1) {
                cand.0 = cand.0.saturating_add(1);
            }
        }
        audio_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
        for (_, _, _, uri) in audio_candidates {
            if let Ok(resolved) = base_url.join(&uri) {
                return Some(resolved);
            }
        }
    }

    if !stream_variants.is_empty() {
        // Prefer audio-only variants first, then standard AAC-LC (`mp4a.40.2`, e.g. YouTube
        // `web_safari` muxed `itag=93`/`94`/`95`/`96`) ahead of HE-AAC (`mp4a.40.5`, `itag=91`/`92`),
        // then lowest bandwidth to avoid downloading high-res video.
        stream_variants.sort_by(|a, b| {
            b.audio_only
                .cmp(&a.audio_only)
                .then_with(|| b.is_aac_lc.cmp(&a.is_aac_lc))
                .then_with(|| a.bandwidth.cmp(&b.bandwidth))
        });
        for variant in stream_variants {
            if let Ok(resolved) = base_url.join(&variant.uri) {
                return Some(resolved);
            }
        }
    }

    None
}

fn parse_hls_media_playlist(playlist: &str, base_url: &Url) -> HlsMediaPlaylist {
    let mut init_segment: Option<HlsSegment> = None;
    let mut segments = Vec::new();
    let mut pending_byterange: Option<(u64, u64)> = None;
    let mut last_byterange_end = 0u64;

    for raw_line in playlist.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_hls_attributes(rest);
            if let Some(uri) = attr_value(&attrs, "URI")
                && let Ok(resolved) = base_url.join(uri)
            {
                let mut map_end = 0u64;
                let br = attr_value(&attrs, "BYTERANGE")
                    .and_then(|spec| parse_hls_byterange(spec, &mut map_end));
                init_segment = Some(HlsSegment {
                    url: resolved.to_string(),
                    byte_range: br,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_byterange = parse_hls_byterange(rest, &mut last_byterange_end);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Ok(resolved) = base_url.join(line) {
            segments.push(HlsSegment {
                url: resolved.to_string(),
                byte_range: pending_byterange.take(),
            });
        }
    }

    HlsMediaPlaylist {
        init_segment,
        segments,
    }
}

/// Minimal, zero-dependency MPEG-TS audio elementary stream demuxer for HLS `.ts` segments.
///
/// Parses PAT (`PID 0x0000`) -> PMT -> primary audio PID (`0x0F` ADTS AAC or `0x03`/`0x04`
/// MPEG audio/MP3) and strips 188-byte TS headers + PES packet headers (`00 00 01 ...`) to
/// emit continuous raw elementary audio frames (`audio/aac` or `audio/mpeg`) for Symphonia.
pub(crate) struct TsAudioDemuxer {
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
    audio_stream_type: Option<u8>,
    remainder: Vec<u8>,
}

impl TsAudioDemuxer {
    pub(crate) fn new() -> Self {
        Self {
            pmt_pid: None,
            audio_pid: None,
            audio_stream_type: None,
            remainder: Vec::with_capacity(188),
        }
    }

    pub(crate) fn detected_content_type(&self) -> &'static str {
        match self.audio_stream_type {
            Some(0x03 | 0x04) => "audio/mpeg",
            _ => "audio/aac",
        }
    }

    pub(crate) fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let mut combined_owned;
        let slice: &[u8] = if self.remainder.is_empty() {
            data
        } else {
            combined_owned = std::mem::take(&mut self.remainder);
            combined_owned.extend_from_slice(data);
            &combined_owned
        };

        let mut offset = 0usize;
        while offset + 188 <= slice.len() {
            if slice[offset] != 0x47 {
                // Re-sync to next 0x47 TS sync byte.
                if let Some(next_sync) = slice[offset + 1..].iter().position(|&b| b == 0x47) {
                    offset += 1 + next_sync;
                    continue;
                } else {
                    return;
                }
            }
            let pkt = &slice[offset..offset + 188];
            self.process_packet(pkt, out);
            offset += 188;
        }

        if offset < slice.len() {
            self.remainder.extend_from_slice(&slice[offset..]);
        }
    }

    fn process_packet(&mut self, pkt: &[u8], out: &mut Vec<u8>) {
        let pusi = (pkt[1] & 0x40) != 0;
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | (pkt[2] as u16);
        let afc = (pkt[3] >> 4) & 0x03;
        let has_adaptation = (afc & 0x02) != 0;
        let has_payload = (afc & 0x01) != 0;
        if !has_payload {
            return;
        }

        let mut idx = 4usize;
        if has_adaptation {
            if idx >= pkt.len() {
                return;
            }
            let adapt_len = pkt[idx] as usize;
            idx = idx.saturating_add(1).saturating_add(adapt_len);
        }
        if idx >= pkt.len() {
            return;
        }
        let payload = &pkt[idx..];

        if pid == 0x0000 {
            self.parse_pat(payload, pusi);
        } else if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload, pusi);
        } else if Some(pid) == self.audio_pid {
            if pusi {
                // Strip PES header: 00 00 01 <stream_id> <len_hi> <len_lo> <flags1> <flags2> <hdr_data_len>
                if payload.len() >= 9
                    && payload[0] == 0x00
                    && payload[1] == 0x00
                    && payload[2] == 0x01
                {
                    let pes_hdr_len = 9usize.saturating_add(payload[8] as usize);
                    if pes_hdr_len < payload.len() {
                        out.extend_from_slice(&payload[pes_hdr_len..]);
                    }
                }
            } else {
                out.extend_from_slice(payload);
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8], pusi: bool) {
        if !pusi || payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        let start = 1usize.saturating_add(pointer);
        if start + 8 > payload.len() {
            return;
        }
        let sec = &payload[start..];
        if sec[0] != 0x00 {
            return;
        }
        let section_len = ((((sec[1] & 0x0F) as usize) << 8) | (sec[2] as usize))
            .min(sec.len().saturating_sub(3));
        let end = (3 + section_len).saturating_sub(4); // exclude 4-byte CRC32
        let mut pos = 8usize;
        while pos + 4 <= end && pos + 4 <= sec.len() {
            let program_num = u16::from_be_bytes([sec[pos], sec[pos + 1]]);
            let prog_pid = (((sec[pos + 2] & 0x1F) as u16) << 8) | (sec[pos + 3] as u16);
            if program_num != 0 {
                self.pmt_pid = Some(prog_pid);
                return;
            }
            pos += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8], pusi: bool) {
        if !pusi || payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        let start = 1usize.saturating_add(pointer);
        if start + 12 > payload.len() {
            return;
        }
        let sec = &payload[start..];
        if sec[0] != 0x02 {
            return;
        }
        let section_len = ((((sec[1] & 0x0F) as usize) << 8) | (sec[2] as usize))
            .min(sec.len().saturating_sub(3));
        let end = (3 + section_len).saturating_sub(4); // exclude 4-byte CRC32
        let program_info_len = (((sec[10] & 0x0F) as usize) << 8) | (sec[11] as usize);
        let mut pos = 12usize.saturating_add(program_info_len);

        while pos + 5 <= end && pos + 5 <= sec.len() {
            let stream_type = sec[pos];
            let elem_pid = (((sec[pos + 1] & 0x1F) as u16) << 8) | (sec[pos + 2] as u16);
            let es_info_len = (((sec[pos + 3] & 0x0F) as usize) << 8) | (sec[pos + 4] as usize);
            // Prefer ADTS AAC (0x0F) or MPEG-1/2 Audio / MP3 (0x03, 0x04).
            if matches!(stream_type, 0x0F | 0x03 | 0x04) {
                self.audio_pid = Some(elem_pid);
                self.audio_stream_type = Some(stream_type);
                return;
            }
            pos = pos.saturating_add(5).saturating_add(es_info_len);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsStreamKind {
    Fmp4,
    MpegTs,
    RawAdts { strip_id3: bool },
    RawPassthrough(&'static str),
}

fn is_mpeg_ts_bytes(data: &[u8]) -> bool {
    if data.len() < 188 || data[0] != 0x47 {
        return false;
    }
    if data.len() >= 376 {
        return data[188] == 0x47;
    }
    true
}

fn id3v2_tag_len(data: &[u8]) -> Option<usize> {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return None;
    }
    let size = (((data[6] & 0x7F) as usize) << 21)
        | (((data[7] & 0x7F) as usize) << 14)
        | (((data[8] & 0x7F) as usize) << 7)
        | ((data[9] & 0x7F) as usize);
    let footer = if (data[5] & 0x10) != 0 { 10 } else { 0 };
    Some(10usize.saturating_add(size).saturating_add(footer))
}

fn is_adts_sync(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xF6) == 0xF0
}

fn strip_leading_id3v2(data: &[u8]) -> &[u8] {
    let mut cur = data;
    while let Some(len) = id3v2_tag_len(cur) {
        if len <= cur.len() {
            cur = &cur[len..];
        } else {
            break;
        }
    }
    cur
}

fn detect_hls_stream_kind(has_init_segment: bool, probe: &[u8]) -> HlsStreamKind {
    if has_init_segment {
        return HlsStreamKind::Fmp4;
    }
    if probe.len() >= 8 {
        let box_tag = &probe[4..8];
        if matches!(box_tag, b"ftyp" | b"styp" | b"moof" | b"moov" | b"sidx") {
            return HlsStreamKind::Fmp4;
        }
    }
    if is_mpeg_ts_bytes(probe) {
        return HlsStreamKind::MpegTs;
    }
    if is_adts_sync(probe) {
        return HlsStreamKind::RawAdts { strip_id3: false };
    }
    if let Some(id3_len) = id3v2_tag_len(probe)
        && id3_len + 2 <= probe.len()
        && is_adts_sync(&probe[id3_len..])
    {
        return HlsStreamKind::RawAdts { strip_id3: true };
    }
    if probe.starts_with(b"ID3")
        || (probe.len() >= 2 && probe[0] == 0xFF && (probe[1] & 0xE0) == 0xE0)
    {
        return HlsStreamKind::RawPassthrough("audio/mpeg");
    }
    if probe.len() >= 4 && probe[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return HlsStreamKind::RawPassthrough("audio/webm");
    }
    if probe.starts_with(b"OggS") {
        return HlsStreamKind::RawPassthrough("audio/ogg");
    }
    if probe.starts_with(b"fLaC") {
        return HlsStreamKind::RawPassthrough("audio/flac");
    }
    if probe.len() >= 12 && &probe[..4] == b"RIFF" && &probe[8..12] == b"WAVE" {
        return HlsStreamKind::RawPassthrough("audio/wav");
    }
    HlsStreamKind::RawPassthrough("application/octet-stream")
}

async fn fetch_hls_segment_bytes(
    client: &reqwest::Client,
    headers: &[(String, String)],
    segment: &HlsSegment,
    kill_notify: &Notify,
) -> AudioResult<Vec<u8>> {
    let range_hdr = segment
        .byte_range
        .map(|(start, end)| format!("bytes={start}-{end}"));
    let mut resp = send_with_preserved_headers(
        client,
        Method::GET,
        &segment.url,
        headers,
        range_hdr.as_deref(),
        MAX_REDIRECTS,
    )
    .await?;
    if !resp.status.is_success() {
        return Err(AudioError::Http(format!(
            "HLS segment HTTP {} for {}",
            resp.status, resp.final_url
        )));
    }
    let mut data = Vec::new();
    loop {
        tokio::select! {
            _ = kill_notify.notified() => break,
            chunk = resp.response.chunk() => {
                match chunk {
                    Ok(Some(c)) => data.extend_from_slice(&c),
                    Ok(None) => break,
                    Err(err) => {
                        return Err(AudioError::Http(format!(
                            "HLS segment read error for {}: {err}",
                            resp.final_url
                        )));
                    }
                }
            }
        }
    }
    Ok(data)
}

async fn handle_hls_relay_from_text(
    mut socket: TcpStream,
    client: &reqwest::Client,
    spec: &HttpRelaySpec,
    handle: &VirtualProcessHandle,
    kill_notify: Arc<Notify>,
    mut playlist_url: Url,
    mut playlist_text: String,
) -> AudioResult<()> {
    // Follow nested HLS master playlists (up to 3 hops) until we reach a media playlist.
    for _ in 0..HLS_MAX_MASTER_HOPS {
        let Some(variant_url) = select_hls_master_variant(&playlist_text, &playlist_url) else {
            break;
        };
        let mut resp = send_with_preserved_headers(
            client,
            Method::GET,
            variant_url.as_str(),
            &spec.headers,
            None,
            MAX_REDIRECTS,
        )
        .await?;
        if !resp.status.is_success() {
            let body = format!(
                "VirtualFfmpegShim HLS variant HTTP {} for {}",
                resp.status, resp.final_url
            );
            let hdr = format!(
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(hdr.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            return Err(AudioError::Http(body));
        }
        playlist_url = resp.final_url;
        playlist_text = read_text_body_capped(&mut resp.response, &kill_notify).await?;
    }

    let media = parse_hls_media_playlist(&playlist_text, &playlist_url);
    if media.segments.is_empty() && media.init_segment.is_none() {
        let body = "VirtualFfmpegShim HLS playlist contained no playable media segments";
        let hdr = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(hdr.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
        return Err(AudioError::Http(body.into()));
    }

    let init_bytes = if let Some(ref init_seg) = media.init_segment {
        Some(fetch_hls_segment_bytes(client, &spec.headers, init_seg, &kill_notify).await?)
    } else {
        None
    };

    let (first_seg_bytes, remaining_start) = if init_bytes.is_some() {
        (None, 0usize)
    } else if let Some(first_seg) = media.segments.first() {
        let bytes = fetch_hls_segment_bytes(client, &spec.headers, first_seg, &kill_notify).await?;
        (Some(bytes), 1usize)
    } else {
        (None, 0usize)
    };

    let probe_slice: &[u8] = init_bytes
        .as_deref()
        .or(first_seg_bytes.as_deref())
        .unwrap_or(&[]);
    let kind = detect_hls_stream_kind(init_bytes.is_some(), probe_slice);

    let mut ts_demuxer = if kind == HlsStreamKind::MpegTs {
        Some(TsAudioDemuxer::new())
    } else {
        None
    };
    let mut initial_demuxed = Vec::new();
    if let (Some(demuxer), Some(first)) = (ts_demuxer.as_mut(), first_seg_bytes.as_deref()) {
        demuxer.feed(first, &mut initial_demuxed);
    }

    let content_type = match kind {
        HlsStreamKind::Fmp4 => "audio/mp4",
        HlsStreamKind::MpegTs => ts_demuxer
            .as_ref()
            .map(TsAudioDemuxer::detected_content_type)
            .unwrap_or("audio/aac"),
        HlsStreamKind::RawAdts { .. } => "audio/aac",
        HlsStreamKind::RawPassthrough(ct) => ct,
    };

    // Unwrapped HLS streams have unknown total length: NEVER emit Content-Length or Accept-Ranges.
    let resp_header =
        format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n");
    socket.write_all(resp_header.as_bytes()).await?;

    let mut bytes_sent = 0u64;
    if let Some(init) = init_bytes
        && !init.is_empty()
    {
        if socket.write_all(&init).await.is_err() {
            let _ = socket.shutdown().await;
            return Ok(());
        }
        bytes_sent += init.len() as u64;
    }

    if kind == HlsStreamKind::MpegTs {
        if !initial_demuxed.is_empty() {
            if socket.write_all(&initial_demuxed).await.is_err() {
                let _ = socket.shutdown().await;
                return Ok(());
            }
            bytes_sent += initial_demuxed.len() as u64;
        }
    } else if let Some(first) = first_seg_bytes {
        let payload = if matches!(kind, HlsStreamKind::RawAdts { strip_id3: true }) {
            strip_leading_id3v2(&first)
        } else {
            &first
        };
        if !payload.is_empty() {
            if socket.write_all(payload).await.is_err() {
                let _ = socket.shutdown().await;
                return Ok(());
            }
            bytes_sent += payload.len() as u64;
        }
    }

    let mut demux_scratch = Vec::new();
    for seg in &media.segments[remaining_start..] {
        if !handle.is_running() {
            break;
        }
        let range_hdr = seg
            .byte_range
            .map(|(start, end)| format!("bytes={start}-{end}"));
        let resp_res = tokio::select! {
            _ = kill_notify.notified() => break,
            res = send_with_preserved_headers(
                client,
                Method::GET,
                &seg.url,
                &spec.headers,
                range_hdr.as_deref(),
                MAX_REDIRECTS,
            ) => res,
        };
        let mut resp = match resp_res {
            Ok(r) if r.status.is_success() => r,
            Ok(r) => {
                handle.append_stderr(&format!(
                    "VirtualFfmpegShim HLS segment HTTP {} for {}\n",
                    r.status, r.final_url
                ));
                break;
            }
            Err(err) => {
                handle.append_stderr(&format!("VirtualFfmpegShim HLS segment error: {err}\n"));
                break;
            }
        };

        let strip_id3_this_seg = matches!(kind, HlsStreamKind::RawAdts { strip_id3: true });
        let mut id3_stripped = !strip_id3_this_seg;
        let mut seg_prefix_buf: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                _ = kill_notify.notified() => {
                    let _ = socket.shutdown().await;
                    return Ok(());
                }
                chunk_res = resp.response.chunk() => {
                    match chunk_res {
                        Ok(Some(chunk)) => {
                            if chunk.is_empty() {
                                continue;
                            }
                            if let Some(demuxer) = ts_demuxer.as_mut() {
                                demux_scratch.clear();
                                demuxer.feed(&chunk, &mut demux_scratch);
                                if !demux_scratch.is_empty() {
                                    if socket.write_all(&demux_scratch).await.is_err() {
                                        let _ = socket.shutdown().await;
                                        return Ok(());
                                    }
                                    bytes_sent += demux_scratch.len() as u64;
                                }
                            } else if !id3_stripped {
                                seg_prefix_buf.extend_from_slice(&chunk);
                                // Wait until we have enough bytes to determine and skip any leading ID3v2 tag.
                                if seg_prefix_buf.len() < 10 {
                                    continue;
                                }
                                if let Some(tag_len) = id3v2_tag_len(&seg_prefix_buf)
                                    && seg_prefix_buf.len() < tag_len
                                {
                                    continue;
                                }
                                let remainder = strip_leading_id3v2(&seg_prefix_buf);
                                id3_stripped = true;
                                if !remainder.is_empty() {
                                    if socket.write_all(remainder).await.is_err() {
                                        let _ = socket.shutdown().await;
                                        return Ok(());
                                    }
                                    bytes_sent += remainder.len() as u64;
                                }
                                seg_prefix_buf.clear();
                            } else {
                                if socket.write_all(&chunk).await.is_err() {
                                    let _ = socket.shutdown().await;
                                    return Ok(());
                                }
                                bytes_sent += chunk.len() as u64;
                            }
                        }
                        Ok(None) => {
                            if !id3_stripped && !seg_prefix_buf.is_empty() {
                                let remainder = strip_leading_id3v2(&seg_prefix_buf);
                                if !remainder.is_empty() {
                                    let _ = socket.write_all(remainder).await;
                                    bytes_sent += remainder.len() as u64;
                                }
                            }
                            break;
                        }
                        Err(err) => {
                            handle.append_stderr(&format!(
                                "VirtualFfmpegShim HLS chunk read error: {err}\n"
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }

    let _ = socket.shutdown().await;
    handle.append_stderr(&format!(
        "size={:>8}kB bitrate=N/A speed=1.0x (hls)\n",
        bytes_sent / 1024
    ));
    Ok(())
}

fn spawn_stdout_pipe(
    input_url: String,
    headers: Vec<(String, String)>,
    format: Option<String>,
    _reconnect: bool,
) -> AudioResult<VirtualProcessHandle> {
    let fmt = format.unwrap_or_else(|| "audio".to_owned());
    let initial_stderr = format!("Input #0, {fmt}, from '{input_url}':\n  Stream #0:0: Audio\n");
    let handle = VirtualProcessHandle::running(None, initial_stderr);
    let task_handle = handle.clone();
    let kill_notify = Arc::clone(&handle.inner.kill_notify);

    io_runtime_handle().spawn(async move {
        let client = match build_no_redirect_client(Duration::from_secs(15)) {
            Ok(c) => c,
            Err(err) => {
                task_handle.append_stderr(&format!("HTTP client error: {err}\n"));
                task_handle.finish(1);
                return;
            }
        };
        let mut resp = match send_with_preserved_headers(
            &client,
            Method::GET,
            &input_url,
            &headers,
            None,
            MAX_REDIRECTS,
        )
        .await
        {
            Ok(r) => r,
            Err(err) => {
                task_handle.append_stderr(&format!("upstream fetch error: {err}\n"));
                task_handle.finish(1);
                return;
            }
        };

        let mut total = 0usize;
        loop {
            tokio::select! {
                _ = kill_notify.notified() => break,
                chunk = resp.response.chunk() => {
                    match chunk {
                        Ok(Some(bytes)) => {
                            total += bytes.len();
                            task_handle.append_stdout(&String::from_utf8_lossy(&bytes));
                        }
                        Ok(None) => break,
                        Err(err) => {
                            task_handle.append_stderr(&format!("pipe stream error: {err}\n"));
                            task_handle.finish(1);
                            return;
                        }
                    }
                }
            }
        }
        task_handle.append_stderr(&format!("piped {total} bytes\n"));
        task_handle.finish(0);
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn test_parse_youtube_music_exact_ffmpeg_flags() {
        let args: Vec<String> = [
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-reconnect",
            "1",
            "-reconnect_streamed",
            "1",
            "-reconnect_delay_max",
            "5",
            "-rw_timeout",
            "15000000",
            "-request_size",
            "10485760",
            "-user_agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
            "-i",
            "https://rr1---sn.googlevideo.com/videoplayback?mime=audio%2Fmp4",
            "-vn",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
            "-f",
            "mp3",
            "-listen",
            "1",
            "http://127.0.0.1:24567/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        match VirtualFfmpegShim::parse_args(&args) {
            FfmpegInvocation::HttpRelay(spec) => {
                assert_eq!(
                    spec.input_url,
                    "https://rr1---sn.googlevideo.com/videoplayback?mime=audio%2Fmp4"
                );
                assert_eq!(spec.output_url, "http://127.0.0.1:24567/");
                assert_eq!(spec.listen_host, "127.0.0.1");
                assert_eq!(spec.listen_port, 24567);
                assert!(spec.listen);
                assert!(spec.reconnect);
                assert!(spec.disable_video);
                assert_eq!(spec.request_size, Some(10_485_760));
                assert_eq!(spec.format.as_deref(), Some("mp3"));
                assert_eq!(spec.audio_codec.as_deref(), Some("libmp3lame"));
                assert!(spec.headers.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("User-Agent")
                        && v == "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"
                }));
            }
            other => panic!("expected HttpRelay, got {other:?}"),
        }

        // Verify -id3v2_version 0 / -write_xing 0 even after the output URL never clobber output_target.
        let reordered: Vec<String> = [
            "-i",
            "https://example.com/audio.webm",
            "-f",
            "mp3",
            "-listen",
            "1",
            "http://127.0.0.1:24568/",
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        match VirtualFfmpegShim::parse_args(&reordered) {
            FfmpegInvocation::HttpRelay(spec) => {
                assert_eq!(spec.output_url, "http://127.0.0.1:24568/");
                assert_eq!(spec.listen_port, 24568);
            }
            other => panic!("expected HttpRelay with reordered flags, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_youtube_music_relay_preserves_upstream_content_type() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let payload = b"fake-mp4-audio-stream-bytes".to_vec();
        let server_payload = payload.clone();

        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream_listener.accept().await.expect("accept upstream");
            let mut buf = vec![0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                server_payload.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write header");
            socket.write_all(&server_payload).await.expect("write body");
            let _ = socket.shutdown().await;
        });

        let free_port = {
            let tmp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
            tmp.local_addr().expect("local addr").port()
        };
        let relay_url = format!("http://127.0.0.1:{free_port}/");
        let upstream_url = format!("http://{upstream_addr}/videoplayback");

        let proc = VirtualFfmpegShim::spawn_str(&[
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-reconnect",
            "1",
            "-reconnect_streamed",
            "1",
            "-reconnect_delay_max",
            "5",
            "-rw_timeout",
            "15000000",
            "-request_size",
            "10485760",
            "-user_agent",
            "Mozilla/5.0",
            "-i",
            &upstream_url,
            "-vn",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
            "-f",
            "mp3",
            "-listen",
            "1",
            &relay_url,
        ])
        .expect("spawn youtube-music relay");

        assert!(proc.is_running());
        assert!(proc.error_output().contains("Input #0"));

        let resp = reqwest::get(&relay_url).await.expect("GET relay");
        assert!(resp.status().is_success());
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("audio/mp4"),
            "relay must preserve upstream audio/mp4 Content-Type even when -f mp3 was passed"
        );
        let bytes = resp.bytes().await.expect("read body");
        assert_eq!(bytes.as_ref(), payload.as_slice());

        proc.kill();
        assert_eq!(proc.wait(), 0);
        upstream_task.await.expect("upstream task ok");
    }

    /// Builds a 188-byte MPEG-TS packet with optional adaptation stuffing so payload fits cleanly.
    fn make_ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = (if pusi { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        let max_payload = 184;
        if payload.len() >= max_payload {
            pkt[3] = 0x10 | (cc & 0x0F); // payload only
            pkt[4..188].copy_from_slice(&payload[..max_payload]);
        } else {
            let stuffing = max_payload - payload.len();
            pkt[3] = 0x30 | (cc & 0x0F); // adaptation + payload
            if stuffing == 1 {
                pkt[4] = 0; // adaptation_field_length = 0
                pkt[5..5 + payload.len()].copy_from_slice(payload);
            } else {
                pkt[4] = (stuffing - 1) as u8;
                pkt[5] = 0x00; // adaptation flags
                for b in &mut pkt[6..4 + stuffing] {
                    *b = 0xFF;
                }
                pkt[4 + stuffing..4 + stuffing + payload.len()].copy_from_slice(payload);
            }
        }
        pkt
    }

    fn make_ts_pat(pmt_pid: u16) -> Vec<u8> {
        let mut sec = vec![
            0x00, // pointer_field
            0x00, // table_id = PAT
            0xB0,
            0x0D, // section_length = 13
            0x00,
            0x01, // transport_stream_id
            0xC1, // version 0, current_next=1
            0x00, // section_number
            0x00, // last_section_number
            0x00,
            0x01, // program_number = 1
            0xE0 | ((pmt_pid >> 8) as u8 & 0x1F),
            (pmt_pid & 0xFF) as u8,
            0x00,
            0x00,
            0x00,
            0x00, // CRC placeholder
        ];
        // Pad PSI section inside payload-only TS packet with 0xFF.
        sec.resize(184, 0xFF);
        make_ts_packet(0x0000, true, 0, &sec)
    }

    fn make_ts_pmt(pmt_pid: u16, audio_pid: u16, stream_type: u8) -> Vec<u8> {
        let mut sec = vec![
            0x00, // pointer_field
            0x02, // table_id = PMT
            0xB0,
            0x12, // section_length = 18
            0x00,
            0x01, // program_number = 1
            0xC1, // version 0, current_next=1
            0x00, // section_number
            0x00, // last_section_number
            0xE0 | ((audio_pid >> 8) as u8 & 0x1F),
            (audio_pid & 0xFF) as u8, // PCR_PID
            0xF0,
            0x00, // program_info_length = 0
            stream_type,
            0xE0 | ((audio_pid >> 8) as u8 & 0x1F),
            (audio_pid & 0xFF) as u8,
            0xF0,
            0x00, // ES_info_length = 0
            0x00,
            0x00,
            0x00,
            0x00, // CRC placeholder
        ];
        sec.resize(184, 0xFF);
        make_ts_packet(pmt_pid, true, 0, &sec)
    }

    fn make_ts_audio_pes_packets(audio_pid: u16, elementary: &[u8]) -> Vec<u8> {
        let mut pes = vec![
            0x00, 0x00, 0x01, // PES start code
            0xC0, // audio stream_id
            0x00, 0x00, // unbounded PES packet length
            0x80, 0x80, // flags (PTS present)
            0x05, // pes_header_data_length = 5 bytes
            0x21, 0x00, 0x01, 0x00, 0x01, // dummy PTS
        ];
        pes.extend_from_slice(elementary);

        let mut out = Vec::new();
        let mut offset = 0usize;
        let mut cc = 0u8;
        while offset < pes.len() {
            let end = (offset + 184).min(pes.len());
            out.extend_from_slice(&make_ts_packet(
                audio_pid,
                offset == 0,
                cc,
                &pes[offset..end],
            ));
            offset = end;
            cc = (cc + 1) & 0x0F;
        }
        out
    }

    /// Valid 16-byte raw ADTS AAC-LC 44.1 kHz stereo silent frame (1024 samples/channel).
    const VALID_ADTS_AAC_FRAME: &[u8] = &[
        0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, // 7-byte ADTS header (frame_len=16)
        0x21, 0x00, 0x49, 0x90, 0x02, 0x19, 0x00, 0x23, 0x80, // 9-byte stereo CPE payload
    ];

    #[test]
    fn test_ts_audio_demuxer_pat_pmt_pes_aac_and_mp3() {
        // Test ADTS AAC (stream_type 0x0F) fed across unaligned chunk boundaries.
        let mut ts_aac = Vec::new();
        ts_aac.extend_from_slice(&make_ts_pat(0x0100));
        ts_aac.extend_from_slice(&make_ts_pmt(0x0100, 0x0101, 0x0F));
        let mut expected_aac = Vec::new();
        for _ in 0..16 {
            expected_aac.extend_from_slice(VALID_ADTS_AAC_FRAME);
        }
        ts_aac.extend_from_slice(&make_ts_audio_pes_packets(0x0101, &expected_aac));

        let mut demuxer = TsAudioDemuxer::new();
        let mut out = Vec::new();
        for chunk in ts_aac.chunks(73) {
            demuxer.feed(chunk, &mut out);
        }
        assert_eq!(demuxer.detected_content_type(), "audio/aac");
        assert_eq!(out, expected_aac);

        // Test MPEG Audio / MP3 (stream_type 0x03).
        let mp3_payload = b"ID3-MP3-TEST-ELEMENTARY-PAYLOAD".to_vec();
        let mut ts_mp3 = Vec::new();
        ts_mp3.extend_from_slice(&make_ts_pat(0x0200));
        ts_mp3.extend_from_slice(&make_ts_pmt(0x0200, 0x0201, 0x03));
        ts_mp3.extend_from_slice(&make_ts_audio_pes_packets(0x0201, &mp3_payload));

        let mut demux_mp3 = TsAudioDemuxer::new();
        let mut out_mp3 = Vec::new();
        demux_mp3.feed(&ts_mp3, &mut out_mp3);
        assert_eq!(demux_mp3.detected_content_type(), "audio/mpeg");
        assert_eq!(out_mp3, mp3_payload);
    }

    #[cfg(feature = "audio-symphonia")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_virtual_ffmpeg_hls_unwrap_mpegts_and_decode() {
        let mut adts_seg = Vec::new();
        for _ in 0..8 {
            adts_seg.extend_from_slice(VALID_ADTS_AAC_FRAME);
        }
        let mut ts_seg = Vec::new();
        ts_seg.extend_from_slice(&make_ts_pat(0x0100));
        ts_seg.extend_from_slice(&make_ts_pmt(0x0100, 0x0101, 0x0F));
        ts_seg.extend_from_slice(&make_ts_audio_pes_packets(0x0101, &adts_seg));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind hls");
        let addr = listener.local_addr().expect("hls addr");
        let ts_seg_arc = Arc::new(ts_seg);

        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seg_bytes = Arc::clone(&ts_seg_arc);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    match path {
                        "/master.m3u8" => {
                            let body = concat!(
                                "#EXTM3U\n",
                                "#EXT-X-VERSION:3\n",
                                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n",
                                "#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"avc1.42e01e,mp4a.40.2\",AUDIO=\"aud\"\n",
                                "video.m3u8\n",
                            );
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(body.as_bytes()).await;
                        }
                        "/audio.m3u8" => {
                            let body = concat!(
                                "#EXTM3U\n",
                                "#EXT-X-VERSION:3\n",
                                "#EXT-X-TARGETDURATION:2\n",
                                "#EXTINF:2.0,\n",
                                "seg0.ts\n",
                                "#EXTINF:2.0,\n",
                                "seg1.ts\n",
                                "#EXT-X-ENDLIST\n",
                            );
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(body.as_bytes()).await;
                        }
                        "/seg0.ts" | "/seg1.ts" => {
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                seg_bytes.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(&seg_bytes).await;
                        }
                        _ => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });

        let free_port = {
            let tmp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
            tmp.local_addr().expect("addr").port()
        };
        let relay_url = format!("http://127.0.0.1:{free_port}/");
        let master_url = format!("http://{addr}/master.m3u8");

        let proc = VirtualFfmpegShim::spawn_str(&[
            "-i",
            &master_url,
            "-vn",
            "-f",
            "mp3",
            "-listen",
            "1",
            &relay_url,
        ])
        .expect("spawn hls relay");

        // First verify HTTP response headers directly: audio/aac, NO Content-Length, NO Accept-Ranges.
        let resp = reqwest::get(&relay_url).await.expect("GET hls relay");
        assert!(resp.status().is_success());
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("audio/aac")
        );
        assert!(
            resp.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .is_none(),
            "unwrapped HLS response must NOT emit Content-Length"
        );
        assert!(
            resp.headers().get(reqwest::header::ACCEPT_RANGES).is_none(),
            "unwrapped HLS response must NOT emit Accept-Ranges"
        );
        let body = resp.bytes().await.expect("read body");
        assert_eq!(body.len(), VALID_ADTS_AAC_FRAME.len() * 16);

        // Now decode end-to-end through AudioCache + open_decoder with contradictory `mp3` hint!
        let dir = std::env::temp_dir().join(format!("nsh-hls-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cache = crate::AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");

        let mut dec = crate::open_decoder(
            &cache,
            &relay_url,
            Vec::new(),
            Some("mp3"),
            None,
            crate::DecoderPreference::SymphoniaOnly,
        )
        .expect("open unwrapped HLS ADTS AAC stream via Symphonia");
        assert_eq!(dec.spec().codec_name, "aac");
        assert_eq!(dec.spec().sample_rate, 44_100);
        assert_eq!(dec.spec().channels, 2);

        let mut pcm = Vec::new();
        let mut total = 0usize;
        while let Ok(n) = dec.read_pcm(&mut pcm) {
            if n == 0 {
                break;
            }
            total += n;
        }
        assert!(
            total >= 8 * 1024 * 2,
            "expected decoded ADTS AAC stereo frames across HLS segments, got {total} samples"
        );

        proc.kill();
        let _ = proc.wait();
        srv.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_virtual_ffmpeg_eager_preflight_403_exits_without_input_banner() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 403");
        let addr = listener.local_addr().expect("addr");
        let srv = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let body = b"Forbidden";
            let hdr = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(hdr.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.shutdown().await;
        });

        let free_port = {
            let tmp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
            tmp.local_addr().expect("addr").port()
        };
        let spec = HttpRelaySpec {
            input_url: format!("http://{addr}/videoplayback"),
            headers: vec![("User-Agent".to_owned(), "nsh-test/1.0".to_owned())],
            reconnect: true,
            request_size: Some(10_485_760),
            format: Some("mp3".to_owned()),
            audio_codec: Some("libmp3lame".to_owned()),
            disable_video: true,
            listen: true,
            listen_host: "127.0.0.1".to_owned(),
            listen_port: free_port,
            output_url: format!("http://127.0.0.1:{free_port}/"),
        };

        let proc = spawn_http_relay_inner(spec, true).expect("spawn eager preflight relay");
        let code = tokio::task::spawn_blocking(move || {
            let exit = proc.wait();
            let err_out = proc.error_output();
            (exit, err_out, proc.is_running())
        })
        .await
        .expect("join wait");

        assert_eq!(code.0, 1, "HTTP 403 preflight must exit with code 1");
        assert!(!code.2, "process must no longer be running after 403");
        assert!(
            !code.1.contains("Input #0"),
            "HTTP 403 preflight must NOT emit 'Input #0', got: {}",
            code.1
        );
        srv.await.expect("srv");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_virtual_ffmpeg_eager_preflight_206_serves_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let payload: Vec<u8> = (0..12_345).map(|i| (i % 251) as u8).collect();
        let srv_payload = Arc::new(payload.clone());
        let accept_count = Arc::new(AtomicUsize::new(0));
        let srv_accepts = Arc::clone(&accept_count);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 206");
        let addr = listener.local_addr().expect("addr");
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                srv_accepts.fetch_add(1, Ordering::SeqCst);
                let data = Arc::clone(&srv_payload);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    assert!(
                        req.contains("range: bytes=0-"),
                        "expected Range header from request_size, got: {req}"
                    );
                    let last = data.len() - 1;
                    let hdr = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nContent-Range: bytes 0-{last}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data.len(),
                        data.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(&data).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let free_port = {
            let tmp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
            tmp.local_addr().expect("addr").port()
        };
        let relay_url = format!("http://127.0.0.1:{free_port}/");
        let spec = HttpRelaySpec {
            input_url: format!("http://{addr}/videoplayback"),
            headers: vec![("User-Agent".to_owned(), "nsh-test/1.0".to_owned())],
            reconnect: true,
            request_size: Some(10_485_760),
            format: Some("mp3".to_owned()),
            audio_codec: Some("libmp3lame".to_owned()),
            disable_video: true,
            listen: true,
            listen_host: "127.0.0.1".to_owned(),
            listen_port: free_port,
            output_url: relay_url.clone(),
        };

        let proc = spawn_http_relay_inner(spec, true).expect("spawn eager preflight 206 relay");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !proc.error_output().contains("Input #0")
            && proc.is_running()
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(
            proc.error_output().contains("Input #0"),
            "HTTP 206 preflight must emit 'Input #0', got: {}",
            proc.error_output()
        );

        let local_client =
            build_loopback_no_redirect_client(Duration::from_secs(5)).expect("loopback client");
        let resp = local_client
            .get(&relay_url)
            .send()
            .await
            .expect("GET relay");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("audio/mp4")
        );
        let received = resp.bytes().await.expect("read body");
        assert_eq!(received.as_ref(), payload.as_slice());
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "stashed preflight response must be reused so upstream is hit only once"
        );

        proc.kill();
        let _ = proc.wait();
        srv.abort();
    }

    #[test]
    fn test_select_hls_master_variant_prefers_aac_lc_muxed_itag_93() {
        let base = Url::parse("https://manifest.googlevideo.com/api/manifest/hls_variant/index.m3u8")
            .expect("parse base url");
        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=86008,CODECS=\"avc1.4D400C,mp4a.40.5\",RESOLUTION=256x144\n",
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/91/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=122443,CODECS=\"avc1.4D4015,mp4a.40.5\",RESOLUTION=426x240\n",
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/92/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=263884,CODECS=\"avc1.4D401E,mp4a.40.2\",RESOLUTION=640x360\n",
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/93/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=322741,CODECS=\"avc1.4D401F,mp4a.40.2\",RESOLUTION=854x480\n",
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/94/index.m3u8\n",
        );

        let selected = select_hls_master_variant(master, &base).expect("select variant");
        assert_eq!(
            selected.as_str(),
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/itag/93/index.m3u8",
            "expected lowest-bandwidth AAC-LC muxed variant (itag=93) ahead of HE-AAC (itag=91/92)"
        );
    }
}
