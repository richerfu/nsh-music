use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nsh_music_audio::virtual_ffmpeg::VirtualProcessHandle;
use nsh_music_core::model::SignInPrompt;
use nsh_music_core::source::SignInPromptCallback;
use rquickjs::function::{Opt, Rest};
use rquickjs::{Array, Ctx, Error as JsError, Exception, Function, IntoJs, Object, Promise, Value};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::crypto;
use crate::headers::{HeaderRequestKind, RequestHeadersResolver};
use crate::installer::atomic_write;
use crate::manifest::{PluginLocales, PluginManifest};
use crate::mobile_compat::{MobileProcessVirtualizer, resolve_relative_command, with_default_cwd};
#[cfg(unix)]
use crate::mobile_compat::{augment_unix_path, resolve_unix_executable};

/// Structured error returned when a plugin tries to spawn an unvirtualized external binary on mobile.
/// Note: Intentionally avoids the substring `"allowed"` (case-insensitive) so optional desktop binary
/// probes do not trip `/allowed/.test(e.message)`.
#[derive(Debug, Clone, Error, PartialEq, Eq, Serialize, Deserialize)]
#[error(
    "MobileSandboxError: external command `{command}` is prohibited on {platform}. {migration_path}"
)]
#[serde(rename_all = "camelCase")]
pub struct MobileSandboxError {
    pub code: String,
    pub command: String,
    pub platform: String,
    pub migration_path: String,
}

impl MobileSandboxError {
    pub fn new(command: impl Into<String>, platform: impl Into<String>) -> Self {
        let command = command.into();
        let platform = platform.into();
        Self {
            code: "MOBILE_SANDBOX_ERROR".to_owned(),
            migration_path: format!(
                "Spawning external binary `{command}` is prohibited by the {platform} mobile sandbox (not permitted on mobile). \
                 Check `host.capabilities.canSpawnExternal` first; replace external HTTP/signing CLIs with `host.fetch` + `host.crypto`, \
                 return direct stream URLs with `headers`, or rely on `host.spawn(\"ffmpeg\" | \"yt-dlp\", ...)` which are virtualized in-process. \
                 See docs/PLUGIN_COMPAT.md for migration steps."
            ),
            command,
            platform,
        }
    }
}

/// Parameters passed from JS `host.webSignIn(opts)` to the native UI WebView presenter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSignInRequest {
    pub plugin_id: String,
    pub url: String,
    pub title: String,
    pub incognito: bool,
    pub user_agent: Option<String>,
    pub eval_js: Option<String>,
    pub cookies_of: Option<String>,
    /// Cookie names whose presence signals sign-in completion (JS `until`, e.g. `["SAPISID", "__Secure-3PSID"]`).
    pub until: Vec<String>,
    pub cookie_names: Vec<String>,
    pub match_url: Option<String>,
    pub allow_manual_cookie: bool,
    pub timeout_ms: u64,
}

/// Outcome returned from the native UI WebView / manual cookie sign-in dialog to JS.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSignInResult {
    pub ok: bool,
    pub url: String,
    pub cookies: HashMap<String, String>,
    pub cookie: String,
    pub eval_result: Option<String>,
    pub cancelled: bool,
    pub manual_fallback: bool,
}

/// Callback invoked by `host.webSignIn(opts)` so the UI can show an embedded `web_view()`
/// (with incognito mode, custom UA, JS eval, and manual cookie fallback for HttpOnly cookies).
pub type WebSignInHandler =
    Arc<dyn Fn(WebSignInRequest) -> Result<WebSignInResult, String> + Send + Sync>;

/// Callback invoked by `host.openURL(url)` so the embedding app can open `http`/`https` pages
/// in the system browser or mobile in-app browser on both Desktop and Mobile.
pub type OpenUrlHandler = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Runtime platform override options for `HostContext` (useful for testing mobile sandbox behavior on desktop).
#[derive(Clone)]
pub struct HostRuntimeOptions {
    pub locale: Option<String>,
    pub is_mobile_override: Option<bool>,
    pub platform_override: Option<String>,
    pub mobile_virtual_compat: bool,
    pub sign_in_prompt: Option<SignInPromptCallback>,
    pub web_sign_in_handler: Option<WebSignInHandler>,
    pub open_url_handler: Option<OpenUrlHandler>,
}

impl Default for HostRuntimeOptions {
    fn default() -> Self {
        Self {
            locale: None,
            is_mobile_override: None,
            platform_override: None,
            mobile_virtual_compat: true,
            sign_in_prompt: None,
            web_sign_in_handler: None,
            open_url_handler: None,
        }
    }
}

impl std::fmt::Debug for HostRuntimeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRuntimeOptions")
            .field("locale", &self.locale)
            .field("is_mobile_override", &self.is_mobile_override)
            .field("platform_override", &self.platform_override)
            .field("mobile_virtual_compat", &self.mobile_virtual_compat)
            .field("has_sign_in_prompt", &self.sign_in_prompt.is_some())
            .field(
                "has_web_sign_in_handler",
                &self.web_sign_in_handler.is_some(),
            )
            .field("has_open_url_handler", &self.open_url_handler.is_some())
            .finish()
    }
}

/// External desktop subprocess wrapper matching `mygo-music` & `VirtualProcessHandle` semantics.
pub(crate) struct ExternalProcessHandle {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stdout_lines: Arc<Mutex<VecDeque<String>>>,
    stdout_eof: Arc<AtomicBool>,
    stdout_cvar: Arc<Condvar>,
    stdout_thread: Mutex<Option<JoinHandle<()>>>,
    stderr_thread: Mutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    done_cvar: Arc<Condvar>,
}

impl ExternalProcessHandle {
    pub(crate) fn spawn(cmd: &str, args: &[String], opts: &SpawnOptions) -> Result<Self, String> {
        #[cfg(unix)]
        let (program, effective_path) = {
            if let Some(explicit_path) = opts.env.get("PATH") {
                let resolved = resolve_unix_executable(cmd, explicit_path)
                    .unwrap_or_else(|| PathBuf::from(cmd));
                (resolved, explicit_path.clone())
            } else {
                let base_path = std::env::var("PATH").unwrap_or_default();
                let augmented = augment_unix_path(&base_path);
                let resolved =
                    resolve_unix_executable(cmd, &augmented).unwrap_or_else(|| PathBuf::from(cmd));
                (resolved, augmented)
            }
        };
        #[cfg(not(unix))]
        let program = PathBuf::from(cmd);

        let mut command = Command::new(&program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &opts.cwd {
            command.current_dir(cwd);
        }
        for (k, v) in &opts.env {
            command.env(k, v);
        }
        #[cfg(unix)]
        {
            command.env("PATH", &effective_path);
        }

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn `{cmd}`: {e}"))?;

        let stdin = Mutex::new(child.stdin.take());
        let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stdout_lines = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let stdout_eof = Arc::new(AtomicBool::new(false));
        let stdout_cvar = Arc::new(Condvar::new());
        let running = Arc::new(AtomicBool::new(true));
        let exit_code = Arc::new(Mutex::new(None));
        let done_cvar = Arc::new(Condvar::new());

        let stdout_thread = if let Some(mut out) = child.stdout.take() {
            let buf = Arc::clone(&stdout_buf);
            let lines = Arc::clone(&stdout_lines);
            let eof = Arc::clone(&stdout_eof);
            let cvar = Arc::clone(&stdout_cvar);
            std::thread::Builder::new()
                .name(format!("plugin-stdout-{cmd}"))
                .spawn(move || {
                    let mut chunk = [0u8; 4096];
                    let mut rem = String::new();
                    while let Ok(n) = out.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        if let Ok(mut guard) = buf.lock() {
                            guard.extend_from_slice(&chunk[..n]);
                        }
                        rem.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        while let Some(pos) = rem.find('\n') {
                            let line = rem[..pos].trim_end_matches('\r').to_owned();
                            rem.drain(..=pos);
                            if let Ok(mut q) = lines.lock() {
                                q.push_back(line);
                            }
                            cvar.notify_all();
                        }
                    }
                    {
                        let mut q = lines.lock();
                        if !rem.is_empty()
                            && let Ok(ref mut q_guard) = q
                        {
                            q_guard.push_back(rem.trim_end_matches('\r').to_owned());
                        }
                        eof.store(true, Ordering::SeqCst);
                        cvar.notify_all();
                        drop(q);
                    }
                })
                .ok()
        } else {
            stdout_eof.store(true, Ordering::SeqCst);
            None
        };

        let stderr_thread = if let Some(mut err) = child.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            std::thread::Builder::new()
                .name(format!("plugin-stderr-{cmd}"))
                .spawn(move || {
                    let mut chunk = [0u8; 4096];
                    const MAX_STDERR_BYTES: usize = 64 * 1024;
                    while let Ok(n) = err.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        if let Ok(mut guard) = buf.lock() {
                            if guard.len() + n > MAX_STDERR_BYTES {
                                // Keep the first 1 KiB (including `Input #0`) and the latest tail.
                                let keep_prefix = guard.len().min(1024);
                                let tail_keep = (MAX_STDERR_BYTES / 2)
                                    .min(guard.len().saturating_sub(keep_prefix));
                                let tail_start = guard.len().saturating_sub(tail_keep);
                                if tail_start > keep_prefix {
                                    let mut compacted = Vec::with_capacity(MAX_STDERR_BYTES);
                                    compacted.extend_from_slice(&guard[..keep_prefix]);
                                    compacted.extend_from_slice(b"\n...\n");
                                    compacted.extend_from_slice(&guard[tail_start..]);
                                    *guard = compacted;
                                }
                            }
                            guard.extend_from_slice(&chunk[..n]);
                        }
                    }
                })
                .ok()
        } else {
            None
        };

        Ok(Self {
            child: Mutex::new(child),
            stdin,
            stdout_buf,
            stderr_buf,
            stdout_lines,
            stdout_eof,
            stdout_cvar,
            stdout_thread: Mutex::new(stdout_thread),
            stderr_thread: Mutex::new(stderr_thread),
            running,
            exit_code,
            done_cvar,
        })
    }

    fn write_stdin(&self, data: &str) -> Result<(), String> {
        if let Ok(mut guard) = self.stdin.lock()
            && let Some(ref mut sin) = *guard
        {
            sin.write_all(data.as_bytes())
                .map_err(|e| format!("stdin write failed: {e}"))?;
        }
        Ok(())
    }

    fn close_stdin(&self) {
        if let Ok(mut guard) = self.stdin.lock() {
            guard.take();
        }
    }

    fn read_line_blocking(&self) -> Option<String> {
        let Ok(mut q) = self.stdout_lines.lock() else {
            return None;
        };
        loop {
            if let Some(line) = q.pop_front() {
                return Some(line);
            }
            if self.stdout_eof.load(Ordering::SeqCst) {
                return None;
            }
            q = match self.stdout_cvar.wait(q) {
                Ok(next) => next,
                Err(_) => return None,
            };
        }
    }

    fn stdout(&self) -> String {
        self.stdout_buf
            .lock()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn stderr(&self) -> String {
        self.stderr_buf
            .lock()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn is_running(&self) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return false;
        }
        if let Ok(mut child) = self.child.lock()
            && let Ok(Some(status)) = child.try_wait()
        {
            self.running.store(false, Ordering::SeqCst);
            if let Ok(mut code) = self.exit_code.lock() {
                if code.is_none() {
                    *code = Some(status.code().unwrap_or(-1));
                }
                self.done_cvar.notify_all();
            }
            return false;
        }
        true
    }

    fn join_io_threads(&self) {
        if let Ok(mut th) = self.stdout_thread.lock()
            && let Some(h) = th.take()
        {
            let _ = h.join();
        }
        if let Ok(mut th) = self.stderr_thread.lock()
            && let Some(h) = th.take()
        {
            let _ = h.join();
        }
    }

    fn wait(&self) -> i32 {
        if let Ok(Some(code)) = self.exit_code.lock().map(|g| *g) {
            self.join_io_threads();
            return code;
        }
        self.close_stdin();
        loop {
            // Never hold `self.child` across a blocking wait so `kill()` and `is_running()`
            // can acquire `self.child` immediately without deadlocking.
            let polled = if let Ok(mut child) = self.child.lock() {
                match child.try_wait() {
                    Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                    Ok(None) => None,
                    Err(_) => Some(-1),
                }
            } else {
                Some(-1)
            };

            if let Some(code) = polled {
                self.running.store(false, Ordering::SeqCst);
                if let Ok(mut guard) = self.exit_code.lock() {
                    let final_code = *guard.get_or_insert(code);
                    self.done_cvar.notify_all();
                    drop(guard);
                    self.join_io_threads();
                    return final_code;
                }
                self.join_io_threads();
                return code;
            }

            let Ok(guard) = self.exit_code.lock() else {
                return -1;
            };
            if let Some(code) = *guard {
                drop(guard);
                self.join_io_threads();
                return code;
            }
            let _ = self
                .done_cvar
                .wait_timeout(guard, Duration::from_millis(15));
        }
    }

    fn kill(&self) {
        self.close_stdin();
        let code = if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
        } else {
            -1
        };
        {
            let _q = self.stdout_lines.lock();
            self.stdout_eof.store(true, Ordering::SeqCst);
            self.stdout_cvar.notify_all();
        }
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.exit_code.lock() {
            if guard.is_none() {
                *guard = Some(code);
            }
            self.done_cvar.notify_all();
        }
    }
}

/// Reusable in-process virtual CLI pipe supporting async stdin/stdout/stderr/readLine/wait/kill
/// semantics matching `mygo-music` spawned process handles (used by shims such as `VirtualYtdlpShim`).
pub struct VirtualPipeHandle {
    stdin_buf: Mutex<String>,
    stdin_closed: AtomicBool,
    stdin_cvar: Condvar,
    stdout_buf: Mutex<String>,
    stderr_buf: Mutex<String>,
    stdout_lines: Mutex<VecDeque<String>>,
    stdout_eof: AtomicBool,
    stdout_cvar: Condvar,
    running: AtomicBool,
    killed: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    done_cvar: Condvar,
}

