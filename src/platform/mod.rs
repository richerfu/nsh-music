//! Platform-specific native integrations (media session / notification bar controls).

#[cfg(target_env = "ohos")]
mod ohos_avsession;

use nsh_music_audio::RemoteController;

/// Installs the native OS media session driver onto `remote` when supported by
/// the active compilation target (e.g. HarmonyOS `AVSession`).
pub fn install_platform_remote(remote: &RemoteController) {
    #[cfg(target_env = "ohos")]
    {
        remote.set_driver(std::sync::Arc::new(
            ohos_avsession::HarmonyAvSessionDriver::new(remote.clone()),
        ));
    }
    #[cfg(not(target_env = "ohos"))]
    {
        let _ = remote;
    }
}
