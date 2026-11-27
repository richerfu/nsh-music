use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nsh_music_audio::virtual_ffmpeg::{VirtualFfmpegShim, is_ffmpeg_command};

use crate::host::{
    ExternalProcessHandle, HostContext, ManagedProcess, MobileSandboxError, SpawnOptions,
};
use crate::virtual_ytdlp::YtdlpVirtualShim;

/// Error returned by a [`VirtualProcessShim`] (either a structured mobile sandbox block or a
/// human-readable spawn/permission error string).
pub type VirtualSpawnError = Result<MobileSandboxError, String>;

/// Result returned by [`VirtualProcessShim::spawn`] / [`MobileProcessVirtualizer::spawn`].
///
/// - `Ok(Some((is_virtual, proc)))`: handled by a shim (`is_virtual == true` for in-process shims).
/// - `Ok(None)`: not handled by this shim; caller may proceed to default external spawn handling.
/// - `Err(...)`: spawn denied or failed.
pub type VirtualSpawnResult = Result<Option<(bool, Arc<ManagedProcess>)>, VirtualSpawnError>;

/// Extensible, plugin-agnostic contract for in-process virtual CLI shims (mirroring `VirtualFfmpegShim`).
///
/// Shims intercept `host.spawn(cmd, args, opts)` for generic system executables so unmodified
/// `mygo-music` plugins see a uniform `host.*` process interface across Desktop and Mobile.
pub trait VirtualProcessShim: Send + Sync {
    /// Canonical executable name handled by this shim (e.g. `"ffmpeg"`).
    fn name(&self) -> &'static str;

    /// Returns `true` if this shim handles `cmd` (bare name, absolute path, or `.exe`).
    fn matches(&self, cmd: &str) -> bool;

    /// Attempts to handle spawning `(cmd, args, opts)`.
    fn spawn(
        &self,
        cmd: &str,
        args: &[String],
        opts: &SpawnOptions,
        ctx: &HostContext,
    ) -> VirtualSpawnResult;
}

/// Registry of generic virtual process shims consulted by `HostContext::spawn_process`.
pub struct MobileProcessVirtualizer {
    shims: Vec<Arc<dyn VirtualProcessShim>>,
}

impl Default for MobileProcessVirtualizer {
    fn default() -> Self {
        Self::with_default_shims()
    }
}

impl MobileProcessVirtualizer {
    /// Builds the default registry with [`FfmpegVirtualShim`] and [`YtdlpVirtualShim`].
    pub fn with_default_shims() -> Self {
        Self {
            shims: vec![Arc::new(FfmpegVirtualShim), Arc::new(YtdlpVirtualShim)],
        }
    }

    /// Registers an additional generic [`VirtualProcessShim`].
    pub fn register(&mut self, shim: Arc<dyn VirtualProcessShim>) {
        self.shims.push(shim);
    }

    /// Returns the canonical names of all registered virtualized executables (e.g. `["ffmpeg"]`).
    pub fn virtualized_commands(&self) -> Vec<&'static str> {
        self.shims.iter().map(|s| s.name()).collect()
    }

    /// Returns `true` if any registered shim handles `cmd`.
    pub fn matches_any(&self, cmd: &str) -> bool {
        self.shims.iter().any(|s| s.matches(cmd))
    }

    /// Dispatches `(cmd, args, opts)` to the first matching shim.
    pub fn spawn(
        &self,
        cmd: &str,
        args: &[String],
        opts: &SpawnOptions,
        ctx: &HostContext,
    ) -> VirtualSpawnResult {
        for shim in &self.shims {
            if shim.matches(cmd)
                && let Some(handled) = shim.spawn(cmd, args, opts, ctx)?
            {
                return Ok(Some(handled));
            }
        }
        Ok(None)
    }
}

/// Generic `ffmpeg` shim wrapping [`VirtualFfmpegShim`].
///
/// - On Desktop (`!is_mobile && granted && allows_process`), prefers real system `ffmpeg` when
///   transcoding (`-c:a libmp3lame`, `-b:a`, or non-copy `-f mp3`) or probing `-h protocol=...`,
///   falling back transparently to `VirtualFfmpegShim` if real `ffmpeg` is absent.
/// - On Mobile (or when `!granted`), runs `VirtualFfmpegShim` in-process (pure passthrough +
///   header injection) without requiring desktop `Allow Programs`.
pub struct FfmpegVirtualShim;