impl VirtualPipeHandle {
    /// Creates an already-completed virtual pipe handle (ideal for instant `--version` probes).
    pub fn completed(stdout: String, stderr: String, exit_code: i32) -> Arc<Self> {
        let mut lines = VecDeque::new();
        for line in stdout.lines() {
            lines.push_back(line.trim_end_matches('\r').to_owned());
        }
        Arc::new(Self {
            stdin_buf: Mutex::new(String::new()),
            stdin_closed: AtomicBool::new(true),
            stdin_cvar: Condvar::new(),
            stdout_buf: Mutex::new(stdout),
            stderr_buf: Mutex::new(stderr),
            stdout_lines: Mutex::new(lines),
            stdout_eof: AtomicBool::new(true),
            stdout_cvar: Condvar::new(),
            running: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            exit_code: Mutex::new(Some(exit_code)),
            done_cvar: Condvar::new(),
        })
    }

    /// Creates a live virtual pipe handle whose background worker pushes output and calls [`Self::finish`].
    pub fn running() -> Arc<Self> {
        Arc::new(Self {
            stdin_buf: Mutex::new(String::new()),
            stdin_closed: AtomicBool::new(false),
            stdin_cvar: Condvar::new(),
            stdout_buf: Mutex::new(String::new()),
            stderr_buf: Mutex::new(String::new()),
            stdout_lines: Mutex::new(VecDeque::new()),
            stdout_eof: AtomicBool::new(false),
            stdout_cvar: Condvar::new(),
            running: AtomicBool::new(true),
            killed: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            done_cvar: Condvar::new(),
        })
    }

    pub fn write_stdin(&self, data: &str) -> Result<(), String> {
        if self.stdin_closed.load(Ordering::SeqCst) {
            return Err("stdin is already closed".to_owned());
        }
        if let Ok(mut buf) = self.stdin_buf.lock() {
            buf.push_str(data);
        }
        Ok(())
    }

    pub fn close_stdin(&self) {
        // Hold the condvar's mutex across the flag store to prevent lost wakeups in `wait_for_stdin`.
        let _guard = self.stdin_buf.lock();
        self.stdin_closed.store(true, Ordering::SeqCst);
        self.stdin_cvar.notify_all();
    }

    /// Waits until [`Self::close_stdin`], [`Self::kill`], or `timeout` elapses and returns buffered stdin.
    pub fn wait_for_stdin(&self, timeout: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + timeout;
        let Ok(mut guard) = self.stdin_buf.lock() else {
            return None;
        };
        while !self.stdin_closed.load(Ordering::SeqCst) && !self.killed.load(Ordering::SeqCst) {
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            if rem.is_zero() {
                break;
            }
            let Ok((next, _)) = self.stdin_cvar.wait_timeout(guard, rem) else {
                return None;
            };
            guard = next;
        }
        if self.killed.load(Ordering::SeqCst) {
            return None;
        }
        Some(guard.clone())
    }

    pub fn push_stdout_line(&self, line: &str) {
        if let Ok(mut buf) = self.stdout_buf.lock() {
            buf.push_str(line);
            buf.push('\n');
        }
        if let Ok(mut q) = self.stdout_lines.lock() {
            q.push_back(line.to_owned());
        }
        self.stdout_cvar.notify_all();
    }

    pub fn push_stderr(&self, msg: &str) {
        if let Ok(mut buf) = self.stderr_buf.lock() {
            buf.push_str(msg);
            if !msg.ends_with('\n') {
                buf.push('\n');
            }
        }
    }

    pub fn finish(&self, code: i32) {
        {
            // Hold `stdout_lines` across `stdout_eof` store to prevent lost wakeups in `read_line_blocking`.
            let _q = self.stdout_lines.lock();
            self.stdout_eof.store(true, Ordering::SeqCst);
            self.stdout_cvar.notify_all();
        }
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.exit_code.lock() {
            if guard.is_none() {
                *guard = Some(code);
            }
            self.done_cvar.notify_all();
        }
    }

    pub fn read_line_blocking(&self) -> Option<String> {
        let Ok(mut q) = self.stdout_lines.lock() else {
            return None;
        };
        loop {
            if let Some(line) = q.pop_front() {
                return Some(line);
            }
            if self.stdout_eof.load(Ordering::SeqCst) {
                return None;
            }
            q = match self.stdout_cvar.wait(q) {
                Ok(next) => next,
                Err(_) => return None,
            };
        }
    }

    pub fn stdout(&self) -> String {
        self.stdout_buf
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn stderr(&self) -> String {
        self.stderr_buf
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn error_output(&self) -> String {
        self.stderr()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// Exposes the underlying cancellation atomic flag so in-process shims can abort
    /// in-flight async work immediately when [`Self::kill`] is called.
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.killed
    }

    pub fn wait(&self) -> i32 {
        let Ok(mut guard) = self.exit_code.lock() else {
            return -1;
        };
        while guard.is_none() {
            guard = match self.done_cvar.wait(guard) {
                Ok(next) => next,
                Err(_) => return -1,
            };
        }
        guard.unwrap_or(-1)
    }

    pub fn kill(&self) {
        {
            let _guard = self.stdin_buf.lock();
            self.killed.store(true, Ordering::SeqCst);
            self.stdin_closed.store(true, Ordering::SeqCst);
            self.stdin_cvar.notify_all();
        }
        self.finish(-1);
    }
}

/// Opaque managed process handle owned by [`HostContext`] (backed by either an in-process
/// virtual shim such as `VirtualFfmpegShim` / `VirtualYtdlpShim` or a desktop external subprocess).
pub struct ManagedProcess {
    inner: ManagedProcessInner,
}

enum ManagedProcessInner {
    VirtualFfmpeg(VirtualProcessHandle, Mutex<usize>),
    VirtualPipe(Arc<VirtualPipeHandle>),
    External(ExternalProcessHandle),
}

impl ManagedProcess {
    /// Wraps an in-process [`VirtualProcessHandle`] produced by `VirtualFfmpegShim`.
    pub fn from_virtual_ffmpeg(handle: VirtualProcessHandle, line_cursor: Mutex<usize>) -> Self {
        Self {
            inner: ManagedProcessInner::VirtualFfmpeg(handle, line_cursor),
        }
    }

    /// Wraps an in-process [`VirtualPipeHandle`] produced by pipe-based virtual CLI shims.
    pub fn from_virtual_pipe(handle: Arc<VirtualPipeHandle>) -> Self {
        Self {
            inner: ManagedProcessInner::VirtualPipe(handle),
        }
    }

    pub(crate) fn from_external(handle: ExternalProcessHandle) -> Self {
        Self {
            inner: ManagedProcessInner::External(handle),
        }
    }

    fn write_stdin(&self, data: &str) -> Result<(), String> {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(_, _) => Ok(()),
            ManagedProcessInner::VirtualPipe(p) => p.write_stdin(data),
            ManagedProcessInner::External(h) => h.write_stdin(data),
        }
    }

    fn close_stdin(&self) {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(_, _) => {}
            ManagedProcessInner::VirtualPipe(p) => p.close_stdin(),
            ManagedProcessInner::External(h) => h.close_stdin(),
        }
    }

    fn read_line_blocking(&self) -> Option<String> {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, cursor_mutex) => loop {
                let text = h.stdout();
                let mut cursor = cursor_mutex.lock().ok()?;
                let start = (*cursor).min(text.len());
                if let Some(pos) = text[start..].find('\n') {
                    let line = text[start..start + pos].trim_end_matches('\r').to_owned();
                    *cursor = start + pos + 1;
                    return Some(line);
                }
                if !h.is_running() {
                    if start < text.len() {
                        let line = text[start..].trim_end_matches('\r').to_owned();
                        *cursor = text.len();
                        return Some(line);
                    }
                    return None;
                }
                drop(cursor);
                std::thread::sleep(Duration::from_millis(15));
            },
            ManagedProcessInner::VirtualPipe(p) => p.read_line_blocking(),
            ManagedProcessInner::External(h) => h.read_line_blocking(),
        }
    }

    fn stdout(&self) -> String {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.stdout(),
            ManagedProcessInner::VirtualPipe(p) => p.stdout(),
            ManagedProcessInner::External(h) => h.stdout(),
        }
    }

    fn stderr(&self) -> String {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.stderr(),
            ManagedProcessInner::VirtualPipe(p) => p.stderr(),
            ManagedProcessInner::External(h) => h.stderr(),
        }
    }

    fn error_output(&self) -> String {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.error_output(),
            ManagedProcessInner::VirtualPipe(p) => p.error_output(),
            ManagedProcessInner::External(h) => h.stderr(),
        }
    }

    fn is_running(&self) -> bool {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.is_running(),
            ManagedProcessInner::VirtualPipe(p) => p.is_running(),
            ManagedProcessInner::External(h) => h.is_running(),
        }
    }

    fn wait(&self) -> i32 {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.wait(),
            ManagedProcessInner::VirtualPipe(p) => p.wait(),
            ManagedProcessInner::External(h) => h.wait(),
        }
    }

    fn kill(&self) {
        match &self.inner {
            ManagedProcessInner::VirtualFfmpeg(h, _) => h.kill(),
            ManagedProcessInner::VirtualPipe(p) => p.kill(),
            ManagedProcessInner::External(h) => h.kill(),
        }
    }
}

/// Process launch options passed from JS `host.spawn(cmd, args, opts)`.
#[derive(Debug, Default, Clone)]
pub struct SpawnOptions {
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct FetchOptions {
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
    follow_redirects: bool,
}

struct FetchResponseData {
    ok: bool,
    status: u16,
    status_text: String,
    url: String,
    headers: HashMap<String, String>,
    set_cookies: Vec<String>,
    bytes: Vec<u8>,
}

/// Shared host state bridged into a plugin's isolated QuickJS VM as `globalThis.host`.
pub struct HostContext {
    pub plugin_id: String,
    pub plugin_dir: PathBuf,
    pub data_dir: PathBuf,
    pub manifest: Arc<PluginManifest>,
    pub locales: Arc<PluginLocales>,
    pub header_resolver: RequestHeadersResolver,
    locale: RwLock<String>,
    storage: RwLock<HashMap<String, serde_json::Value>>,
    config: RwLock<HashMap<String, String>>,
    http_client: reqwest::Client,
    no_redirect_client: reqwest::Client,
    sign_in_prompt: RwLock<Option<SignInPromptCallback>>,
    web_sign_in_handler: RwLock<Option<WebSignInHandler>>,
    open_url_handler: RwLock<Option<OpenUrlHandler>>,
    virtualizer: MobileProcessVirtualizer,
    is_mobile: bool,
    platform: String,
    mobile_virtual_compat: AtomicBool,
    last_runtime_error: RwLock<Option<String>>,
    last_ytdlp_exhausted: Arc<Mutex<Option<(String, Instant)>>>,
    granted: AtomicBool,
    next_proc_id: AtomicU64,
    processes: Mutex<HashMap<u64, Arc<ManagedProcess>>>,
    rescue_relay_ids: Mutex<VecDeque<u64>>,
    abort_tokens: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
}

impl HostContext {
    /// Creates a new `HostContext`, loading persisted storage, settings, and grant state for `manifest.id`.
    pub fn new(
        plugin_dir: PathBuf,
        data_dir: PathBuf,
        manifest: Arc<PluginManifest>,
        locales: Arc<PluginLocales>,
        options: HostRuntimeOptions,
    ) -> Self {
        let plugin_data_dir = data_dir.join(&manifest.id);
        let _ = fs::create_dir_all(&plugin_data_dir);

        let storage = load_json_map::<serde_json::Value>(&plugin_data_dir.join("storage.json"));
        let settings_path = plugin_data_dir.join("settings.json");
        let config = if settings_path.is_file() {
            load_json_map::<String>(&settings_path)
        } else {
            // Backwards-compatible fallback for older `config.json` files.
            load_json_map::<String>(&plugin_data_dir.join("config.json"))
        };
        let persisted_granted =
            load_json_value::<bool>(&plugin_data_dir.join("granted.json")).unwrap_or(true);

        let header_resolver = RequestHeadersResolver::new(manifest.request_headers.clone());

        let http_client = reqwest::Client::builder()
            .user_agent("nsh-music/0.1")
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Some(Duration::from_secs(30)))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let no_redirect_client = reqwest::Client::builder()
            .user_agent("nsh-music/0.1")
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Some(Duration::from_secs(30)))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let is_mobile = options.is_mobile_override.unwrap_or_else(detect_is_mobile);
        let platform = options
            .platform_override
            .unwrap_or_else(|| detect_platform_name(is_mobile).to_owned());

        let locale = options.locale.unwrap_or_else(|| "zh-CN".to_owned());

