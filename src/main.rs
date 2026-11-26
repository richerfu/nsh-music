//! Standalone native desktop launcher for `nsh-music` (`cargo run`).

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(target_os = "macos")]
const APP_ICON_FILE: &str = "app-icon-macos.png";
#[cfg(target_os = "macos")]
const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon-macos.png");

#[cfg(target_os = "windows")]
const APP_ICON_FILE: &str = "app-icon.png";
#[cfg(target_os = "windows")]
const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon.png");

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn resolve_app_icon_path() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};

    let rel = Path::new("assets").join(APP_ICON_FILE);
    if rel.is_file() {
        return Some(rel);
    }

    let manifest_icon = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(APP_ICON_FILE);
    if manifest_icon.is_file() {
        return Some(manifest_icon);
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("assets").join(APP_ICON_FILE),
            dir.join("../assets").join(APP_ICON_FILE),
            dir.join("../Resources").join(APP_ICON_FILE),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nsh-music");
    let cached = cache_dir.join(APP_ICON_FILE);
    let needs_write = std::fs::metadata(&cached)
        .map(|m| m.len() != APP_ICON_BYTES.len() as u64)
        .unwrap_or(true);
    if needs_write {
        let _ = std::fs::create_dir_all(&cache_dir);
        if std::fs::write(&cached, APP_ICON_BYTES).is_err() {
            return None;
        }
    }
    Some(cached)
}

#[cfg(any(
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "windows"
))]
const APP_URL_SCHEMES: &[&str] = &["nsh-music", "mygo-music"];

#[cfg(any(
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "windows"
))]
fn main() -> impl std::process::Termination {
    env_logger::init();
    platform::set_ui_sets(vec![nsh_base::registry(), nsh_app_music::registry()]);
    let app_id = "com.richerfu.nshmusic";

    #[cfg(target_os = "macos")]
    let options = platform::AppOptions {
        title: "NSH Music".to_owned(),
        app_id: app_id.to_owned(),
        icon: resolve_app_icon_path(),
        size: (1280, 820),
        keep_alive_on_close: true,
        ..Default::default()
    };

    #[cfg(target_os = "windows")]
    let options = platform::AppOptions {
        title: "NSH Music".to_owned(),
        app_id: app_id.to_owned(),
        icon: resolve_app_icon_path(),
        size: (1280, 820),
        schemes: APP_URL_SCHEMES.iter().map(|s| (*s).to_owned()).collect(),
        single_instance: true,
        ..Default::default()
    };

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    let options = platform::AppOptions {
        title: "NSH Music".to_owned(),
        app_id: app_id.to_owned(),
        size: (1280, 820),
        schemes: APP_URL_SCHEMES.iter().map(|s| (*s).to_owned()).collect(),
        single_instance: true,
        ..Default::default()
    };

    platform::run_app(
        platform::AppSource::Rust(
            platform::Root::of::<nsh_app_music::App>()
                .tokens(nsh_app_music::app_tokens())
                .app_id(app_id)
                .url_schemes(APP_URL_SCHEMES.iter().copied()),
        ),
        options,
    )
}

#[cfg(not(any(
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "windows"
)))]
compile_error!(
    "Run this binary on macOS, Windows or Linux; use `nsh run` for iOS, Android, HarmonyOS, and web."
);