impl VirtualProcessShim for FfmpegVirtualShim {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn matches(&self, cmd: &str) -> bool {
        is_ffmpeg_command(cmd)
    }

    fn spawn(
        &self,
        cmd: &str,
        args: &[String],
        opts: &SpawnOptions,
        ctx: &HostContext,
    ) -> VirtualSpawnResult {
        if !ctx.is_mobile()
            && ctx.is_granted()
            && ctx.manifest.allows_process(&ctx.plugin_dir, cmd)
            && (ffmpeg_requests_transcoding(args) || is_ffmpeg_protocol_help_probe(args))
            && let Some(real_ffmpeg) = resolve_real_ffmpeg(cmd)
        {
            let real_cmd = real_ffmpeg.to_string_lossy().into_owned();
            let effective_opts = with_default_cwd(opts, &ctx.plugin_dir);
            match ExternalProcessHandle::spawn(&real_cmd, args, &effective_opts) {
                Ok(handle) => {
                    return Ok(Some((
                        false,
                        Arc::new(ManagedProcess::from_external(handle)),
                    )));
                }
                Err(err) => {
                    log::warn!(
                        target: "nsh_music_plugin",
                        "real ffmpeg spawn failed ({err}); falling back to VirtualFfmpegShim"
                    );
                }
            }
        }

        let vhandle = VirtualFfmpegShim::spawn(args)
            .map_err(|e| Err(format!("VirtualFfmpegShim failed: {e}")))?;
        Ok(Some((
            true,
            Arc::new(ManagedProcess::from_virtual_ffmpeg(vhandle, Mutex::new(0))),
        )))
    }
}

fn is_rooted_or_abs_command(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let has_windows_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    Path::new(cmd).is_absolute()
        || cmd.starts_with('/')
        || cmd.starts_with('\\')
        || has_windows_drive
}

pub(crate) fn resolve_relative_command(plugin_dir: &Path, cmd: &str) -> String {
    if (cmd.contains('/') || cmd.contains('\\')) && !is_rooted_or_abs_command(cmd) {
        plugin_dir.join(cmd).to_string_lossy().into_owned()
    } else {
        cmd.to_owned()
    }
}

pub(crate) fn with_default_cwd(opts: &SpawnOptions, plugin_dir: &Path) -> SpawnOptions {
    SpawnOptions {
        cwd: opts
            .cwd
            .clone()
            .or_else(|| Some(plugin_dir.to_string_lossy().into_owned())),
        env: opts.env.clone(),
    }
}

#[cfg(unix)]
pub(crate) fn augment_unix_path(base: &str) -> String {
    let mut parts: Vec<String> = base
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if !parts.iter().any(|p| p == extra) {
            parts.push(extra.to_owned());
        }
    }
    parts.join(":")
}