        Self {
            plugin_id: manifest.id.clone(),
            plugin_dir,
            data_dir: plugin_data_dir,
            manifest,
            locales,
            header_resolver,
            locale: RwLock::new(locale),
            storage: RwLock::new(storage),
            config: RwLock::new(config),
            http_client,
            no_redirect_client,
            sign_in_prompt: RwLock::new(options.sign_in_prompt),
            web_sign_in_handler: RwLock::new(options.web_sign_in_handler),
            open_url_handler: RwLock::new(options.open_url_handler),
            virtualizer: MobileProcessVirtualizer::with_default_shims(),
            is_mobile,
            platform,
            mobile_virtual_compat: AtomicBool::new(options.mobile_virtual_compat),
            last_runtime_error: RwLock::new(None),
            last_ytdlp_exhausted: Arc::new(Mutex::new(None)),
            granted: AtomicBool::new(persisted_granted),
            next_proc_id: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
            rescue_relay_ids: Mutex::new(VecDeque::new()),
            abort_tokens: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub(crate) fn ytdlp_exhausted_tracker(&self) -> Arc<Mutex<Option<(String, Instant)>>> {
        Arc::clone(&self.last_ytdlp_exhausted)
    }

    pub fn record_ytdlp_exhausted(&self, video_id: &str) {
        if let Ok(mut guard) = self.last_ytdlp_exhausted.lock() {
            *guard = Some((video_id.to_owned(), Instant::now()));
        }
    }

    pub fn clear_ytdlp_exhausted(&self) {
        if let Ok(mut guard) = self.last_ytdlp_exhausted.lock() {
            *guard = None;
        }
    }

    pub fn was_ytdlp_recently_exhausted(&self, video_id: &str, within: Duration) -> bool {
        self.last_ytdlp_exhausted
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .is_some_and(|(vid, at)| vid == video_id && at.elapsed() <= within)
    }

    pub fn has_web_sign_in_handler(&self) -> bool {
        self.web_sign_in_handler
            .read()
            .ok()
            .is_some_and(|g| g.is_some())
    }

    pub fn set_granted(&self, granted: bool) {
        self.granted.store(granted, Ordering::SeqCst);
        save_json_value(&self.data_dir.join("granted.json"), &granted);
    }

    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::SeqCst)
    }

    pub fn is_mobile(&self) -> bool {
        self.is_mobile
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn mobile_virtual_compat_enabled(&self) -> bool {
        self.mobile_virtual_compat.load(Ordering::SeqCst)
    }

    pub fn set_mobile_virtual_compat(&self, enabled: bool) {
        self.mobile_virtual_compat.store(enabled, Ordering::SeqCst);
    }

    pub fn last_runtime_error(&self) -> Option<String> {
        self.last_runtime_error.read().ok().and_then(|g| g.clone())
    }

    pub fn record_runtime_error(&self, err: impl Into<String>) {
        let msg = err.into();
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(mut g) = self.last_runtime_error.write() {
            *g = Some(trimmed.to_owned());
        }
    }

    pub fn clear_runtime_error(&self) {
        if let Ok(mut g) = self.last_runtime_error.write() {
            *g = None;
        }
    }

    pub fn locale(&self) -> String {
        self.locale
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "zh-CN".to_owned())
    }

    pub fn set_locale(&self, locale: &str) {
        if let Ok(mut g) = self.locale.write() {
            *g = locale.to_owned();
        }
    }

    pub fn set_sign_in_prompt(&self, cb: Option<SignInPromptCallback>) {
        if let Ok(mut g) = self.sign_in_prompt.write() {
            *g = cb;
        }
    }

    pub fn set_web_sign_in_handler(&self, handler: Option<WebSignInHandler>) {
        if let Ok(mut g) = self.web_sign_in_handler.write() {
            *g = handler;
        }
    }

    pub fn set_open_url_handler(&self, handler: Option<OpenUrlHandler>) {
        if let Ok(mut g) = self.open_url_handler.write() {
            *g = handler;
        }
    }

    pub fn open_url_handler(&self) -> Option<OpenUrlHandler> {
        self.open_url_handler.read().ok().and_then(|g| g.clone())
    }

    pub fn get_config(&self, key: &str) -> Option<String> {
        self.config.read().ok()?.get(key).cloned()
    }

    pub fn set_config(&self, key: &str, value: &str) {
        if let Ok(mut g) = self.config.write() {
            g.insert(key.to_owned(), value.to_owned());
            let snapshot = g.clone();
            drop(g);
            save_json_map(&self.data_dir.join("settings.json"), &snapshot);
        }
    }

    pub fn all_config(&self) -> HashMap<String, String> {
        self.config.read().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn cookie_value(&self) -> Option<String> {
        if let Some(c) = self.get_config("cookie").filter(|s| !s.is_empty()) {
            return Some(c);
        }
        self.storage
            .read()
            .ok()
            .and_then(|s| s.get("cookie").and_then(|v| v.as_str()).map(str::to_owned))
            .filter(|s| !s.is_empty())
    }

    pub fn all_storage(&self) -> HashMap<String, serde_json::Value> {
        self.storage.read().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn resolve_request_headers(
        &self,
        url: &str,
        kind: HeaderRequestKind,
    ) -> Vec<(String, String)> {
        if self.manifest.has_declared_network_policy() {
            let Ok(parsed) = url::Url::parse(url) else {
                return Vec::new();
            };
            if !matches!(parsed.scheme(), "http" | "https") {
                return Vec::new();
            }
            let host_clean = parsed
                .host_str()
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '[' || c == ']')
                .to_ascii_lowercase();
            if host_clean.is_empty() {
                return Vec::new();
            }
            if !self.manifest.declares_wildcard_network() {
                let declared = self.manifest.declared_network_hosts();
                if !declared
                    .iter()
                    .any(|pat| network_host_matches(pat, &host_clean))
                {
                    return Vec::new();
                }
            }
        }
        let cfg = self.all_config();
        let storage = self.all_storage();
        self.header_resolver
            .resolve_with_storage(url, kind, &cfg, &storage)
    }

    pub fn get_storage(&self, key: &str) -> Option<serde_json::Value> {
        self.storage.read().ok()?.get(key).cloned()
    }

    pub fn set_storage(&self, key: &str, value: serde_json::Value) {
        if let Ok(mut g) = self.storage.write() {
            g.insert(key.to_owned(), value);
            let snapshot = g.clone();
            drop(g);
            save_json_map(&self.data_dir.join("storage.json"), &snapshot);
        }
    }

    pub fn remove_storage(&self, key: &str) {
        if let Ok(mut g) = self.storage.write() {
            g.remove(key);
            let snapshot = g.clone();
            drop(g);
            save_json_map(&self.data_dir.join("storage.json"), &snapshot);
        }
    }

    pub fn clear_storage(&self) {
        if let Ok(mut g) = self.storage.write() {
            g.clear();
            let snapshot = g.clone();
            drop(g);
            save_json_map(&self.data_dir.join("storage.json"), &snapshot);
        }
    }

    pub fn storage_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .storage
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    pub fn emit_sign_in_prompt(&self, prompt: SignInPrompt) {
        if let Ok(guard) = self.sign_in_prompt.read()
            && let Some(cb) = guard.as_ref()
        {
            cb(prompt);
        }
    }

    /// Aborts any in-flight `fetch` requests or `setTimeout` timers cleanly on shutdown.
    pub fn cancel_all_async(&self) {
        if let Ok(mut map) = self.abort_tokens.lock() {
            for (_, tx) in map.drain() {
                let _ = tx.send(());
            }
        }
    }

    fn register_abort_token(&self, req_id: u64) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut map) = self.abort_tokens.lock() {
            map.insert(req_id, tx);
        }
        rx
    }

    pub fn trigger_abort_token(&self, req_id: u64) {
        if let Ok(mut map) = self.abort_tokens.lock()
            && let Some(tx) = map.remove(&req_id)
        {
            let _ = tx.send(());
        }
    }

    fn clear_abort_token(&self, req_id: u64) {
        if let Ok(mut map) = self.abort_tokens.lock() {
            map.remove(&req_id);
        }
    }

    /// Registers a rescue `VirtualFfmpegShim` loopback relay in `self.processes` so shutdown
    /// cleans it up, trimming and killing older rescue relays beyond 3 to prevent socket leaks.
    pub(crate) fn register_rescue_relay(&self, handle: VirtualProcessHandle) -> u64 {
        const MAX_RESCUE_RELAYS: usize = 3;
        let id = self.next_proc_id.fetch_add(1, Ordering::Relaxed);
        let managed = Arc::new(ManagedProcess::from_virtual_ffmpeg(handle, Mutex::new(0)));
        if let Ok(mut map) = self.processes.lock() {
            map.insert(id, managed);
        }

        let mut stale_ids = Vec::new();
        if let Ok(mut queue) = self.rescue_relay_ids.lock() {
            queue.push_back(id);
            while queue.len() > MAX_RESCUE_RELAYS {
                if let Some(old_id) = queue.pop_front() {
                    stale_ids.push(old_id);
                }
            }
        }

        if !stale_ids.is_empty() {
            let stale_procs: Vec<Arc<ManagedProcess>> = self
                .processes
                .lock()
                .ok()
                .map(|mut map| {
                    stale_ids
                        .into_iter()
                        .filter_map(|old_id| map.remove(&old_id))
                        .collect()
                })
                .unwrap_or_default();
            for proc_handle in stale_procs {
                proc_handle.kill();
            }
        }

        id
    }

    #[cfg(test)]
    pub(crate) fn active_process_count(&self) -> usize {
        self.processes
            .lock()
            .ok()
            .map(|map| map.values().filter(|p| p.is_running()).count())
            .unwrap_or(0)
    }

    /// Terminates all active spawned processes (both virtual ffmpeg relays and desktop child processes).
    pub fn kill_all_processes(&self) {
        self.cancel_all_async();
        if let Ok(mut queue) = self.rescue_relay_ids.lock() {
            queue.clear();
        }
        let procs: Vec<Arc<ManagedProcess>> = self
            .processes
            .lock()
            .ok()
            .map(|mut map| map.drain().map(|(_, p)| p).collect())
            .unwrap_or_default();
        for proc_handle in procs {
            proc_handle.kill();
        }
    }

    /// Returns `true` if `host` is permitted by `permissions.network` (optionally including
    /// loopback hosts `127.0.0.1` / `localhost` when the plugin declares process permissions).
    pub(crate) fn is_network_host_allowed(
        &self,
        host: &str,
        allow_loopback_for_process: bool,
    ) -> bool {
        let host_clean = host
            .trim()
            .trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase();
        if host_clean.is_empty() {
            return false;
        }
        let mut patterns = self.manifest.all_network_hosts();
        // Also check wildcard `*` explicitly declared in `permissions.network` / `domains`.
        let has_star = self
            .manifest
            .permissions
            .network
            .iter()
            .chain(self.manifest.domains.iter())
            .any(|h| h.trim() == "*");
        if has_star {
            return true;
        }
        if allow_loopback_for_process
            && !self.manifest.all_process_commands().is_empty()
            && (self.is_granted() || (self.is_mobile && self.mobile_virtual_compat_enabled()))
        {
            patterns.push("127.0.0.1".to_owned());
            patterns.push("localhost".to_owned());
            patterns.push("::1".to_owned());
        }
        patterns
            .iter()
            .any(|pat| network_host_matches(pat, &host_clean))
    }

    /// Validates a `WebSignInRequest` against `permissions.network` and HTTPS requirements
    /// (matching `mygo-music`'s `checkWebSignIn`).
    pub(crate) fn check_web_sign_in(&self, req: &WebSignInRequest) -> Result<(), String> {
        let has_any_network = !self.manifest.all_network_hosts().is_empty()
            || self
                .manifest
                .permissions
                .network
                .iter()
                .chain(self.manifest.domains.iter())
                .any(|h| h.trim() == "*");
        if !has_any_network {
            return Err("webSignIn needs the network permission".to_owned());
        }
        let sign_in_url = url::Url::parse(req.url.trim())
            .map_err(|_| format!("not a secure web address to sign in on: {}", req.url))?;
        if sign_in_url.scheme() != "https" || sign_in_url.host_str().unwrap_or("").is_empty() {
            return Err(format!(
                "not a secure web address to sign in on: {}",
                req.url
            ));
        }
        if let Some(ref cookies_of) = req.cookies_of {
            let co_url = url::Url::parse(cookies_of.trim())
                .map_err(|_| format!("cookiesOf isn't a secure web address: {cookies_of}"))?;
            let host = co_url.host_str().unwrap_or("");
            if co_url.scheme() != "https" || host.is_empty() {
                return Err(format!(
                    "cookiesOf isn't a secure web address: {cookies_of}"
                ));
            }
            if !self.is_network_host_allowed(host, false) {
                return Err(format!(
                    "the plugin can't reach {host}, so it can't have its cookies"
                ));
            }
        }
        Ok(())
    }

    async fn perform_fetch(
        &self,
        url: &str,
        opts: FetchOptions,
        mut abort_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<FetchResponseData, String> {
        let parsed_url = url::Url::parse(url).map_err(|e| format!("invalid URL `{url}`: {e}"))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(format!("unsupported URL scheme for `{url}`"));
        }
        let host = parsed_url.host_str().unwrap_or("");
        if !self.is_network_host_allowed(host, true) {
            return Err(format!(
                "host `{host}` is not permitted by plugin network permission"
            ));
        }

        let method_str = if opts.method.is_empty() {
            "GET"
        } else {
            opts.method.as_str()
        };
        let method = reqwest::Method::from_bytes(method_str.as_bytes())
            .map_err(|e| format!("invalid HTTP method `{method_str}`: {e}"))?;

        let client = if opts.follow_redirects {
            &self.http_client
        } else {
            &self.no_redirect_client
        };

        // Apply plugin manifest requestHeaders first, then let explicit opts.headers override.
        let mut merged_headers = self.resolve_request_headers(url, HeaderRequestKind::Api);
        for (k, v) in opts.headers {
            if let Some(existing) = merged_headers
                .iter_mut()
                .find(|(ek, _)| ek.eq_ignore_ascii_case(&k))
            {
                existing.0 = k;
                existing.1 = v;
            } else {
                merged_headers.push((k, v));
            }
        }
        let body_bytes = opts.body;
        let timeout_ms = opts.timeout_ms;

        let build_req = |headers_list: &[(String, String)], body: Option<&[u8]>| {
            let mut req = client.request(method.clone(), url);
            for (k, v) in headers_list {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(bytes) = body {
                req = req.body(bytes.to_vec());
            }
            if let Some(ms) = timeout_ms {
                req = req.timeout(Duration::from_millis(ms));
            }
            req
        };

        let req = build_req(&merged_headers, body_bytes.as_deref());
        let mut resp = tokio::select! {
            _ = &mut abort_rx => {
                return Err("AbortError: The operation was aborted".to_owned());
            }
            sent = req.send() => {
                sent.map_err(|e| format!("host.fetch failed for `{url}`: {e}"))?
            }
        };

        // YouTube Music's `ANDROID_MUSIC` InnerTube client rejects web `Cookie` / `SAPISIDHASH`
        // auth headers with HTTP 400 on `/youtubei/v1/next` and `/youtubei/v1/browse` (lyrics).
        // Retry once anonymously with web auth/origin headers stripped so signed-in plugins
        // receive `MPLY*` timed lyrics cleanly without patching plugin JS.
        let innertube_endpoint = if parsed_url.path().ends_with("/youtubei/v1/next") {
            Some("/next")
        } else if parsed_url.path().ends_with("/youtubei/v1/browse") {
            Some("/browse")
        } else {
            None
        };
        if resp.status() == reqwest::StatusCode::BAD_REQUEST
            && method == reqwest::Method::POST
            && let Some(endpoint_label) = innertube_endpoint
            && body_bytes
                .as_deref()
                .is_some_and(|b| contains_subslice(b, b"ANDROID_MUSIC"))
            && merged_headers
                .iter()
                .any(|(k, _)| is_innertube_web_auth_header(k))
        {
            log::info!(
                target: "nsh_music_plugin",
                "retrying ANDROID_MUSIC InnerTube {endpoint_label} anonymously after HTTP 400 for plugin `{}`",
                self.plugin_id
            );
            let anon_headers: Vec<(String, String)> = merged_headers
                .into_iter()
                .filter(|(k, _)| !is_innertube_web_auth_header(k))
                .collect();
            let retry_req = build_req(&anon_headers, body_bytes.as_deref());
            let retry_res = tokio::select! {
                _ = &mut abort_rx => {
                    return Err("AbortError: The operation was aborted".to_owned());
                }
                sent = retry_req.send() => sent,
            };
            if let Ok(retry_resp) = retry_res
                && retry_resp.status().is_success()
            {
                resp = retry_resp;
            }
        }

        let status = resp.status();
        let final_url = resp.url().to_string();
        let mut headers = HashMap::new();
        for (k, v) in resp.headers() {
            if let Ok(val) = v.to_str() {
                let key = k.as_str().to_ascii_lowercase();
                headers
                    .entry(key)
                    .and_modify(|existing: &mut String| {
                        existing.push_str(", ");
                        existing.push_str(val);
                    })
                    .or_insert_with(|| val.to_owned());
            }
        }
        let set_cookies = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();

        let bytes = tokio::select! {
            _ = &mut abort_rx => {
                return Err("AbortError: The operation was aborted".to_owned());
            }
            body = resp.bytes() => {
                body.map_err(|e| format!("failed to read response body for `{url}`: {e}"))?
                    .to_vec()
            }
        };

        Ok(FetchResponseData {
            ok: status.is_success(),
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or("").to_owned(),
            url: final_url,
            headers,
            set_cookies,
            bytes,
        })
    }

    fn spawn_process(
        &self,
        cmd: &str,
        args: &[String],
        opts: &SpawnOptions,
    ) -> Result<(u64, bool, Arc<ManagedProcess>), Result<MobileSandboxError, String>> {
        let virtual_compat = self.mobile_virtual_compat_enabled();

        if virtual_compat {
            // 1. Consult registered virtual process shims (e.g. `ffmpeg`, `yt-dlp`).
            match self.virtualizer.spawn(cmd, args, opts, self) {
                Ok(Some((is_virtual, proc))) => {
                    self.clear_runtime_error();
                    let id = self.next_proc_id.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut map) = self.processes.lock() {
                        map.insert(id, Arc::clone(&proc));
                    }
                    return Ok((id, is_virtual, proc));
                }
                Ok(None) => {}
                Err(err) => {
                    if let Err(ref msg) = err {
                        self.record_runtime_error(msg);
                    }
                    return Err(err);
                }
            }

            // 2. Non-virtualized binaries are blocked on mobile sandboxes (without `"allowed"` in error text).
            if self.is_mobile {
                return Err(Ok(MobileSandboxError::new(cmd, &self.platform)));
            }
        }

        // 3. Enforce `permissions.process` & Allow Programs matching `mygo-music`.
        if self.manifest.all_process_commands().is_empty() {
            let msg = "spawn needs the process permission".to_owned();
            if !virtual_compat {
                self.record_runtime_error(&msg);
            }
            return Err(Err(msg));
        }
        if !self.granted.load(Ordering::SeqCst) {
            let msg = "the listener hasn't allowed this plugin to start programs: enable Allow Programs in plugin settings"
                .to_owned();
            if !virtual_compat {
                self.record_runtime_error(&msg);
            }
            return Err(Err(msg));
        }

        let resolved_cmd = resolve_relative_command(&self.plugin_dir, cmd);
        if !self
            .manifest
            .allows_process(&self.plugin_dir, &resolved_cmd)
        {
            let msg = format!("{cmd} isn't one of the programs in the plugin's process permission");
            if !virtual_compat {
                self.record_runtime_error(&msg);
            }
            return Err(Err(msg));
        }

        let effective_opts = with_default_cwd(opts, &self.plugin_dir);
        let handle = match ExternalProcessHandle::spawn(&resolved_cmd, args, &effective_opts) {
            Ok(handle) => handle,
            Err(err) => {
                self.record_runtime_error(&err);
                return Err(Err(err));
            }
        };
        self.clear_runtime_error();
        let proc = Arc::new(ManagedProcess::from_external(handle));
        let id = self.next_proc_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.processes.lock() {
            map.insert(id, Arc::clone(&proc));
        }
        Ok((id, false, proc))
    }

    fn run_web_sign_in(&self, req: WebSignInRequest) -> Result<WebSignInResult, String> {
        let handler = self.web_sign_in_handler.read().ok().and_then(|g| g.clone());

        if let Some(h) = handler {
            return h(req);
        }

        // Fallback to SignInPromptCallback so UI hosts without a custom WebSignInHandler
        // can still present the sign-in URL / WebView sheet and manual cookie input.
        let has_prompt = self.sign_in_prompt.read().ok().and_then(|g| g.clone());
        if let Some(cb) = has_prompt {
            let text = if req.title.is_empty() {
                format!("Sign in via embedded web view: {}", req.url)
            } else {
                format!("{}: {}", req.title, req.url)
            };
            cb(SignInPrompt {
                qr: req.url.clone(),
                text,
            });
            let existing_cookie = self.cookie_value().unwrap_or_default();
            return Ok(WebSignInResult {
                ok: !existing_cookie.is_empty(),
                url: req.url,
                cookies: HashMap::new(),
                cookie: existing_cookie,
                eval_result: None,
                cancelled: false,
                manual_fallback: true,
            });
        }

        Err(
            "host.webSignIn requires an active UI WebView handler or SignInPromptCallback"
                .to_owned(),
        )
    }
}

