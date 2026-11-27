//! iOS native media integration (`MPNowPlayingInfoCenter` without macOS-only
//! `setPlaybackState:`, `MPRemoteCommandCenter`, `UIImage` artwork, and
//! `AVAudioSession` playback category + interruption & AirPods route-change observers).

#![cfg(target_os = "ios")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use nsh_music_audio::{RemoteBackend, RemoteCommand, RemoteController};
use objc2::AnyThread;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_foundation::{NSData, NSString};
use objc2_ui_kit::UIImage;

use crate::apple_common::{AppleNowPlayingDriver, ApplePlatform, wrap_media_item_artwork};

#[link(name = "AVFAudio", kind = "framework")]
unsafe extern "C" {
    static AVAudioSessionInterruptionNotification: *const NSString;
    static AVAudioSessionRouteChangeNotification: *const NSString;
    static AVAudioSessionInterruptionTypeKey: *const NSString;
    static AVAudioSessionInterruptionOptionKey: *const NSString;
    static AVAudioSessionRouteChangeReasonKey: *const NSString;
}

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {}

pub(crate) type IosNowPlayingDriver = AppleNowPlayingDriver;

pub(crate) struct IosSessionState {
    notification_observers: Vec<Retained<AnyObject>>,
    audio_session_activated: Arc<AtomicBool>,
}

pub(crate) struct IosPlatform;

impl ApplePlatform for IosPlatform {
    type SessionState = IosSessionState;

    fn backend() -> RemoteBackend {
        RemoteBackend::IosControlCenter
    }

    fn is_gui_active() -> bool {
        AnyClass::get(c"MPNowPlayingInfoCenter").is_some()
    }

    fn build_media_item_artwork(bytes: &[u8]) -> Option<Retained<AnyObject>> {
        let data = NSData::with_bytes(bytes);
        let img = UIImage::initWithData(UIImage::alloc(), &data)?;
        let sz = unsafe { img.size() };
        wrap_media_item_artwork(img, sz)
    }

    fn create_session_state(remote: &RemoteController) -> Self::SessionState {
        let audio_session_activated = Arc::new(AtomicBool::new(false));
        let notification_observers =
            register_ios_audio_session_observers(remote, Arc::clone(&audio_session_activated));
        IosSessionState {
            notification_observers,
            audio_session_activated,
        }
    }

    fn on_playing(state: &mut Self::SessionState) {
        if !state.audio_session_activated.load(Ordering::Acquire) && activate_ios_audio_session() {
            state.audio_session_activated.store(true, Ordering::Release);
        }
    }

    fn teardown_session_state(state: Self::SessionState) {
        if !state.notification_observers.is_empty()
            && let Some(nc_cls) = AnyClass::get(c"NSNotificationCenter")
        {
            unsafe {
                let nc: Option<Retained<AnyObject>> = msg_send![nc_cls, defaultCenter];
                if let Some(nc) = nc {
                    for observer in state.notification_observers {
                        let _: () = msg_send![&nc, removeObserver: &*observer];
                    }
                }
            }
        }
    }
}

unsafe fn notification_u64(
    user_info: &AnyObject,
    key_ptr: *const NSString,
    fallback_key: &str,
) -> Option<u64> {
    let fallback;
    let key: &NSString = if let Some(s) = unsafe { key_ptr.as_ref() } {
        s
    } else {
        fallback = NSString::from_str(fallback_key);
        &fallback
    };
    let value: Option<Retained<AnyObject>> = unsafe { msg_send![user_info, objectForKey: key] };
    let value = value?;
    let responds: Bool =
        unsafe { msg_send![&value, respondsToSelector: objc2::sel!(unsignedIntegerValue)] };
    if !responds.as_bool() {
        return None;
    }
    let num: usize = unsafe { msg_send![&value, unsignedIntegerValue] };
    Some(num as u64)
}

fn add_ios_notification_observer(
    nc: &AnyObject,
    queue_ptr: *const AnyObject,
    name_ptr: *const NSString,
    fallback_name: &str,
    handler: RcBlock<dyn Fn(*mut AnyObject)>,
) -> Option<Retained<AnyObject>> {
    let fallback;
    let name: &NSString = if let Some(s) = unsafe { name_ptr.as_ref() } {
        s
    } else {
        fallback = NSString::from_str(fallback_name);
        &fallback
    };
    unsafe {
        msg_send![
            nc,
            addObserverForName: name,
            object: std::ptr::null::<AnyObject>(),
            queue: queue_ptr,
            usingBlock: &*handler
        ]
    }
}