#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
pub(crate) fn resolve_unix_executable(cmd: &str, augmented_path: &str) -> Option<PathBuf> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.contains('/') {
        return None;
    }
    for dir in augmented_path.split(':').filter(|s| !s.is_empty()) {
        let candidate = Path::new(dir).join(trimmed);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Resolves a real desktop binary (`cmd`, augmented `PATH`, `/opt/homebrew/bin/<name>`, `/usr/local/bin/<name>`).
pub(crate) fn resolve_real_binary(cmd: &str, binary_name: &str) -> Option<PathBuf> {
    let trimmed = cmd.trim().trim_matches('"').trim_matches('\'');
    if (trimmed.contains('/') || trimmed.contains('\\')) && is_executable_file(Path::new(trimmed)) {
        return Some(PathBuf::from(trimmed));
    }

    #[cfg(unix)]
    {
        let base_path = std::env::var("PATH").unwrap_or_default();
        let augmented = augment_unix_path(&base_path);
        if let Some(found) = resolve_unix_executable(binary_name, &augmented) {
            return Some(found);
        }
        for prefix in ["/opt/homebrew/bin", "/usr/local/bin"] {
            let p = Path::new(prefix).join(binary_name);
            if is_executable_file(&p) {
                return Some(p);
            }
        }
        None
    }

    #[cfg(not(unix))]
    {
        let base_path = std::env::var("PATH").unwrap_or_default();
        let exe_name = format!("{binary_name}.exe");
        for dir in std::env::split_paths(&base_path) {
            for name in [exe_name.as_str(), binary_name] {
                let candidate = dir.join(name);
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

/// Resolves a real system `ffmpeg` binary if available on desktop.
pub(crate) fn resolve_real_ffmpeg(cmd: &str) -> Option<PathBuf> {
    resolve_real_binary(cmd, "ffmpeg")
}

/// Returns `true` when an `ffmpeg` CLI invocation requests audio transcoding (e.g., `-c:a libmp3lame`,
/// `-b:a 192k`, or `-f mp3` without stream-copy on a non-`.mp3` input).
pub(crate) fn ffmpeg_requests_transcoding(args: &[String]) -> bool {
    let mut explicit_copy = false;
    let mut format_mp3 = false;
    let mut input_url = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let next = args.get(i + 1).map(String::as_str);
        match arg {
            "-c:a" | "-c:a:0" | "-acodec" | "-codec:a" => {
                if let Some(val) = next {
                    if val.eq_ignore_ascii_case("copy") {
                        explicit_copy = true;
                    } else {
                        return true;
                    }
                    i += 2;
                    continue;
                }
            }
            "-c" | "-codec" => {
                if let Some(val) = next {
                    if val.eq_ignore_ascii_case("copy") {
                        explicit_copy = true;
                    } else {
                        return true;
                    }
                    i += 2;
                    continue;
                }
            }
            "-b:a" | "-b:a:0" | "-ab" | "-q:a" | "-aq" | "-af" | "-filter:a" => {
                if next.is_some() {
                    return true;
                }
            }
            "-f" => {
                if let Some(val) = next {
                    if val.eq_ignore_ascii_case("mp3") {
                        format_mp3 = true;
                    }
                    i += 2;
                    continue;
                }
            }
            "-i" => {
                if let Some(val) = next {
                    input_url = val.to_owned();
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if !format_mp3 || explicit_copy {
        return false;
    }
    let input_path_mp3 = url::Url::parse(&input_url)
        .map(|u| u.path().to_ascii_lowercase().ends_with(".mp3"))
        .unwrap_or_else(|_| input_url.to_ascii_lowercase().ends_with(".mp3"));
    !input_path_mp3
}

/// Returns `true` when `args` is a protocol capability probe (`-h protocol=...`).
pub(crate) fn is_ffmpeg_protocol_help_probe(args: &[String]) -> bool {
    args.windows(2)
        .any(|w| (w[0] == "-h" || w[0] == "-help") && w[1].starts_with("protocol="))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyShim;
    impl VirtualProcessShim for DummyShim {
        fn name(&self) -> &'static str {
            "dummy-tool"
        }
        fn matches(&self, cmd: &str) -> bool {
            cmd == "dummy-tool"
        }
        fn spawn(
            &self,
            _cmd: &str,
            _args: &[String],
            _opts: &SpawnOptions,
            _ctx: &HostContext,
        ) -> VirtualSpawnResult {
            Ok(None)
        }
    }

    #[test]
    fn default_virtualizer_registers_ffmpeg_and_ytdlp_and_is_extensible() {
        let mut reg = MobileProcessVirtualizer::with_default_shims();
        assert_eq!(reg.virtualized_commands(), vec!["ffmpeg", "yt-dlp"]);
        assert!(reg.matches_any("ffmpeg"));
        assert!(reg.matches_any("/opt/homebrew/bin/ffmpeg"));
        assert!(reg.matches_any("yt-dlp"));
        assert!(reg.matches_any("/opt/homebrew/bin/yt-dlp"));
        assert!(!reg.matches_any("python3"));

        reg.register(Arc::new(DummyShim));
        assert_eq!(
            reg.virtualized_commands(),
            vec!["ffmpeg", "yt-dlp", "dummy-tool"]
        );
        assert!(reg.matches_any("dummy-tool"));
    }

    #[test]
    fn resolves_relative_commands_and_protocol_probes() {
        let pdir = Path::new("/plugins/sample");
        assert_eq!(resolve_relative_command(pdir, "ffmpeg"), "ffmpeg");
        assert_eq!(
            resolve_relative_command(pdir, "bin/helper"),
            pdir.join("bin/helper").to_string_lossy().as_ref()
        );
        assert_eq!(
            resolve_relative_command(pdir, "/usr/local/bin/tool"),
            "/usr/local/bin/tool"
        );
        assert_eq!(
            resolve_relative_command(pdir, r"C:\tools\helper.exe"),
            r"C:\tools\helper.exe"
        );

        let help_args: Vec<String> = ["-hide_banner", "-h", "protocol=http"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(is_ffmpeg_protocol_help_probe(&help_args));
    }
}