fn network_host_matches(pattern: &str, host_lower: &str) -> bool {
    let pat = pattern
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if pat.is_empty() {
        return false;
    }
    if pat == "*" {
        return true;
    }
    if let Some(suffix) = pat.strip_prefix("*.") {
        return host_lower == suffix || host_lower.ends_with(&format!(".{suffix}"));
    }
    if let Some(suffix) = pat.strip_prefix('.') {
        return host_lower == suffix || host_lower.ends_with(&format!(".{suffix}"));
    }
    // Strip optional scheme/path if pattern came from a requestHeaders URL glob.
    if pat.contains("://")
        && let Ok(u) = url::Url::parse(&pat)
        && let Some(h) = u.host_str()
    {
        return host_lower == h.to_ascii_lowercase();
    }
    host_lower == pat
}

fn load_json_value<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_json_value<T: Serialize>(path: &Path, val: &T) {
    if let Ok(json) = serde_json::to_string_pretty(val)
        && let Err(err) = atomic_write(path, json.as_bytes())
    {
        log::warn!(target: "nsh_music_plugin", "failed to persist {}: {err}", path.display());
    }
}

fn detect_is_mobile() -> bool {
    cfg!(any(
        target_os = "ios",
        target_os = "android",
        target_env = "ohos"
    ))
}

fn detect_platform_name(is_mobile: bool) -> &'static str {
    if cfg!(target_env = "ohos") {
        "harmonyos"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(target_os = "android") {
        "android"
    } else if is_mobile {
        "mobile"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn detect_arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        std::env::consts::ARCH
    }
}

fn load_json_map<T: for<'de> Deserialize<'de>>(path: &Path) -> HashMap<String, T> {
    let Ok(raw) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_json_map<T: Serialize>(path: &Path, map: &HashMap<String, T>) {
    if let Ok(json) = serde_json::to_string_pretty(map)
        && let Err(err) = atomic_write(path, json.as_bytes())
    {
        log::warn!(target: "nsh_music_plugin", "failed to persist {}: {err}", path.display());
    }
}

/// Injects `globalThis.host` and standard JS helper globals (`console`, `fetch`, `setTimeout`,
/// `atob`, `btoa`, `TextEncoder`, `TextDecoder`, `URL`, `URLSearchParams`, `AbortController`,
/// `structuredClone`) into `ctx`.
pub fn install_host_globals<'js>(ctx: &Ctx<'js>, state: Arc<HostContext>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let host = Object::new(ctx.clone())?;

    install_metadata(&host, &state)?;
    install_fetch(ctx, &host, &globals, Arc::clone(&state))?;
    install_storage(ctx, &host, Arc::clone(&state))?;
    install_config(ctx, &host, Arc::clone(&state))?;
    install_crypto(ctx, &host)?;
    install_log(ctx, &host, &globals, Arc::clone(&state))?;
    install_i18n(ctx, &host, Arc::clone(&state))?;
    install_capabilities(ctx, &host, &state)?;
    install_spawn(ctx, &host, Arc::clone(&state))?;
    install_web_sign_in(ctx, &host, &globals, Arc::clone(&state))?;
    install_prompt_and_sleep(ctx, &host, &globals, Arc::clone(&state))?;
    install_navigation_and_data_helpers(ctx, &host, Arc::clone(&state))?;

    globals.set("host", host)?;

    // Standard Web API shims (`URL`, `URLSearchParams`, `AbortController`, `fetch`, `setTimeout`, `structuredClone`).
    install_js_polyfills(ctx)?;
    Ok(())
}

fn install_metadata<'js>(host: &Object<'js>, state: &HostContext) -> rquickjs::Result<()> {
    host.set("platform", state.platform())?;
    host.set("arch", detect_arch_name())?;
    host.set("dir", state.plugin_dir.to_string_lossy().into_owned())?;
    host.set("dataDir", state.data_dir.to_string_lossy().into_owned())?;
    Ok(())
}

fn install_fetch<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    globals: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let st_fetch = Arc::clone(&state);
    let native_fetch = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              req_id: u64,
              url: String,
              opts: Opt<Value<'js>>|
              -> rquickjs::Result<Promise<'js>> {
            let parsed = parse_fetch_opts(&ctx, opts.0)?;
            let hc = Arc::clone(&st_fetch);
            let abort_rx = hc.register_abort_token(req_id);
            let ctx_inner = ctx.clone();
            Promise::wrap_future(&ctx, async move {
                let res = hc.perform_fetch(&url, parsed, abort_rx).await;
                hc.clear_abort_token(req_id);
                let data = res.map_err(|msg| Exception::throw_message(&ctx_inner, &msg))?;
                build_fetch_response_js(&ctx_inner, data)
            })
        },
    )?;

    let st_abort = Arc::clone(&state);
    let native_abort = Function::new(ctx.clone(), move |req_id: u64| {
        st_abort.trigger_abort_token(req_id);
    })?;

    globals.set("__nshFetch", native_fetch.clone())?;
    globals.set("__nshAbortFetch", native_abort)?;
    host.set("fetch", native_fetch.clone())?;
    globals.set("fetch", native_fetch)?;
    Ok(())
}

fn parse_fetch_opts<'js>(
    ctx: &Ctx<'js>,
    opts: Option<Value<'js>>,
) -> rquickjs::Result<FetchOptions> {
    let mut out = FetchOptions {
        method: "GET".to_owned(),
        follow_redirects: true,
        ..FetchOptions::default()
    };
    let Some(val) = opts else {
        return Ok(out);
    };
    let Some(obj) = val.into_object() else {
        return Ok(out);
    };

    if let Ok(Some(m)) = obj.get::<_, Option<String>>("method")
        && !m.is_empty()
    {
        out.method = m.to_ascii_uppercase();
    }
    if let Ok(Some(ms)) = obj.get::<_, Option<u64>>("timeout") {
        out.timeout_ms = Some(ms);
    } else if let Ok(Some(ms)) = obj.get::<_, Option<u64>>("timeoutMs") {
        out.timeout_ms = Some(ms);
    }
    if let Ok(Some(redir)) = obj.get::<_, Option<String>>("redirect")
        && redir.eq_ignore_ascii_case("manual")
    {
        out.follow_redirects = false;
    }
    if let Ok(Some(headers_val)) = obj.get::<_, Option<Value<'js>>>("headers")
        && let Some(hobj) = headers_val.into_object()
    {
        for entry in hobj.props::<String, Value<'js>>() {
            if let Ok((k, v)) = entry
                && let Some(s) = js_value_to_lossy_string(ctx, v)
            {
                out.headers.push((k, s));
            }
        }
    }
    if let Ok(Some(body_val)) = obj.get::<_, Option<Value<'js>>>("body")
        && !body_val.is_undefined()
        && !body_val.is_null()
    {
        out.body = Some(js_value_to_bytes(ctx, body_val)?);
    }

    Ok(out)
}

fn build_fetch_response_js<'js>(
    ctx: &Ctx<'js>,
    data: FetchResponseData,
) -> rquickjs::Result<Object<'js>> {
    let resp = Object::new(ctx.clone())?;
    resp.set("ok", data.ok)?;
    resp.set("status", i32::from(data.status))?;
    resp.set("statusText", data.status_text)?;
    resp.set("url", data.url)?;

    let headers_obj = Object::new(ctx.clone())?;
    for (k, v) in &data.headers {
        headers_obj.set(k.as_str(), v.as_str())?;
    }
    let headers_map = data.headers.clone();
    headers_obj.set(
        "get",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<Value<'js>> {
                match headers_map.get(&name.to_ascii_lowercase()) {
                    Some(v) => v.clone().into_js(&ctx),
                    None => Ok(Value::new_null(ctx)),
                }
            },
        )?,
    )?;
    let headers_has_map = data.headers.clone();
    headers_obj.set(
        "has",
        Function::new(ctx.clone(), move |name: String| -> bool {
            headers_has_map.contains_key(&name.to_ascii_lowercase())
        })?,
    )?;
    let set_cookies = data.set_cookies;
    headers_obj.set(
        "getSetCookie",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Array<'js>> {
                let arr = Array::new(ctx)?;
                for (idx, line) in set_cookies.iter().enumerate() {
                    arr.set(idx, line.as_str())?;
                }
                Ok(arr)
            },
        )?,
    )?;
    let headers_iter_map = data.headers;
    headers_obj.set(
        "entries",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Array<'js>> {
                let arr = Array::new(ctx.clone())?;
                for (idx, (k, v)) in headers_iter_map.iter().enumerate() {
                    let pair = Array::new(ctx.clone())?;
                    pair.set(0, k.as_str())?;
                    pair.set(1, v.as_str())?;
                    arr.set(idx, pair)?;
                }
                Ok(arr)
            },
        )?,
    )?;
    resp.set("headers", headers_obj)?;

    let body_text = String::from_utf8_lossy(&data.bytes).into_owned();
    resp.set("body", body_text.clone())?;

    let text_copy = body_text.clone();
    resp.set(
        "text",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let s = text_copy.clone();
                Promise::wrap_future(&ctx, async move { Ok::<String, JsError>(s) })
            },
        )?,
    )?;

    let json_copy = body_text;
    resp.set(
        "json",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let s = json_copy.clone();
                let c = ctx.clone();
                Promise::wrap_future(&ctx, async move {
                    c.json_parse(s.as_bytes()).map_err(|e| {
                        Exception::throw_message(&c, &format!("invalid JSON body: {e}"))
                    })
                })
            },
        )?,
    )?;

    let bytes_copy = data.bytes;
    resp.set(
        "bytes",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let b = bytes_copy.clone();
                let c = ctx.clone();
                Promise::wrap_future(&ctx, async move {
                    let arr = Array::new(c)?;
                    for (idx, byte) in b.into_iter().enumerate() {
                        arr.set(idx, i32::from(byte))?;
                    }
                    Ok::<Array<'js>, JsError>(arr)
                })
            },
        )?,
    )?;

    Ok(resp)
}

