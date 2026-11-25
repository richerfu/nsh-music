//! Standalone native desktop launcher for `nsh-music` (`cargo run`).

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> impl std::process::Termination {
    env_logger::init();
    platform::set_ui_sets(vec![nsh_base::registry(), nsh_app_music::registry()]);
    let app_id = "com.southorange.nshmusic";
    platform::run_app(
        platform::AppSource::Rust(
            platform::Root::of::<nsh_app_music::App>()
                .tokens(nsh_app_music::app_tokens())
                .app_id(app_id),
        ),
        platform::AppOptions {
            title: "NSH Music".to_owned(),
            app_id: app_id.to_owned(),
            size: (1280, 820),
            ..Default::default()
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!(
    "Run this binary on macOS, Windows or Linux; use `nsh run` for iOS, Android, HarmonyOS, and web."
);
