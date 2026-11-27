//! macOS native media integration (`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`
//! + `NSImage` artwork + `NSApp` headless guard).

#![cfg(target_os = "macos")]

use nsh_music_audio::{RemoteBackend, RemoteController, RemotePlaybackStatus};
use objc2::AnyThread;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::NSData;

use crate::apple_common::{AppleNowPlayingDriver, ApplePlatform, wrap_media_item_artwork};
use crate::apple_playback_state;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSApp: *const AnyObject;
}

pub(crate) type MacOsNowPlayingDriver = AppleNowPlayingDriver;

pub(crate) struct MacOsSessionState;

pub(crate) struct MacOsPlatform;

impl ApplePlatform for MacOsPlatform {
    type SessionState = MacOsSessionState;

    fn backend() -> RemoteBackend {
        RemoteBackend::MacOsNowPlaying
    }

    fn is_gui_active() -> bool {
        // SAFETY: `NSApp` is AppKit's global `NSApplication *` pointer (null in headless tests).
        !unsafe { NSApp }.is_null()
    }

    fn build_media_item_artwork(bytes: &[u8]) -> Option<Retained<AnyObject>> {
        let data = NSData::with_bytes(bytes);
        let img = NSImage::initWithData(NSImage::alloc(), &data)?;
        let sz = img.size();
        wrap_media_item_artwork(img, CGSize::new(sz.width, sz.height))
    }

    fn apply_playback_state(center: &AnyObject, status: RemotePlaybackStatus) {
        let state = apple_playback_state(status);
        unsafe {
            let _: () = msg_send![center, setPlaybackState: state];
        }
    }

    fn clear_playback_state(center: &AnyObject) {
        unsafe {
            let _: () = msg_send![center, setPlaybackState: 3usize]; // Stopped
        }
    }

    fn create_session_state(_remote: &RemoteController) -> Self::SessionState {
        MacOsSessionState
    }
}