fn install_storage<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let storage = Object::new(ctx.clone())?;

    let s_get = Arc::clone(&state);
    storage.set(
        "get",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<Value<'js>> {
                match s_get.get_storage(&key) {
                    Some(v) => json_to_js(&ctx, &v),
                    None => Ok(Value::new_null(ctx)),
                }
            },
        )?,
    )?;

    let s_set = Arc::clone(&state);
    storage.set(
        "set",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, val: Value<'js>| -> rquickjs::Result<()> {
                let json = js_to_json(&ctx, val)?;
                s_set.set_storage(&key, json);
                Ok(())
            },
        )?,
    )?;

    let s_remove = Arc::clone(&state);
    let remove_fn = Function::new(ctx.clone(), move |key: String| {
        s_remove.remove_storage(&key);
    })?;
    storage.set("remove", remove_fn.clone())?;
    storage.set("delete", remove_fn)?;

    let s_keys = Arc::clone(&state);
    storage.set(
        "keys",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Array<'js>> {
                let arr = Array::new(ctx)?;
                for (idx, k) in s_keys.storage_keys().into_iter().enumerate() {
                    arr.set(idx, k)?;
                }
                Ok(arr)
            },
        )?,
    )?;

    let s_clear = Arc::clone(&state);
    storage.set(
        "clear",
        Function::new(ctx.clone(), move || {
            s_clear.clear_storage();
        })?,
    )?;

    host.set("storage", storage)?;
    Ok(())
}

fn install_config<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let cfg = Object::new(ctx.clone())?;

    let c_get = Arc::clone(&state);
    cfg.set(
        "get",
        Function::new(
            ctx.clone(),
            move |key: String, fallback: Opt<String>| -> Option<String> {
                c_get.get_config(&key).or(fallback.0)
            },
        )?,
    )?;

    let c_set = Arc::clone(&state);
    cfg.set(
        "set",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, key: String, val: Value<'js>| {
                let s = js_value_to_lossy_string(&ctx, val).unwrap_or_default();
                c_set.set_config(&key, &s);
            },
        )?,
    )?;

    let c_all = Arc::clone(&state);
    cfg.set(
        "all",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
                let obj = Object::new(ctx)?;
                for (k, v) in c_all.all_config() {
                    obj.set(k, v)?;
                }
                Ok(obj)
            },
        )?,
    )?;

    let c_setting = Arc::clone(&state);
    host.set(
        "setting",
        Function::new(ctx.clone(), move |key: String| -> String {
            c_setting.get_config(&key).unwrap_or_default()
        })?,
    )?;

    host.set("config", cfg.clone())?;
    host.set("settings", cfg)?;
    Ok(())
}

fn install_crypto<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let cobj = Object::new(ctx.clone())?;

    cobj.set(
        "md5",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: Value<'js>| -> rquickjs::Result<String> {
                let bytes = js_value_to_bytes(&ctx, data)?;
                Ok(crypto::md5_hex(&bytes))
            },
        )?,
    )?;

    cobj.set(
        "sha256",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: Value<'js>| -> rquickjs::Result<String> {
                let bytes = js_value_to_bytes(&ctx, data)?;
                Ok(crypto::sha256_hex(&bytes))
            },
        )?,
    )?;

    let hmac_fn = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, key: Value<'js>, data: Value<'js>| -> rquickjs::Result<String> {
            let key_bytes = js_value_to_bytes(&ctx, key)?;
            let data_bytes = js_value_to_bytes(&ctx, data)?;
            Ok(crypto::hmac_sha256_hex(&key_bytes, &data_bytes))
        },
    )?;
    cobj.set("hmacSha256", hmac_fn.clone())?;
    cobj.set("hmac_sha256", hmac_fn)?;

    // Supports both `mygo-music`'s 2-arg `aesEcbEncrypt(key, text)` (hex output) AND
    // legacy 3-arg `aesEcbEncrypt(plaintext, key, encoding)`.
    let aes_ecb_enc = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         arg0: Value<'js>,
         arg1: Value<'js>,
         encoding: Opt<String>|
         -> rquickjs::Result<String> {
            let b0 = js_value_to_bytes(&ctx, arg0)?;
            let b1 = js_value_to_bytes(&ctx, arg1)?;
            let (pt, key_bytes) = if encoding.0.is_some() {
                (b0, resolve_aes_key_from_raw(b1))
            } else {
                let k0 = try_parse_aes_key(&b0);
                let k1 = try_parse_aes_key(&b1);
                match (k0, k1) {
                    (Some(key), None) => (b1, key),
                    (None, Some(key)) => (b0, key),
                    (Some(key), Some(_)) => (b1, key),
                    (None, None) => (b1, b0),
                }
            };
            let ct = crypto::aes_ecb_encrypt(&pt, &key_bytes)
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
            encode_output_bytes(&ct, encoding.0.as_deref())
        },
    )?;
    cobj.set("aesEcbEncrypt", aes_ecb_enc.clone())?;
    cobj.set("aes_ecb_encrypt", aes_ecb_enc)?;

    let aes_ecb_dec = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         ciphertext: String,
         key: Value<'js>,
         encoding: Opt<String>|
         -> rquickjs::Result<String> {
            let ct = decode_input_bytes(&ciphertext, encoding.0.as_deref())
                .map_err(|e| Exception::throw_message(&ctx, &e))?;
            let key_bytes = resolve_aes_key_bytes(&ctx, key)?;
            let pt = crypto::aes_ecb_decrypt(&ct, &key_bytes)
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
            Ok(String::from_utf8_lossy(&pt).into_owned())
        },
    )?;
    cobj.set("aesEcbDecrypt", aes_ecb_dec.clone())?;
    cobj.set("aes_ecb_decrypt", aes_ecb_dec)?;

    let aes_cbc_enc = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         plaintext: Value<'js>,
         key: Value<'js>,
         iv: Value<'js>,
         encoding: Opt<String>|
         -> rquickjs::Result<String> {
            let pt = js_value_to_bytes(&ctx, plaintext)?;
            let key_bytes = resolve_aes_key_bytes(&ctx, key)?;
            let iv_bytes = resolve_iv_bytes(&ctx, iv)?;
            let ct = crypto::aes_cbc_encrypt(&pt, &key_bytes, &iv_bytes)
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
            encode_output_bytes(&ct, encoding.0.as_deref())
        },
    )?;
    cobj.set("aesCbcEncrypt", aes_cbc_enc)?;

    let aes_cbc_dec = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         ciphertext: String,
         key: Value<'js>,
         iv: Value<'js>,
         encoding: Opt<String>|
         -> rquickjs::Result<String> {
            let ct = decode_input_bytes(&ciphertext, encoding.0.as_deref())
                .map_err(|e| Exception::throw_message(&ctx, &e))?;
            let key_bytes = resolve_aes_key_bytes(&ctx, key)?;
            let iv_bytes = resolve_iv_bytes(&ctx, iv)?;
            let pt = crypto::aes_cbc_decrypt(&ct, &key_bytes, &iv_bytes)
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
            Ok(String::from_utf8_lossy(&pt).into_owned())
        },
    )?;
    cobj.set("aesCbcDecrypt", aes_cbc_dec)?;

    let b64_enc = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, data: Value<'js>| -> rquickjs::Result<String> {
            let bytes = js_value_to_bytes(&ctx, data)?;
            Ok(crypto::base64_encode(&bytes))
        },
    )?;
    cobj.set("base64Encode", b64_enc.clone())?;
    cobj.set("base64_encode", b64_enc)?;

    let b64_dec = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, data: String| -> rquickjs::Result<String> {
            let bytes = crypto::base64_decode(&data)
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        },
    )?;
    cobj.set("base64Decode", b64_dec.clone())?;
    cobj.set("base64_decode", b64_dec)?;

    cobj.set(
        "base64UrlEncode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: Value<'js>| -> rquickjs::Result<String> {
                let bytes = js_value_to_bytes(&ctx, data)?;
                Ok(crypto::base64_url_encode(&bytes))
            },
        )?,
    )?;

    cobj.set(
        "base64UrlDecode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: String| -> rquickjs::Result<String> {
                let bytes = crypto::base64_decode(&data)
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            },
        )?,
    )?;

    cobj.set(
        "hexEncode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: Value<'js>| -> rquickjs::Result<String> {
                let bytes = js_value_to_bytes(&ctx, data)?;
                Ok(crypto::hex_encode(&bytes))
            },
        )?,
    )?;

    cobj.set(
        "hexDecode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, data: String| -> rquickjs::Result<String> {
                let bytes = crypto::hex_decode(&data)
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            },
        )?,
    )?;

    host.set("crypto", cobj)?;
    Ok(())
}

fn install_log<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    globals: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let log_obj = Object::new(ctx.clone())?;

    let make_logger = |level: log::Level| -> rquickjs::Result<Function<'js>> {
        let pid = state.plugin_id.clone();
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: Rest<Value<'js>>| {
            let parts: Vec<String> = args.0.into_iter().map(|v| format_js_arg(&ctx, v)).collect();
            let line = parts.join(" ");
            match level {
                log::Level::Debug => log::debug!(target: "plugin", "[{pid}] {line}"),
                log::Level::Info => log::info!(target: "plugin", "[{pid}] {line}"),
                log::Level::Warn => log::warn!(target: "plugin", "[{pid}] {line}"),
                _ => log::error!(target: "plugin", "[{pid}] {line}"),
            }
        })
    };

    let debug_fn = make_logger(log::Level::Debug)?;
    let info_fn = make_logger(log::Level::Info)?;
    let warn_fn = make_logger(log::Level::Warn)?;
    let error_fn = make_logger(log::Level::Error)?;

    log_obj.set("debug", debug_fn.clone())?;
    log_obj.set("info", info_fn.clone())?;
    log_obj.set("warn", warn_fn.clone())?;
    log_obj.set("error", error_fn.clone())?;
    host.set("log", log_obj)?;

    let console = Object::new(ctx.clone())?;
    console.set("log", info_fn.clone())?;
    console.set("debug", debug_fn)?;
    console.set("info", info_fn)?;
    console.set("warn", warn_fn)?;
    console.set("error", error_fn)?;
    globals.set("console", console)?;
    Ok(())
}

fn install_i18n<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let i18n = Object::new(ctx.clone())?;

    let s_t = Arc::clone(&state);
    let t_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, params: Opt<Value<'js>>| -> String {
            let locale = s_t.locale();
            let mut map = HashMap::new();
            if let Some(pobj) = params.0.and_then(|v| v.into_object()) {
                for entry in pobj.props::<String, Value<'js>>() {
                    if let Ok((k, v)) = entry
                        && let Some(s) = js_value_to_lossy_string(&ctx, v)
                    {
                        map.insert(k, s);
                    }
                }
            }
            s_t.locales.translate_with_params(&locale, &key, &map)
        },
    )?;

    let s_loc = Arc::clone(&state);
    let lang_fn = Function::new(ctx.clone(), move || -> String { s_loc.locale() })?;

    i18n.set("t", t_fn.clone())?;
    i18n.set("locale", lang_fn.clone())?;

    host.set("t", t_fn)?;
    host.set("language", lang_fn)?;
    host.set("i18n", i18n)?;
    Ok(())
}

fn install_capabilities<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: &HostContext,
) -> rquickjs::Result<()> {
    let caps = Object::new(ctx.clone())?;
    let is_mobile = state.is_mobile();
    let virtual_compat = state.mobile_virtual_compat_enabled();
    caps.set("platform", state.platform())?;
    caps.set("isMobile", is_mobile)?;
    caps.set("mobileVirtualCompat", virtual_compat)?;
    caps.set("embeddedFfmpeg", virtual_compat)?;
    caps.set("embeddedYtdlp", virtual_compat)?;
    caps.set("canSpawnExternal", !is_mobile || !virtual_compat)?;
    caps.set("openUrl", true)?;
    caps.set(
        "webSignInMode",
        if is_mobile {
            "webview-with-manual-cookie"
        } else {
            "webview"
        },
    )?;

    let vprocs = Array::new(ctx.clone())?;
    if virtual_compat {
        for (idx, name) in state
            .virtualizer
            .virtualized_commands()
            .into_iter()
            .enumerate()
        {
            vprocs.set(idx, name)?;
        }
    }
    caps.set("virtualProcesses", vprocs)?;

    let codecs = ["mp3", "aac", "mp4", "flac", "vorbis", "wav", "alac", "opus"];
    let codecs_arr = Array::new(ctx.clone())?;
    for (idx, codec) in codecs.into_iter().enumerate() {
        codecs_arr.set(idx, codec)?;
    }
    caps.set("nativeCodecs", codecs_arr)?;

    host.set("capabilities", caps)?;
    Ok(())
}

