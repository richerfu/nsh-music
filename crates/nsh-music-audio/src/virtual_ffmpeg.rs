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
    MAX_REDIRECTS, build_no_redirect_client, io_runtime_handle, send_with_preserved_headers,
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
    let file_name = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed);
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
        if let Ok(mut guard) = self.inner.state.lock() {
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
        && ct != "application/octet-stream"
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

fn spawn_http_relay(spec: HttpRelaySpec) -> AudioResult<VirtualProcessHandle> {
    let bind_addr = format!("{}:{}", spec.listen_host, spec.listen_port);
    let rt = io_runtime_handle();

    let (bind_tx, bind_rx) = std::sync::mpsc::channel::<Result<(TcpListener, SocketAddr), String>>();
    let bind_addr_clone = bind_addr.clone();
    rt.spawn(async move {
        let res = async {
            let listener = TcpListener::bind(&bind_addr_clone)
                .await
                .map_err(|e| format!("failed to bind virtual ffmpeg relay on {bind_addr_clone}: {e}"))?;
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

    // Emit FFmpeg-compatible stderr banner immediately AFTER binding TCP port so
    // `youtube-music` / `bilibili` plugins polling `proc.errorOutput().includes("Input #0")`
    // unblock right away and immediately connect to an already-listening loopback port.
    let initial_stderr = format!(
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

    let handle = VirtualProcessHandle::running(Some(local_addr), initial_stderr);
    let task_handle = handle.clone();
    let kill_notify = Arc::clone(&handle.inner.kill_notify);

    rt.spawn(async move {
        let client = match build_no_redirect_client(Duration::from_secs(15)) {
            Ok(c) => c,
            Err(err) => {
                task_handle.append_stderr(&format!("VirtualFfmpegShim HTTP client error: {err}\n"));
                task_handle.finish(1);
                return;
            }
        };

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
                            tokio::spawn(async move {
                                if let Err(err) = handle_relay_connection(
                                    socket,
                                    &req_client,
                                    &req_spec,
                                    &req_handle,
                                    req_kill,
                                )
                                .await
                                {
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
) -> AudioResult<()> {
    let mut req_buf = Vec::with_capacity(1024);
    let mut temp = [0u8; 1024];
    let header_end;

    loop {
        let n = socket.read(&mut temp).await?;
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
                .write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n")
                .await;
            return Ok(());
        }
    }

    let req_text = String::from_utf8_lossy(&req_buf[..header_end]);
    let mut lines = req_text.lines();
    let request_line = lines.next().unwrap_or("GET / HTTP/1.1");
    let method_str = request_line.split_whitespace().next().unwrap_or("GET");
    let is_head = method_str.eq_ignore_ascii_case("HEAD");
    let upstream_method = if is_head { Method::HEAD } else { Method::GET };

    let mut client_range: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("Range")
        {
            client_range = Some(v.trim().to_owned());
        }
    }

    let mut upstream = match send_with_preserved_headers(
        client,
        upstream_method,
        &spec.input_url,
        &spec.headers,
        client_range.as_deref(),
        MAX_REDIRECTS,
    )
    .await
    {
        Ok(resp) => resp,
        Err(err) => {
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

    let status_code = upstream.status.as_u16();
    let reason = upstream.status.canonical_reason().unwrap_or("OK");
    let content_type =
        content_type_for_format(spec.format.as_deref(), upstream.content_type.as_deref());

    let mut resp_header = format!(
        "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n"
    );
    if let Some(len) = upstream.content_length {
        resp_header.push_str(&format!("Content-Length: {len}\r\n"));
    }
    if let Some(cr) = upstream
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
                        break;
                    }
                    Err(err) => {
                        if retries_left > 0 && bytes_sent > 0 && client_range.is_none() {
                            retries_left -= 1;
                            let resume_range = format!("bytes={bytes_sent}-");
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