fn register_ios_audio_session_observers(
    remote: &RemoteController,
    audio_session_activated: Arc<AtomicBool>,
) -> Vec<Retained<AnyObject>> {
    let mut observers = Vec::new();
    let Some(nc_cls) = AnyClass::get(c"NSNotificationCenter") else {
        return observers;
    };
    let nc: Option<Retained<AnyObject>> = unsafe { msg_send![nc_cls, defaultCenter] };
    let Some(nc) = nc else {
        return observers;
    };
    let main_queue: Option<Retained<AnyObject>> =
        AnyClass::get(c"NSOperationQueue").and_then(|cls| unsafe { msg_send![cls, mainQueue] });
    let queue_ptr = main_queue
        .as_deref()
        .map_or(std::ptr::null(), |q| q as *const AnyObject);

    // Pause on audio session interruption began (phone call, Siri, alarm) and
    // resume when interruption ends with `AVAudioSessionInterruptionOptionShouldResume`.
    let remote_int = remote.clone();
    let activated_flag = Arc::clone(&audio_session_activated);
    let interruption_block = RcBlock::new(move |note_ptr: *mut AnyObject| {
        let Some(note) = (unsafe { note_ptr.as_ref() }) else {
            return;
        };
        let user_info: Option<Retained<AnyObject>> = unsafe { msg_send![note, userInfo] };
        let Some(user_info) = user_info else {
            return;
        };
        let Some(kind) = (unsafe {
            notification_u64(
                &user_info,
                AVAudioSessionInterruptionTypeKey,
                "AVAudioSessionInterruptionTypeKey",
            )
        }) else {
            return;
        };
        match kind {
            // AVAudioSessionInterruptionTypeBegan (1)
            1 => {
                activated_flag.store(false, Ordering::Release);
                remote_int.dispatch(RemoteCommand::Pause);
            }
            // AVAudioSessionInterruptionTypeEnded (0)
            0 => {
                let opts = unsafe {
                    notification_u64(
                        &user_info,
                        AVAudioSessionInterruptionOptionKey,
                        "AVAudioSessionInterruptionOptionKey",
                    )
                };
                // AVAudioSessionInterruptionOptionShouldResume (1)
                if opts.is_some_and(|v| v & 1 != 0) {
                    let activated = activate_ios_audio_session();
                    activated_flag.store(activated, Ordering::Release);
                    remote_int.dispatch(RemoteCommand::Play);
                }
            }
            _ => {}
        }
    });
    if let Some(tok) = add_ios_notification_observer(
        &nc,
        queue_ptr,
        unsafe { AVAudioSessionInterruptionNotification },
        "AVAudioSessionInterruptionNotification",
        interruption_block,
    ) {
        observers.push(tok);
    }

    // Pause when headphones / AirPods are unplugged (`OldDeviceUnavailable` == 2).
    let remote_route = remote.clone();
    let route_block = RcBlock::new(move |note_ptr: *mut AnyObject| {
        let Some(note) = (unsafe { note_ptr.as_ref() }) else {
            return;
        };
        let user_info: Option<Retained<AnyObject>> = unsafe { msg_send![note, userInfo] };
        let Some(user_info) = user_info else {
            return;
        };
        let reason = unsafe {
            notification_u64(
                &user_info,
                AVAudioSessionRouteChangeReasonKey,
                "AVAudioSessionRouteChangeReasonKey",
            )
        };
        if reason == Some(2) {
            remote_route.dispatch(RemoteCommand::Pause);
        }
    });
    if let Some(tok) = add_ios_notification_observer(
        &nc,
        queue_ptr,
        unsafe { AVAudioSessionRouteChangeNotification },
        "AVAudioSessionRouteChangeNotification",
        route_block,
    ) {
        observers.push(tok);
    }

    observers
}

fn activate_ios_audio_session() -> bool {
    let Some(cls) = AnyClass::get(c"AVAudioSession") else {
        return false;
    };
    unsafe {
        let session: Option<Retained<AnyObject>> = msg_send![cls, sharedInstance];
        let Some(session) = session else {
            return false;
        };
        let category = NSString::from_str("AVAudioSessionCategoryPlayback");
        let mut cat_err: *mut AnyObject = std::ptr::null_mut();
        let ok_cat: Bool = msg_send![&session, setCategory: &*category, error: &mut cat_err];
        if !ok_cat.as_bool() {
            log::warn!("[remote:ios] AVAudioSession setCategory playback failed");
        }
        let mut act_err: *mut AnyObject = std::ptr::null_mut();
        let ok_act: Bool = msg_send![&session, setActive: Bool::YES, error: &mut act_err];
        if !ok_act.as_bool() {
            log::warn!("[remote:ios] AVAudioSession setActive failed");
        }
        ok_act.as_bool()
    }
}