fn install_spawn<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let spawn_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              cmd: String,
              args: Opt<Vec<String>>,
              opts: Opt<Value<'js>>|
              -> rquickjs::Result<Object<'js>> {
            let arg_list = args.0.unwrap_or_default();
            let spawn_opts = parse_spawn_opts(&ctx, opts.0);

            match state.spawn_process(&cmd, &arg_list, &spawn_opts) {
                Ok((id, is_virtual, proc_handle)) => {
                    build_process_handle_js(&ctx, id, is_virtual, proc_handle)
                }
                Err(Ok(sandbox_err)) => {
                    let err_obj = build_mobile_sandbox_js_error(&ctx, &sandbox_err)?;
                    Err(ctx.throw(err_obj))
                }
                Err(Err(msg)) => Err(Exception::throw_message(&ctx, &msg)),
            }
        },
    )?;

    host.set("spawn", spawn_fn)?;
    Ok(())
}

fn parse_spawn_opts<'js>(ctx: &Ctx<'js>, opts: Option<Value<'js>>) -> SpawnOptions {
    let mut out = SpawnOptions::default();
    let Some(obj) = opts.and_then(|v| v.into_object()) else {
        return out;
    };
    if let Ok(Some(cwd)) = obj.get::<_, Option<String>>("cwd") {
        out.cwd = Some(cwd);
    }
    if let Ok(Some(env_val)) = obj.get::<_, Option<Value<'js>>>("env")
        && let Some(eobj) = env_val.into_object()
    {
        for entry in eobj.props::<String, Value<'js>>() {
            if let Ok((k, v)) = entry
                && let Some(s) = js_value_to_lossy_string(ctx, v)
            {
                out.env.insert(k, s);
            }
        }
    }
    out
}

fn build_mobile_sandbox_js_error<'js>(
    ctx: &Ctx<'js>,
    err: &MobileSandboxError,
) -> rquickjs::Result<Value<'js>> {
    let exc = Exception::from_message(ctx.clone(), &err.to_string())?;
    let obj = exc.as_object();
    obj.set("name", "MobileSandboxError")?;
    obj.set("code", err.code.as_str())?;
    obj.set("command", err.command.as_str())?;
    obj.set("platform", err.platform.as_str())?;
    obj.set("migrationPath", err.migration_path.as_str())?;
    Ok(exc.into_value())
}

fn build_process_handle_js<'js>(
    ctx: &Ctx<'js>,
    id: u64,
    is_virtual: bool,
    proc: Arc<ManagedProcess>,
) -> rquickjs::Result<Object<'js>> {
    let handle = Object::new(ctx.clone())?;
    handle.set("pid", id as f64)?;
    handle.set("virtual", is_virtual)?;

    let p_out = Arc::clone(&proc);
    handle.set(
        "stdout",
        Function::new(ctx.clone(), move || -> String { p_out.stdout() })?,
    )?;

    let p_err = Arc::clone(&proc);
    handle.set(
        "stderr",
        Function::new(ctx.clone(), move || -> String { p_err.stderr() })?,
    )?;

    let p_eout = Arc::clone(&proc);
    handle.set(
        "errorOutput",
        Function::new(ctx.clone(), move || -> String { p_eout.error_output() })?,
    )?;

    let p_running = Arc::clone(&proc);
    handle.set(
        "isRunning",
        Function::new(ctx.clone(), move || -> bool { p_running.is_running() })?,
    )?;

    let p_kill = Arc::clone(&proc);
    handle.set(
        "kill",
        Function::new(ctx.clone(), move || -> bool {
            p_kill.kill();
            true
        })?,
    )?;

    let p_write = Arc::clone(&proc);
    handle.set(
        "write",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, data: String| -> rquickjs::Result<()> {
                p_write
                    .write_stdin(&data)
                    .map_err(|e| Exception::throw_message(&ctx, &e))
            },
        )?,
    )?;

    let p_close_stdin = Arc::clone(&proc);
    handle.set(
        "closeStdin",
        Function::new(ctx.clone(), move || {
            p_close_stdin.close_stdin();
        })?,
    )?;

    let p_readline = Arc::clone(&proc);
    handle.set(
        "readLine",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let p = Arc::clone(&p_readline);
                let ctx_inner = ctx.clone();
                Promise::wrap_future(&ctx, async move {
                    let line = tokio::task::spawn_blocking(move || p.read_line_blocking())
                        .await
                        .ok()
                        .flatten();
                    match line {
                        Some(s) => s.into_js(&ctx_inner),
                        None => Ok(Value::new_null(ctx_inner)),
                    }
                })
            },
        )?,
    )?;

    let p_wait = Arc::clone(&proc);
    handle.set(
        "wait",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let p = Arc::clone(&p_wait);
                Promise::wrap_future(&ctx, async move {
                    let code = tokio::task::spawn_blocking(move || p.wait())
                        .await
                        .unwrap_or(-1);
                    Ok::<i32, JsError>(code)
                })
            },
        )?,
    )?;

    Ok(handle)
}

fn install_web_sign_in<'js>(
    ctx: &Ctx<'js>,
    _host: &Object<'js>,
    globals: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    // Expose `host.webSignIn` dynamically via a property getter in `install_js_polyfills`
    // so plugins see a callable function whenever `state.has_web_sign_in_handler()` is true
    // (whether registered before OR after plugin load), and `undefined` otherwise (so plugins
    // like `bilibili` fall back cleanly to QR sign-in).
    let state_has = Arc::clone(&state);
    let has_handler = Function::new(ctx.clone(), move || -> bool {
        state_has.has_web_sign_in_handler()
    })?;

    let web_signin = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, opts: Value<'js>| -> rquickjs::Result<Promise<'js>> {
            let req = parse_web_sign_in_req(&ctx, &state.plugin_id, opts)?;
            if let Err(msg) = state.check_web_sign_in(&req) {
                return Err(Exception::throw_message(&ctx, &msg));
            }
            let hc = Arc::clone(&state);
            let ctx_inner = ctx.clone();
            Promise::wrap_future(&ctx, async move {
                let result = tokio::task::spawn_blocking(move || hc.run_web_sign_in(req))
                    .await
                    .map_err(|e| {
                        Exception::throw_message(&ctx_inner, &format!("webSignIn task failed: {e}"))
                    })?
                    .map_err(|msg| Exception::throw_message(&ctx_inner, &msg))?;

                let cookie_str = if result.cookie.is_empty() && !result.cookies.is_empty() {
                    let mut pairs: Vec<(&String, &String)> = result.cookies.iter().collect();
                    pairs.sort_by_key(|(a, _)| *a);
                    pairs
                        .into_iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                } else {
                    result.cookie
                };

                let out = Object::new(ctx_inner.clone())?;
                out.set("ok", result.ok)?;
                out.set("url", result.url)?;
                out.set("cookie", cookie_str)?;
                out.set("cancelled", result.cancelled)?;
                out.set("manualFallback", result.manual_fallback)?;
                if let Some(eval_res) = result.eval_result {
                    out.set("evalResult", eval_res)?;
                } else {
                    out.set("evalResult", Value::new_null(ctx_inner.clone()))?;
                }
                let cookies_obj = Object::new(ctx_inner)?;
                for (k, v) in result.cookies {
                    cookies_obj.set(k, v)?;
                }
                out.set("cookies", cookies_obj)?;
                Ok::<Object<'js>, JsError>(out)
            })
        },
    )?;

    globals.set("__nshHasWebSignIn", has_handler)?;
    globals.set("__nshWebSignIn", web_signin)?;
    Ok(())
}

fn parse_string_list<'js>(obj: &Object<'js>, key: &str) -> Vec<String> {
    if let Ok(Some(list)) = obj.get::<_, Option<Vec<String>>>(key) {
        return list
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(Some(single)) = obj.get::<_, Option<String>>(key) {
        let trimmed = single.trim();
        if !trimmed.is_empty() {
            return vec![trimmed.to_owned()];
        }
    }
    Vec::new()
}

fn parse_web_sign_in_req<'js>(
    ctx: &Ctx<'js>,
    plugin_id: &str,
    opts: Value<'js>,
) -> rquickjs::Result<WebSignInRequest> {
    if let Some(url) = opts.as_string().and_then(|s| s.to_string().ok()) {
        return Ok(WebSignInRequest {
            plugin_id: plugin_id.to_owned(),
            url,
            allow_manual_cookie: true,
            ..WebSignInRequest::default()
        });
    }
    let Some(obj) = opts.into_object() else {
        return Err(Exception::throw_message(
            ctx,
            "host.webSignIn expects a URL string or options object",
        ));
    };
    let url = obj.get::<_, String>("url").unwrap_or_default();
    if url.trim().is_empty() {
        return Err(Exception::throw_message(
            ctx,
            "host.webSignIn requires a non-empty `url`",
        ));
    }
    let title = obj
        .get::<_, Option<String>>("title")
        .ok()
        .flatten()
        .unwrap_or_default();
    let incognito = obj
        .get::<_, Option<bool>>("incognito")
        .ok()
        .flatten()
        .unwrap_or(false);
    let user_agent = obj.get::<_, Option<String>>("userAgent").ok().flatten();
    let eval_js = obj.get::<_, Option<String>>("evalJs").ok().flatten();
    let cookies_of = obj.get::<_, Option<String>>("cookiesOf").ok().flatten();
    let match_url = obj.get::<_, Option<String>>("matchUrl").ok().flatten();
    let mut until = parse_string_list(&obj, "until");
    let mut cookie_names = parse_string_list(&obj, "cookieNames");
    if cookie_names.is_empty() && !until.is_empty() {
        cookie_names = until.clone();
    } else if until.is_empty() && !cookie_names.is_empty() {
        until = cookie_names.clone();
    }
    let allow_manual_cookie = obj
        .get::<_, Option<bool>>("allowManualCookie")
        .ok()
        .flatten()
        .unwrap_or(true);
    let timeout_ms = obj
        .get::<_, Option<u64>>("timeoutMs")
        .ok()
        .flatten()
        .unwrap_or(180_000);

    Ok(WebSignInRequest {
        plugin_id: plugin_id.to_owned(),
        url,
        title,
        incognito,
        user_agent,
        eval_js,
        cookies_of,
        until,
        cookie_names,
        match_url,
        allow_manual_cookie,
        timeout_ms,
    })
}

fn install_prompt_and_sleep<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    globals: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let s_prompt = Arc::clone(&state);
    let prompt_fn = Function::new(ctx.clone(), move |val: Value<'js>| {
        let prompt = if let Some(obj) = val.into_object() {
            SignInPrompt {
                qr: obj
                    .get::<_, Option<String>>("qr")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                text: obj
                    .get::<_, Option<String>>("text")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            }
        } else {
            SignInPrompt::default()
        };
        s_prompt.emit_sign_in_prompt(prompt);
    })?;
    host.set("prompt", prompt_fn.clone())?;
    host.set("signInPrompt", prompt_fn)?;

    let sleep_fn = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, ms: u64| -> rquickjs::Result<Promise<'js>> {
            Promise::wrap_future(&ctx, async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok::<(), JsError>(())
            })
        },
    )?;
    host.set("sleep", sleep_fn)?;

    // Cancellable native timer backend for `setTimeout` / `clearTimeout` so long timeouts
    // (such as ffmpeg 15-minute idle guards) never stall runtime shutdown.
    let s_timer = Arc::clone(&state);
    let native_timer = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, timer_id: u64, ms: u64| -> rquickjs::Result<Promise<'js>> {
            let hc = Arc::clone(&s_timer);
            let cancel_rx = hc.register_abort_token(timer_id);
            Promise::wrap_future(&ctx, async move {
                let fired = tokio::select! {
                    _ = cancel_rx => false,
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => true,
                };
                hc.clear_abort_token(timer_id);
                Ok::<bool, JsError>(fired)
            })
        },
    )?;
    let s_clear = Arc::clone(&state);
    let native_clear = Function::new(ctx.clone(), move |timer_id: u64| {
        s_clear.trigger_abort_token(timer_id);
    })?;
    globals.set("__nshTimer", native_timer)?;
    globals.set("__nshClearTimer", native_clear)?;
    Ok(())
}

fn install_navigation_and_data_helpers<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    state: Arc<HostContext>,
) -> rquickjs::Result<()> {
    let s_open = Arc::clone(&state);
    host.set(
        "openURL",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, address: String| -> rquickjs::Result<()> {
                let parsed = url::Url::parse(address.trim()).map_err(|_| {
                    Exception::throw_message(&ctx, &format!("not a web address: {address}"))
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("not a web address: {address}"),
                    ));
                }
                if let Some(handler) = s_open.open_url_handler() {
                    return handler(parsed.as_str())
                        .map_err(|msg| Exception::throw_message(&ctx, &msg));
                }
                if !s_open.is_mobile() {
                    #[cfg(target_os = "macos")]
                    let _ = Command::new("open").arg(parsed.as_str()).spawn();
                    #[cfg(target_os = "linux")]
                    let _ = Command::new("xdg-open").arg(parsed.as_str()).spawn();
                    #[cfg(target_os = "windows")]
                    let _ = Command::new("cmd")
                        .args(["/C", "start", "", parsed.as_str()])
                        .spawn();
                    Ok(())
                } else {
                    Err(Exception::throw_message(&ctx, "can't open pages here"))
                }
            },
        )?,
    )?;

    let s_rm = Arc::clone(&state);
    host.set(
        "removeDataDir",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, name: String| -> rquickjs::Result<()> {
                let rel = Path::new(name.trim());
                if name.trim().is_empty()
                    || name.trim() == "."
                    || rel.is_absolute()
                    || rel.components().any(|c| {
                        matches!(
                            c,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
                {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("not a folder of the plugin's data: {name}"),
                    ));
                }
                let target = s_rm.data_dir.join(rel);
                if target.exists() {
                    fs::remove_dir_all(&target).map_err(|e| {
                        Exception::throw_message(&ctx, &format!("removeDataDir failed: {e}"))
                    })?;
                }
                Ok(())
            },
        )?,
    )?;

    let has_network = !state.manifest.all_network_hosts().is_empty();
    host.set(
        "waitForRedirect",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, opts: Value<'js>| -> rquickjs::Result<Promise<'js>> {
                if !has_network {
                    return Err(Exception::throw_message(
                        &ctx,
                        "waitForRedirect needs the network permission",
                    ));
                }
                let Some(obj) = opts.into_object() else {
                    return Err(Exception::throw_message(
                        &ctx,
                        "waitForRedirect expects an options object",
                    ));
                };
                let port = obj
                    .get::<_, Option<u16>>("port")
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                let path = obj
                    .get::<_, Option<String>>("path")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let timeout_ms = obj
                    .get::<_, Option<u64>>("timeout")
                    .ok()
                    .flatten()
                    .filter(|&ms| ms > 0)
                    .unwrap_or(180_000);
                let page = obj
                    .get::<_, Option<String>>("page")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "You're signed in. You can close this page.".to_owned());
                let ctx_inner = ctx.clone();
                Promise::wrap_future(&ctx, async move {
                    let query_map = await_redirect_once(port, &path, &page, timeout_ms)
                        .await
                        .map_err(|msg| Exception::throw_message(&ctx_inner, &msg))?;
                    let res = Object::new(ctx_inner)?;
                    for (k, v) in query_map {
                        res.set(k, v)?;
                    }
                    Ok::<Object<'js>, JsError>(res)
                })
            },
        )?,
    )?;

    host.set(
        "__parseUrl",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, input: String, base: Opt<String>| -> rquickjs::Result<Object<'js>> {
                let parsed = if let Some(b) = base.0.as_deref() {
                    let base_url = url::Url::parse(b).map_err(|e| {
                        Exception::throw_message(&ctx, &format!("Invalid base URL: {e}"))
                    })?;
                    base_url
                        .join(&input)
                        .map_err(|e| Exception::throw_message(&ctx, &format!("Invalid URL: {e}")))?
                } else {
                    url::Url::parse(&input)
                        .map_err(|e| Exception::throw_message(&ctx, &format!("Invalid URL: {e}")))?
                };
                let out = Object::new(ctx)?;
                let protocol = format!("{}:", parsed.scheme());
                let hostname = parsed.host_str().unwrap_or("").to_owned();
                let port = parsed.port().map(|p| p.to_string()).unwrap_or_default();
                let host = if port.is_empty() {
                    hostname.clone()
                } else {
                    format!("{hostname}:{port}")
                };
                let origin = parsed.origin().ascii_serialization();
                let search = parsed
                    .query()
                    .filter(|q| !q.is_empty())
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                let hash = parsed
                    .fragment()
                    .filter(|h| !h.is_empty())
                    .map(|h| format!("#{h}"))
                    .unwrap_or_default();
                out.set("href", parsed.as_str())?;
                out.set("origin", origin)?;
                out.set("protocol", protocol)?;
                out.set("username", parsed.username())?;
                out.set("password", parsed.password().unwrap_or(""))?;
                out.set("host", host)?;
                out.set("hostname", hostname)?;
                out.set("port", port)?;
                out.set("pathname", parsed.path())?;
                out.set("search", search)?;
                out.set("hash", hash)?;
                Ok(out)
            },
        )?,
    )?;

    Ok(())
}

async fn await_redirect_once(
    port: u16,
    expected_path: &str,
    page: &str,
    timeout_ms: u64,
) -> Result<HashMap<String, String>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("waitForRedirect failed to listen on 127.0.0.1:{port}: {e}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            return Err("timed out waiting for the sign-in to come back".to_owned());
        }
        let Ok(accepted) = tokio::time::timeout(remain, listener.accept()).await else {
            return Err("timed out waiting for the sign-in to come back".to_owned());
        };
        let (mut socket, _) =
            accepted.map_err(|e| format!("waitForRedirect accept failed: {e}"))?;
        let mut buf = [0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let req_target = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/");
        let Ok(parsed) = url::Url::parse(&format!("http://127.0.0.1{req_target}")) else {
            continue;
        };
        if !expected_path.is_empty() && parsed.path() != expected_path {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let query: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            page.len(),
            page
        );
        let _ = socket.write_all(resp.as_bytes()).await;
        return Ok(query);
    }
}

fn install_js_polyfills<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<()> {
    const POLYFILLS: &str = r#"
(function() {
  if (typeof globalThis.atob !== "function") {
    globalThis.atob = (s) => host.crypto.base64Decode(String(s));
  }
  if (typeof globalThis.btoa !== "function") {
    globalThis.btoa = (s) => host.crypto.base64Encode(String(s));
  }

  // Cancellable timers backed by Rust `oneshot` channels so long timers cancel immediately.
  let _nativeTimer = globalThis.__nshTimer;
  const _nativeClear = globalThis.__nshClearTimer;
  delete globalThis.__nshTimer;
  delete globalThis.__nshClearTimer;
  let _nextTimerSeq = 1;
  const _intervalActive = new Set();

  globalThis.setTimeout = (fn, ms, ...args) => {
    const id = (_nextTimerSeq++) * 2;
    const delay = Math.max(0, Number(ms) || 0);
    _nativeTimer(id, delay).then((fired) => {
      if (fired && typeof fn === "function") fn(...args);
    });
    return id;
  };
  globalThis.clearTimeout = (id) => {
    if (typeof id === "number") _nativeClear(id);
  };
  globalThis.setInterval = (fn, ms, ...args) => {
    const token = (_nextTimerSeq++) * 2;
    _intervalActive.add(token);
    const delay = Math.max(1, Number(ms) || 0);
    const tick = () => {
      if (!_intervalActive.has(token)) return;
      _nativeTimer(token, delay).then((fired) => {
        if (fired && _intervalActive.has(token) && typeof fn === "function") {
          fn(...args);
          tick();
        }
      });
    };
    tick();
    return token;
  };
  globalThis.clearInterval = (id) => {
    _intervalActive.delete(id);
    if (typeof id === "number") _nativeClear(id);
  };

  if (typeof globalThis.URLSearchParams !== "function") {
    class URLSearchParams {
      constructor(init) {
        this._entries = [];
        if (typeof init === "string") {
          const raw = init.startsWith("?") ? init.slice(1) : init;
          if (raw) {
            for (const part of raw.split("&")) {
              if (!part) continue;
              const idx = part.indexOf("=");
              const k = decodeURIComponent((idx >= 0 ? part.slice(0, idx) : part).replace(/\+/g, " "));
              const v = decodeURIComponent((idx >= 0 ? part.slice(idx + 1) : "").replace(/\+/g, " "));
              this._entries.push([k, v]);
            }
          }
        } else if (Array.isArray(init)) {
          for (const pair of init) {
            if (Array.isArray(pair) && pair.length >= 2) {
              this._entries.push([String(pair[0]), String(pair[1])]);
            }
          }
        } else if (init && typeof init === "object") {
          for (const [k, v] of Object.entries(init)) {
            this._entries.push([String(k), String(v)]);
          }
        }
      }
      append(k, v) { this._entries.push([String(k), String(v)]); }
      set(k, v) {
        k = String(k);
        this._entries = this._entries.filter(([ek]) => ek !== k);
        this._entries.push([k, String(v)]);
      }
      get(k) {
        const hit = this._entries.find(([ek]) => ek === String(k));
        return hit ? hit[1] : null;
      }
      getAll(k) {
        return this._entries.filter(([ek]) => ek === String(k)).map(([, v]) => v);
      }
      has(k) { return this._entries.some(([ek]) => ek === String(k)); }
      delete(k) { this._entries = this._entries.filter(([ek]) => ek !== String(k)); }
      sort() { this._entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)); }
      keys() { return this._entries.map(([k]) => k)[Symbol.iterator](); }
      values() { return this._entries.map(([, v]) => v)[Symbol.iterator](); }
      entries() { return this._entries[Symbol.iterator](); }
      forEach(cb, thisArg) { for (const [k, v] of this._entries) cb.call(thisArg, v, k, this); }
      get size() { return this._entries.length; }
      [Symbol.iterator]() { return this.entries(); }
      toString() {
        return this._entries
          .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
          .join("&");
      }
    }
    globalThis.URLSearchParams = URLSearchParams;
  }

  if (typeof globalThis.URL !== "function") {
    class URL {
      constructor(input, base) {
        const parts = base !== undefined ? host.__parseUrl(String(input), String(base)) : host.__parseUrl(String(input));
        this.protocol = parts.protocol;
        this.username = parts.username;
        this.password = parts.password;
        this.host = parts.host;
        this.hostname = parts.hostname;
        this.port = parts.port;
        this.origin = parts.origin;
        this.pathname = parts.pathname;
        this.hash = parts.hash;
        this._search = parts.search;
        this.searchParams = new globalThis.URLSearchParams(parts.search);
      }
      get search() {
        const q = this.searchParams.toString();
        return q ? "?" + q : "";
      }
      set search(val) {
        const s = String(val ?? "");
        this._search = s.startsWith("?") ? s : (s ? "?" + s : "");
        this.searchParams = new globalThis.URLSearchParams(this._search);
      }
      get href() {
        const auth = this.username ? (this.password ? `${this.username}:${this.password}@` : `${this.username}@`) : "";
        return `${this.protocol}//${auth}${this.host}${this.pathname}${this.search}${this.hash}`;
      }
      toString() { return this.href; }
      toJSON() { return this.href; }
    }
    globalThis.URL = URL;
  }

  if (typeof globalThis.AbortSignal !== "function") {
    class AbortSignal {
      constructor() {
        this.aborted = false;
        this.reason = undefined;
        this._listeners = [];
        this.onabort = null;
      }
      addEventListener(type, fn) {
        if (type === "abort" && typeof fn === "function") this._listeners.push(fn);
      }
      removeEventListener(type, fn) {
        if (type === "abort") this._listeners = this._listeners.filter((l) => l !== fn);
      }
      throwIfAborted() {
        if (this.aborted) {
          const e = new Error(String(this.reason ?? "The operation was aborted"));
          e.name = "AbortError";
          throw e;
        }
      }
      _trigger(reason) {
        if (this.aborted) return;
        this.aborted = true;
        if (reason !== undefined) {
          this.reason = reason;
        } else {
          const e = new Error("The operation was aborted");
          e.name = "AbortError";
          this.reason = e;
        }
        const ev = { type: "abort", target: this };
        if (typeof this.onabort === "function") {
          try { this.onabort(ev); } catch {}
        }
        const list = this._listeners.slice();
        this._listeners = [];
        for (const fn of list) {
          try { fn(ev); } catch {}
        }
      }
      static abort(reason) {
        const s = new AbortSignal();
        s._trigger(reason);
        return s;
      }
      static timeout(ms) {
        const s = new AbortSignal();
        globalThis.setTimeout(() => {
          const e = new Error("The operation timed out");
          e.name = "TimeoutError";
          s._trigger(e);
        }, ms);
        return s;
      }
    }
    class AbortController {
      constructor() {
        this.signal = new AbortSignal();
      }
      abort(reason) {
        this.signal._trigger(reason);
      }
    }
    globalThis.AbortSignal = AbortSignal;
    globalThis.AbortController = AbortController;
  }

  if (typeof globalThis.structuredClone !== "function") {
    globalThis.structuredClone = function clone(val, seen = new WeakMap()) {
      if (val === null || typeof val !== "object") return val;
      if (seen.has(val)) return seen.get(val);
      if (val instanceof Date) return new Date(val.getTime());
      if (val instanceof RegExp) return new RegExp(val.source, val.flags);
      if (val instanceof Uint8Array) return new Uint8Array(val);
      if (val instanceof ArrayBuffer) return val.slice(0);
      if (val instanceof Map) {
        const out = new Map();
        seen.set(val, out);
        for (const [k, v] of val.entries()) out.set(clone(k, seen), clone(v, seen));
        return out;
      }
      if (val instanceof Set) {
        const out = new Set();
        seen.set(val, out);
        for (const v of val.values()) out.add(clone(v, seen));
        return out;
      }
      if (Array.isArray(val)) {
        const out = new Array(val.length);
        seen.set(val, out);
        for (let i = 0; i < val.length; i++) out[i] = clone(val[i], seen);
        return out;
      }
      const out = {};
      seen.set(val, out);
      for (const k of Object.keys(val)) out[k] = clone(val[k], seen);
      return out;
    };
  }

  // Wrap native `fetch` to wire `AbortSignal`, `URL` inputs, and `URLSearchParams` bodies.
  const _nativeFetch = globalThis.__nshFetch;
  const _nativeAbortFetch = globalThis.__nshAbortFetch;
  delete globalThis.__nshFetch;
  delete globalThis.__nshAbortFetch;
  let _nextFetchSeq = 1;
  function wrappedFetch(input, opts) {
    const urlStr = String(input);
    const sig = opts && opts.signal;
    if (sig && sig.aborted) {
      const err = new Error(String(sig.reason ?? "The operation was aborted"));
      err.name = "AbortError";
      return Promise.reject(err);
    }
    let forwarded = opts;
    if (opts && opts.body instanceof globalThis.URLSearchParams) {
      forwarded = { ...opts, body: opts.body.toString() };
    }
    const reqId = (_nextFetchSeq++) * 2 + 1;
    let onAbort = null;
    if (sig && typeof sig.addEventListener === "function") {
      onAbort = () => _nativeAbortFetch(reqId);
      sig.addEventListener("abort", onAbort);
    }
    const detach = () => {
      if (sig && onAbort && typeof sig.removeEventListener === "function") {
        sig.removeEventListener("abort", onAbort);
      }
    };
    return _nativeFetch(reqId, urlStr, forwarded).then(
      (res) => { detach(); return res; },
      (err) => {
        detach();
        if (sig && sig.aborted && err) err.name = "AbortError";
        throw err;
      }
    );
  }
  globalThis.fetch = wrappedFetch;
  host.fetch = wrappedFetch;

  // Dynamically expose `host.webSignIn` only while a native WebSignInHandler is registered.
  const _hasWebSignIn = globalThis.__nshHasWebSignIn;
  const _webSignInFn = globalThis.__nshWebSignIn;
  delete globalThis.__nshHasWebSignIn;
  delete globalThis.__nshWebSignIn;
  if (typeof _hasWebSignIn === "function" && typeof _webSignInFn === "function") {
    Object.defineProperty(host, "webSignIn", {
      enumerable: true,
      configurable: true,
      get() {
        return _hasWebSignIn() ? _webSignInFn : undefined;
      },
    });
  }

  if (typeof globalThis.TextEncoder !== "function") {
    class TextEncoder {
      encode(str) {
        const s = String(str ?? "");
        const out = [];
        for (let i = 0; i < s.length; i++) {
          let cp = s.charCodeAt(i);
          if (cp >= 0xd800 && cp <= 0xdbff && i + 1 < s.length) {
            const lo = s.charCodeAt(i + 1);
            if (lo >= 0xdc00 && lo <= 0xdfff) {
              cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
              i++;
            }
          }
          if (cp < 0x80) {
            out.push(cp);
          } else if (cp < 0x800) {
            out.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
          } else if (cp < 0x10000) {
            out.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
          } else {
            out.push(
              0xf0 | (cp >> 18),
              0x80 | ((cp >> 12) & 0x3f),
              0x80 | ((cp >> 6) & 0x3f),
              0x80 | (cp & 0x3f)
            );
          }
        }
        return new Uint8Array(out);
      }
    }
    globalThis.TextEncoder = TextEncoder;
  }
  if (typeof globalThis.TextDecoder !== "function") {
    class TextDecoder {
      decode(bytes) {
        if (!bytes) return "";
        const arr = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
        let out = "";
        for (let i = 0; i < arr.length; ) {
          const b0 = arr[i++];
          if (b0 < 0x80) {
            out += String.fromCharCode(b0);
          } else if ((b0 & 0xe0) === 0xc0 && i < arr.length) {
            const cp = ((b0 & 0x1f) << 6) | (arr[i++] & 0x3f);
            out += String.fromCharCode(cp);
          } else if ((b0 & 0xf0) === 0xe0 && i + 1 < arr.length) {
            const cp = ((b0 & 0x0f) << 12) | ((arr[i++] & 0x3f) << 6) | (arr[i++] & 0x3f);
            out += String.fromCharCode(cp);
          } else if ((b0 & 0xf8) === 0xf0 && i + 2 < arr.length) {
            const cp =
              ((b0 & 0x07) << 18) |
              ((arr[i++] & 0x3f) << 12) |
              ((arr[i++] & 0x3f) << 6) |
              (arr[i++] & 0x3f);
            out += String.fromCodePoint(cp);
          } else {
            out += "\ufffd";
          }
        }
        return out;
      }
    }
    globalThis.TextDecoder = TextDecoder;
  }
})();
"#;
    ctx.eval::<(), _>(POLYFILLS)
}

fn try_parse_aes_key(raw: &[u8]) -> Option<Vec<u8>> {
    if matches!(raw.len(), 16 | 24 | 32) {
        return Some(raw.to_vec());
    }
    if let Ok(s) = std::str::from_utf8(raw)
        && let Ok(decoded) = crypto::hex_decode(s)
        && matches!(decoded.len(), 16 | 24 | 32)
    {
        return Some(decoded);
    }
    None
}

fn resolve_aes_key_from_raw(raw: Vec<u8>) -> Vec<u8> {
    try_parse_aes_key(&raw).unwrap_or(raw)
}

fn resolve_aes_key_bytes<'js>(ctx: &Ctx<'js>, key: Value<'js>) -> rquickjs::Result<Vec<u8>> {
    let raw = js_value_to_bytes(ctx, key)?;
    Ok(resolve_aes_key_from_raw(raw))
}

fn resolve_iv_bytes<'js>(ctx: &Ctx<'js>, iv: Value<'js>) -> rquickjs::Result<Vec<u8>> {
    let raw = js_value_to_bytes(ctx, iv)?;
    if raw.len() == 16 {
        return Ok(raw);
    }
    if let Ok(s) = std::str::from_utf8(&raw)
        && let Ok(decoded) = crypto::hex_decode(s)
        && decoded.len() == 16
    {
        return Ok(decoded);
    }
    Ok(raw)
}

fn encode_output_bytes(bytes: &[u8], encoding: Option<&str>) -> rquickjs::Result<String> {
    match encoding.unwrap_or("hex").to_ascii_lowercase().as_str() {
        "base64" | "b64" => Ok(crypto::base64_encode(bytes)),
        "base64url" => Ok(crypto::base64_url_encode(bytes)),
        "hex_upper" | "HEX" => Ok(crypto::hex_encode(bytes).to_ascii_uppercase()),
        _ => Ok(crypto::hex_encode(bytes)),
    }
}

fn decode_input_bytes(input: &str, encoding: Option<&str>) -> Result<Vec<u8>, String> {
    match encoding.unwrap_or("hex").to_ascii_lowercase().as_str() {
        "base64" | "b64" | "base64url" => crypto::base64_decode(input).map_err(|e| e.to_string()),
        _ => crypto::hex_decode(input).map_err(|e| e.to_string()),
    }
}

fn js_value_to_bytes<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> rquickjs::Result<Vec<u8>> {
    if let Some(s) = val.as_string() {
        return Ok(s.to_string()?.into_bytes());
    }
    if let Some(arr) = val.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for i in 0..arr.len() {
            let n: i32 = arr.get(i)?;
            out.push((n & 0xff) as u8);
        }
        return Ok(out);
    }
    if let Some(obj) = val.as_object() {
        if let Some(ta) = obj.as_typed_array::<u8>() {
            // SAFETY: QuickJS runtime lock is held on this thread and no JS executes while copying.
            if let Some(slice) = unsafe { ta.as_bytes() } {
                return Ok(slice.to_vec());
            }
        }
        if let Some(ab) = obj.as_array_buffer() {
            // SAFETY: QuickJS runtime lock is held on this thread and no JS executes while copying.
            if let Some(slice) = unsafe { ab.as_bytes() } {
                return Ok(slice.to_vec());
            }
        }
    }
    let fallback = js_value_to_lossy_string(ctx, val).unwrap_or_default();
    Ok(fallback.into_bytes())
}

fn js_value_to_lossy_string<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> Option<String> {
    if val.is_undefined() || val.is_null() {
        return None;
    }
    if let Some(s) = val.as_string() {
        return s.to_string().ok();
    }
    if let Some(b) = val.as_bool() {
        return Some(b.to_string());
    }
    if let Some(i) = val.as_int() {
        return Some(i.to_string());
    }
    if let Some(f) = val.as_float() {
        return Some(f.to_string());
    }
    ctx.json_stringify(val)
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
}

fn format_js_arg<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> String {
    if val.is_undefined() {
        return "undefined".to_owned();
    }
    if val.is_null() {
        return "null".to_owned();
    }
    js_value_to_lossy_string(ctx, val).unwrap_or_else(|| "[object]".to_owned())
}

/// Converts a `serde_json::Value` into a QuickJS `Value`.
pub(crate) fn json_to_js<'js>(
    ctx: &Ctx<'js>,
    val: &serde_json::Value,
) -> rquickjs::Result<Value<'js>> {
    if val.is_null() {
        return Ok(Value::new_null(ctx.clone()));
    }
    let raw = serde_json::to_string(val).unwrap_or_else(|_| "null".to_owned());
    ctx.json_parse(raw.as_bytes())
}

/// Converts a QuickJS `Value` into a `serde_json::Value`.
pub(crate) fn js_to_json<'js>(
    ctx: &Ctx<'js>,
    val: Value<'js>,
) -> rquickjs::Result<serde_json::Value> {
    if val.is_undefined() || val.is_null() {
        return Ok(serde_json::Value::Null);
    }
    match ctx.json_stringify(val)? {
        Some(s) => {
            let text = s.to_string()?;
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
        }
        None => Ok(serde_json::Value::Null),
    }
}

fn is_innertube_web_auth_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("cookie")
        || name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("x-goog-authuser")
        || name.eq_ignore_ascii_case("origin")
        || name.eq_ignore_ascii_case("x-origin")
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn read_full_http_request(stream: &mut std::net::TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut header_end: Option<usize> = None;
        let mut content_len: usize = 0;

        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if header_end.is_none()
                        && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        header_end = Some(pos + 4);
                        let hdr_str = String::from_utf8_lossy(&buf[..pos]);
                        for line in hdr_str.lines() {
                            if let Some((k, v)) = line.split_once(':')
                                && k.trim().eq_ignore_ascii_case("content-length")
                            {
                                content_len = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    if let Some(hend) = header_end
                        && buf.len() >= hend + content_len
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn android_music_innertube_400_retries_anonymously_on_next_and_browse() {
        let root = std::env::temp_dir().join(format!(
            "nsh-plugin-host-android-retry-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();

        // Expect 4 requests: (/next with auth -> 400, /next anon -> 200,
        //                     /browse with auth -> 400, /browse anon -> 200).
        let server_thread = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept");
                let raw = read_full_http_request(&mut stream);
                let lower = raw.to_ascii_lowercase();
                let has_auth = lower.contains("\ncookie:")
                    || lower.contains("\nauthorization:")
                    || lower.contains("\nx-goog-authuser:")
                    || lower.contains("\norigin:")
                    || lower.contains("\nx-origin:");
                let has_android_ua = lower.contains("com.google.android.apps.youtube.music");
                let has_android_body = raw.contains("ANDROID_MUSIC");
                seen.push((has_auth, has_android_ua, has_android_body));

                let (status_line, body) = if has_auth {
                    (
                        "HTTP/1.1 400 Bad Request",
                        r#"{"error":{"code":400,"message":"bad android auth"}}"#,
                    )
                } else {
                    (
                        "HTTP/1.1 200 OK",
                        r#"{"browseId":"MPLYt_ok","timedLyricsData":[{"lyricLine":"hello"}]}"#,
                    )
                };
                let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
            seen
        });

        let manifest = Arc::new(
            PluginManifest::parse_json(
                r#"{
                    "id": "yt-retry-test",
                    "name": "YT Retry Test",
                    "permissions": { "network": ["127.0.0.1"] }
                }"#,
            )
            .unwrap(),
        );
        let ctx = HostContext::new(
            plugin_dir,
            data_dir,
            manifest,
            Arc::new(PluginLocales::default()),
            HostRuntimeOptions::default(),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        for endpoint in ["next", "browse"] {
            let url = format!("http://127.0.0.1:{port}/youtubei/v1/{endpoint}?prettyPrint=false");
            let opts = FetchOptions {
                method: "POST".to_owned(),
                headers: vec![
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    (
                        "User-Agent".to_owned(),
                        "com.google.android.apps.youtube.music/7.21.50 (Linux; U; Android 14) gzip"
                            .to_owned(),
                    ),
                    ("Origin".to_owned(), "https://music.youtube.com".to_owned()),
                    (
                        "X-Origin".to_owned(),
                        "https://music.youtube.com".to_owned(),
                    ),
                    ("Cookie".to_owned(), "SAPISID=secret".to_owned()),
                    (
                        "Authorization".to_owned(),
                        "SAPISIDHASH 123_abc".to_owned(),
                    ),
                    ("X-Goog-AuthUser".to_owned(), "0".to_owned()),
                ],
                body: Some(
                    br#"{"context":{"client":{"clientName":"ANDROID_MUSIC"}},"videoId":"dQw4w9WgXcQ"}"#
                        .to_vec(),
                ),
                timeout_ms: Some(5_000),
                follow_redirects: true,
            };
            let (_tx, rx) = tokio::sync::oneshot::channel();
            let resp = rt
                .block_on(ctx.perform_fetch(&url, opts, rx))
                .expect("retry succeeds");
            assert!(resp.ok, "endpoint={endpoint} should succeed after retry");
            assert_eq!(resp.status, 200);
            let body = String::from_utf8_lossy(&resp.bytes);
            assert!(body.contains("MPLYt_ok"), "body={body}");
        }

        let seen = server_thread.join().expect("server thread");
        assert_eq!(
            seen,
            vec![
                (true, true, true),
                (false, true, true),
                (true, true, true),
                (false, true, true),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_request_headers_enforces_declared_network_hosts_and_storage_placeholders() {
        let root =
            std::env::temp_dir().join(format!("nsh-plugin-host-reqheaders-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugin");
        let data_dir = root.join("data");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let manifest = Arc::new(
            PluginManifest::parse_json(
                r#"{
                    "id": "selfhost-demo",
                    "name": "Selfhost Demo",
                    "permissions": {
                        "network": ["navidrome.example.com"]
                    },
                    "requestHeaders": [
                        {
                            "hosts": ["{setting:server}", "evil.outside.com"],
                            "kinds": ["image", "audio"],
                            "headers": {
                                "Cookie": "music-token={storage:token}",
                                "X-User": "{setting:username}",
                                "Host": "spoofed.invalid"
                            }
                        }
                    ]
                }"#,
            )
            .unwrap(),
        );
        let ctx = HostContext::new(
            plugin_dir,
            data_dir,
            manifest,
            Arc::new(PluginLocales::default()),
            HostRuntimeOptions::default(),
        );
        ctx.set_config("server", "https://navidrome.example.com:4533/sub");
        ctx.set_config("username", "tester");
        ctx.set_storage("token", serde_json::json!("sess-xyz"));

        // Declared network host receives expanded `{setting:*}` and `{storage:*}` headers (no `Host`).
        let allowed = ctx.resolve_request_headers(
            "https://navidrome.example.com:4533/rest/stream?id=7",
            HeaderRequestKind::Audio,
        );
        assert!(allowed.contains(&("Cookie".to_owned(), "music-token=sess-xyz".to_owned())));
        assert!(allowed.contains(&("X-User".to_owned(), "tester".to_owned())));
        assert!(!allowed.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")));

        // Host listed only in `requestHeaders` (outside `permissions.network`) MUST be blocked.
        let blocked = ctx.resolve_request_headers(
            "https://evil.outside.com/track.mp3",
            HeaderRequestKind::Audio,
        );
        assert!(
            blocked.is_empty(),
            "host outside declared permissions.network must not receive requestHeaders: {blocked:?}"
        );

        // Changing `{setting:server}` to a host outside `permissions.network` is also blocked.
        ctx.set_config("server", "https://undeclared.example.org");
        let blocked_setting = ctx.resolve_request_headers(
            "https://undeclared.example.org/cover.jpg",
            HeaderRequestKind::Image,
        );
        assert!(blocked_setting.is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
