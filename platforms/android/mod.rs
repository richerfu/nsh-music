//! Native Android `MediaSession` + `Notification.MediaStyle` driver via JNI.
//!
//! Spawns a dedicated JNI worker thread, discovers the process `JavaVM` via
//! `JNI_GetCreatedJavaVMs` (with `libart.so` fallback), acquires the host
//! `Application` context from `ActivityThread.currentApplication()`, and manages:
//! - `android.media.session.MediaSession` (metadata, playback state, launch `PendingIntent`,
//!   and interactive `MediaSession.Callback` via a build-time compiled `InMemoryDexClassLoader`
//!   Java bridge from `platforms/android/java/com/richerfu/nshmusic/NshMediaBridge.java`)
//! - `android.media.MediaMetadata` (title, artist, album, duration, `BitmapFactory.decodeFile`
//!   artwork + HTTP(S) `ALBUM_ART_URI` fallback)
//! - `android.media.session.PlaybackState` (state, position, speed, buffered position, actions,
//!   and lock-screen / Quick Settings `"favorite"` custom action)
//! - `android.app.Notification` (`Notification.MediaStyle`) on channel `"nsh_music_playback"`
//!   with interactive `Previous` / `Play|Pause` / `Next` broadcast actions, Android 13+
//!   `POST_NOTIFICATIONS` runtime prompt, and `PowerManager.PARTIAL_WAKE_LOCK` during playback.

#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::{JNI_OK, jint, jlong, jsize};
use jni::{JNIEnv, JavaVM, NativeMethod};
use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteController,
    RemotePlaybackStatus,
};

use crate::{
    android_playback_actions, android_playback_speed, android_playback_state, artwork_http_url,
    artwork_local_path,
};

/// DEX compiled from `platforms/android/java/com/richerfu/nshmusic/NshMediaBridge.java`
/// at build time by `platforms/build.rs`.
const MEDIA_BRIDGE_DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nsh_media_bridge.dex"));
const MEDIA_BRIDGE_CLASS: &str = "com.richerfu.nshmusic.NshMediaBridge";
const MEDIA_ACTION_INTENT: &str = "com.richerfu.nshmusic.MEDIA_ACTION";
const MEDIA_EXTRA_CMD: &str = "cmd";

const NOTIFICATION_CHANNEL_ID: &str = "nsh_music_playback";
const NOTIFICATION_CHANNEL_NAME: &str = "Music Playback";
const NOTIFICATION_ID: jint = 0x4E53_4801;
/// `NotificationManager.IMPORTANCE_LOW`
const IMPORTANCE_LOW: jint = 2;
/// `Notification.VISIBILITY_PUBLIC`
const VISIBILITY_PUBLIC: jint = 1;
/// `PendingIntent.FLAG_UPDATE_CURRENT`
const FLAG_UPDATE_CURRENT: jint = 0x0800_0000;
/// `PendingIntent.FLAG_IMMUTABLE` (API >= 23)
const FLAG_IMMUTABLE: jint = 0x0400_0000;
/// `MediaSession.FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS`
const SESSION_FLAGS: jint = 3;
/// `PowerManager.PARTIAL_WAKE_LOCK`
const PARTIAL_WAKE_LOCK: jint = 1;

/// Android system drawable icons for notification actions & custom playback actions.
const IC_MEDIA_PLAY: jint = 0x0108_0024;
const IC_MEDIA_PAUSE: jint = 0x0108_0023;
const IC_MEDIA_NEXT: jint = 0x0108_0022;
const IC_MEDIA_PREVIOUS: jint = 0x0108_0025;
const IC_STAR_ON: jint = 0x0108_000c;
const IC_STAR_OFF: jint = 0x0108_000b;
const FALLBACK_SMALL_ICON: jint = IC_MEDIA_PLAY;

const CMD_PLAY: jint = 1;
const CMD_PAUSE: jint = 2;
const CMD_STOP: jint = 3;
const CMD_NEXT: jint = 4;
const CMD_PREV: jint = 5;
const CMD_SEEK: jint = 6;
const CMD_FAVORITE: jint = 7;
const CMD_LOOP: jint = 8;
const CMD_TOGGLE_PLAY_PAUSE: jint = 9;

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVE_REMOTE: Mutex<Option<RemoteController>> = Mutex::new(None);
static NOTIF_PERMISSION_PROMPTED: AtomicBool = AtomicBool::new(false);

/// JNI callback target invoked from `com.richerfu.nshmusic.NshMediaBridge.nativeOnAction`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_richerfu_nshmusic_NshMediaBridge_nativeOnAction(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    action: jint,
    arg: jlong,
) {
    let remote = ACTIVE_REMOTE.lock().ok().and_then(|g| g.clone());
    let Some(remote) = remote else {
        return;
    };
    match action {
        CMD_PLAY => remote.dispatch(RemoteCommand::Play),
        CMD_PAUSE => remote.dispatch(RemoteCommand::Pause),
        CMD_STOP => remote.dispatch(RemoteCommand::Stop),
        CMD_NEXT => remote.dispatch(RemoteCommand::Next),
        CMD_PREV => remote.dispatch(RemoteCommand::Prev),
        CMD_SEEK => remote.dispatch(RemoteCommand::Seek(arg.max(0) as u64)),
        CMD_FAVORITE => remote.dispatch(RemoteCommand::ToggleFavorite),
        CMD_LOOP => {
            let next = remote.metadata().loop_mode.next_cycle();
            remote.dispatch(RemoteCommand::SetLoopMode(next));
        }
        CMD_TOGGLE_PLAY_PAUSE => remote.dispatch(RemoteCommand::TogglePlayPause),
        _ => {}
    }
}

fn discover_java_vm() -> Option<&'static JavaVM> {
    if let Some(vm) = JAVA_VM.get() {
        return Some(vm);
    }

    type GetCreatedVmsFn =
        unsafe extern "C" fn(*mut *mut jni::sys::JavaVM, jsize, *mut jsize) -> jint;

    let mut sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"JNI_GetCreatedJavaVMs".as_ptr()) };
    if sym.is_null() {
        let art = unsafe { libc::dlopen(c"libart.so".as_ptr(), libc::RTLD_LAZY) };
        if !art.is_null() {
            sym = unsafe { libc::dlsym(art, c"JNI_GetCreatedJavaVMs".as_ptr()) };
        }
    }
    if sym.is_null() {
        return None;
    }

    let get_vms: GetCreatedVmsFn = unsafe { std::mem::transmute(sym) };
    let mut vm_ptr: *mut jni::sys::JavaVM = std::ptr::null_mut();
    let mut count: jsize = 0;
    let status = unsafe { get_vms(&mut vm_ptr, 1, &mut count) };
    if status != JNI_OK || count < 1 || vm_ptr.is_null() {
        return None;
    }

    let wrapped = unsafe { JavaVM::from_raw(vm_ptr) }.ok()?;
    let _ = JAVA_VM.set(wrapped);
    JAVA_VM.get()
}

#[derive(Default)]
struct PendingState {
    metadata: Option<NowPlayingMetadata>,
    meta_dirty: bool,
    playback_dirty: bool,
    clear_requested: bool,
    shutdown: bool,
}

struct SharedState {
    remote: RemoteController,
    pending: Mutex<PendingState>,
    wake: Condvar,
}

/// Android `MediaSession` + `Notification.MediaStyle` driver.
pub(crate) struct AndroidMediaSessionDriver {
    shared: Arc<SharedState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AndroidMediaSessionDriver {
    pub(crate) fn new(remote: RemoteController) -> Self {
        if let Ok(mut slot) = ACTIVE_REMOTE.lock() {
            *slot = Some(remote.clone());
        }
        let shared = Arc::new(SharedState {
            remote,
            pending: Mutex::new(PendingState::default()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("nsh-android-mediasession".into())
            .spawn(move || run_android_worker(worker_shared))
            .ok();
        Self {
            shared,
            worker: Mutex::new(handle),
        }
    }
}

impl PlatformRemoteDriver for AndroidMediaSessionDriver {
    fn backend(&self) -> RemoteBackend {
        RemoteBackend::AndroidMediaSession
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = Some(meta.clone());
            pending.meta_dirty = true;
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
    }

    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            let meta = pending
                .metadata
                .get_or_insert_with(|| self.shared.remote.metadata());
            meta.is_playing = is_playing;
            meta.status = if is_playing {
                RemotePlaybackStatus::Playing
            } else {
                RemotePlaybackStatus::Paused
            };
            meta.position_ms = position_ms;
            if duration_ms > 0 {
                meta.duration_ms = duration_ms;
            }
            meta.playback_rate_milli = if is_playing { 1000 } else { 0 };
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
    }

    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            let meta_changed = pending.metadata.as_ref().is_none_or(|prev| {
                prev.track_id != meta.track_id
                    || prev.title != meta.title
                    || prev.artist != meta.artist
                    || prev.album != meta.album
                    || prev.artwork_url != meta.artwork_url
                    || prev.duration_ms != meta.duration_ms
                    || prev.is_favorite != meta.is_favorite
            });
            pending.metadata = Some(meta.clone());
            if meta_changed {
                pending.meta_dirty = true;
            }
            pending.playback_dirty = true;
            pending.clear_requested = false;
            self.shared.wake.notify_one();
        }
    }

    fn clear(&self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = None;
            pending.meta_dirty = false;
            pending.playback_dirty = false;
            pending.clear_requested = true;
            self.shared.wake.notify_one();
        }
    }
}

impl Drop for AndroidMediaSessionDriver {
    fn drop(&mut self) {
        if let Ok(mut slot) = ACTIVE_REMOTE.lock() {
            *slot = None;
        }
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.shutdown = true;
            self.shared.wake.notify_all();
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

struct AndroidSessionRefs {
    context: GlobalRef,
    session: GlobalRef,
    session_token: GlobalRef,
    notif_mgr: Option<GlobalRef>,
    launch_intent: Option<GlobalRef>,
    bridge_class: Option<GlobalRef>,
    _session_callback: Option<GlobalRef>,
    action_receiver: Option<GlobalRef>,
    wake_lock: Option<GlobalRef>,
    wake_lock_held: bool,
    session_active: bool,
    small_icon: jint,
    sdk_int: jint,
    cached_artwork_path: String,
    cached_artwork_bitmap: Option<GlobalRef>,
    last_notif_playing: Option<bool>,
    last_notif_track_id: String,
    last_notif_artwork: String,
    last_notif_fav: Option<bool>,
    notif_posted: bool,
}

fn run_android_worker(shared: Arc<SharedState>) {
    let mut session_refs: Option<AndroidSessionRefs> = None;

    loop {
        let (meta_opt, meta_dirty, playback_dirty, clear_requested, shutdown) = {
            let Ok(mut pending) = shared.pending.lock() else {
                break;
            };
            while !pending.shutdown
                && !pending.clear_requested
                && !pending.meta_dirty
                && !pending.playback_dirty
            {
                let Ok(next) = shared.wake.wait(pending) else {
                    return;
                };
                pending = next;
            }
            let out = (
                pending.metadata.clone(),
                pending.meta_dirty,
                pending.playback_dirty,
                pending.clear_requested,
                pending.shutdown,
            );
            pending.meta_dirty = false;
            pending.playback_dirty = false;
            pending.clear_requested = false;
            out
        };

        if shutdown {
            break;
        }

        let Some(vm) = discover_java_vm() else {
            continue;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            continue;
        };

        let _ = env.with_local_frame(64, |env| -> jni::errors::Result<()> {
            if clear_requested {
                if let Some(refs) = session_refs.as_mut() {
                    clear_android_session(env, refs);
                }
                return Ok(());
            }

            let Some(meta) = meta_opt else {
                return Ok(());
            };

            if session_refs.is_none() {
                session_refs = create_android_session(env);
            }
            let Some(refs) = session_refs.as_mut() else {
                return Ok(());
            };

            let playing = meta.status.is_playing();
            sync_wake_lock(env, refs, playing);

            if refs.sdk_int >= 33
                && !NOTIF_PERMISSION_PROMPTED.load(Ordering::Relaxed)
                && !can_post_notifications(env, refs)
            {
                maybe_request_post_notifications(env, refs);
            }

            let mut need_notif = meta_dirty || !refs.notif_posted;
            if meta_dirty {
                push_android_metadata(env, refs, &meta);
            }
            if playback_dirty {
                push_android_playback(env, refs, &meta);
                if refs.last_notif_playing != Some(playing)
                    || refs.last_notif_track_id != meta.track_id
                    || refs.last_notif_artwork != meta.artwork_url
                    || refs.last_notif_fav != Some(meta.is_favorite)
                {
                    need_notif = true;
                }
            }
            if need_notif {
                if post_media_notification(env, refs, &meta) {
                    refs.notif_posted = true;
                    refs.last_notif_playing = Some(playing);
                    refs.last_notif_track_id = meta.track_id.clone();
                    refs.last_notif_artwork = meta.artwork_url.clone();
                    refs.last_notif_fav = Some(meta.is_favorite);
                } else {
                    refs.notif_posted = false;
                }
            }
            Ok(())
        });
    }

    if let Some(mut refs) = session_refs
        && let Some(vm) = discover_java_vm()
        && let Ok(mut env) = vm.attach_current_thread()
    {
        let _ = env.with_local_frame(32, |env| -> jni::errors::Result<()> {
            release_android_session(env, &mut refs);
            Ok(())
        });
    }
}

fn create_android_session(env: &mut JNIEnv<'_>) -> Option<AndroidSessionRefs> {
    let app_val = env
        .call_static_method(
            "android/app/ActivityThread",
            "currentApplication",
            "()Landroid/app/Application;",
            &[],
        )
        .ok()?;
    if clear_pending_exception(env) {
        return None;
    }
    let app_obj = app_val.l().ok()?;
    if app_obj.is_null() {
        return None;
    }

    let sdk_int = env
        .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .ok()
        .and_then(|v| v.i().ok())
        .unwrap_or(26);
    clear_pending_exception(env);

    let tag = env.new_string("NshMusicSession").ok()?;
    let session_obj = env
        .new_object(
            "android/media/session/MediaSession",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(&app_obj), JValue::Object(&tag)],
        )
        .ok()?;
    if clear_pending_exception(env) || session_obj.is_null() {
        return None;
    }

    let _ = env.call_method(
        &session_obj,
        "setFlags",
        "(I)V",
        &[JValue::Int(SESSION_FLAGS)],
    );
    clear_pending_exception(env);

    let launch_intent = build_launch_pending_intent(env, &app_obj, sdk_int);
    if let Some(ref pi) = launch_intent {
        let _ = env.call_method(
            &session_obj,
            "setSessionActivity",
            "(Landroid/app/PendingIntent;)V",
            &[JValue::Object(pi.as_obj())],
        );
        clear_pending_exception(env);
    }

    let (bridge_class, session_callback, action_receiver) =
        install_media_bridge(env, &app_obj, &session_obj);

    let token_obj = env
        .call_method(
            &session_obj,
            "getSessionToken",
            "()Landroid/media/session/MediaSession$Token;",
            &[],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || token_obj.is_null() {
        return None;
    }

    let notif_mgr = obtain_notification_manager(env, &app_obj, sdk_int);
    let wake_lock = obtain_partial_wake_lock(env, &app_obj);
    let small_icon = resolve_app_icon(env, &app_obj);

    Some(AndroidSessionRefs {
        context: env.new_global_ref(&app_obj).ok()?,
        session: env.new_global_ref(&session_obj).ok()?,
        session_token: env.new_global_ref(&token_obj).ok()?,
        notif_mgr,
        launch_intent,
        bridge_class,
        _session_callback: session_callback,
        action_receiver,
        wake_lock,
        wake_lock_held: false,
        session_active: false,
        small_icon,
        sdk_int,
        cached_artwork_path: String::new(),
        cached_artwork_bitmap: None,
        last_notif_playing: None,
        last_notif_track_id: String::new(),
        last_notif_artwork: String::new(),
        last_notif_fav: None,
        notif_posted: false,
    })
}

fn install_media_bridge(
    env: &mut JNIEnv<'_>,
    context: &JObject<'_>,
    session: &JObject<'_>,
) -> (Option<GlobalRef>, Option<GlobalRef>, Option<GlobalRef>) {
    let Ok(dex_arr) = env.byte_array_from_slice(MEDIA_BRIDGE_DEX) else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    let dex_obj = JObject::from(dex_arr);
    let Ok(buf_val) = env.call_static_method(
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
        &[JValue::Object(&dex_obj)],
    ) else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    let Ok(buf_obj) = buf_val.l() else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    if clear_pending_exception(env) || buf_obj.is_null() {
        return (None, None, None);
    }

    let Ok(parent_loader_val) =
        env.call_method(context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
    else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    let Ok(parent_loader) = parent_loader_val.l() else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    if clear_pending_exception(env) || parent_loader.is_null() {
        return (None, None, None);
    }

    let Ok(dex_loader) = env.new_object(
        "dalvik/system/InMemoryDexClassLoader",
        "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
        &[JValue::Object(&buf_obj), JValue::Object(&parent_loader)],
    ) else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    if clear_pending_exception(env) || dex_loader.is_null() {
        return (None, None, None);
    }

    let Ok(jcls_name) = env.new_string(MEDIA_BRIDGE_CLASS) else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    let Ok(cls_val) = env.call_method(
        &dex_loader,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&jcls_name)],
    ) else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    let Ok(cls_obj) = cls_val.l() else {
        clear_pending_exception(env);
        return (None, None, None);
    };
    if clear_pending_exception(env) || cls_obj.is_null() {
        return (None, None, None);
    }

    let bridge_cls = JClass::from(cls_obj);
    let native_methods = [NativeMethod {
        name: "nativeOnAction".into(),
        sig: "(IJ)V".into(),
        fn_ptr: Java_com_richerfu_nshmusic_NshMediaBridge_nativeOnAction as *mut c_void,
    }];
    if env
        .register_native_methods(&bridge_cls, &native_methods)
        .is_err()
        || clear_pending_exception(env)
    {
        return (None, None, None);
    }

    let cb_ref = env
        .call_static_method(
            &bridge_cls,
            "attachCallback",
            "(Landroid/media/session/MediaSession;)Landroid/media/session/MediaSession$Callback;",
            &[JValue::Object(session)],
        )
        .ok()
        .and_then(|v| v.l().ok())
        .filter(|obj| !clear_pending_exception(env) && !obj.is_null())
        .and_then(|obj| env.new_global_ref(&obj).ok());

    let rx_ref = env
        .call_static_method(
            &bridge_cls,
            "registerActionReceiver",
            "(Landroid/content/Context;)Landroid/content/BroadcastReceiver;",
            &[JValue::Object(context)],
        )
        .ok()
        .and_then(|v| v.l().ok())
        .filter(|obj| !clear_pending_exception(env) && !obj.is_null())
        .and_then(|obj| env.new_global_ref(&obj).ok());

    let bridge_ref = env.new_global_ref(&bridge_cls).ok();
    (bridge_ref, cb_ref, rx_ref)
}

fn maybe_request_post_notifications(env: &mut JNIEnv<'_>, refs: &AndroidSessionRefs) {
    let Some(ref bridge_ref) = refs.bridge_class else {
        return;
    };
    let Ok(local_cls_obj) = env.new_local_ref(bridge_ref.as_obj()) else {
        clear_pending_exception(env);
        return;
    };
    let bridge_cls = JClass::from(local_cls_obj);
    let prompted = env
        .call_static_method(
            &bridge_cls,
            "requestPostNotifications",
            "(Landroid/content/Context;)Z",
            &[JValue::Object(refs.context.as_obj())],
        )
        .ok()
        .and_then(|v| v.z().ok())
        .unwrap_or(false);
    if !clear_pending_exception(env) && prompted {
        NOTIF_PERMISSION_PROMPTED.store(true, Ordering::Relaxed);
    }
}

fn obtain_partial_wake_lock(env: &mut JNIEnv<'_>, context: &JObject<'_>) -> Option<GlobalRef> {
    let svc_name = env.new_string("power").ok()?;
    let pm = env
        .call_method(
            context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&svc_name)],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || pm.is_null() {
        return None;
    }
    let tag = env.new_string("nshmusic:playback").ok()?;
    let wl = env
        .call_method(
            &pm,
            "newWakeLock",
            "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
            &[JValue::Int(PARTIAL_WAKE_LOCK), JValue::Object(&tag)],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || wl.is_null() {
        return None;
    }
    let _ = env.call_method(&wl, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)]);
    clear_pending_exception(env);
    env.new_global_ref(&wl).ok()
}

fn sync_wake_lock(env: &mut JNIEnv<'_>, refs: &mut AndroidSessionRefs, want_held: bool) {
    let Some(ref wl) = refs.wake_lock else {
        return;
    };
    if want_held && !refs.wake_lock_held {
        let _ = env.call_method(wl.as_obj(), "acquire", "()V", &[]);
        if !clear_pending_exception(env) {
            refs.wake_lock_held = true;
        }
    } else if !want_held && refs.wake_lock_held {
        let is_held = env
            .call_method(wl.as_obj(), "isHeld", "()Z", &[])
            .ok()
            .and_then(|v| v.z().ok())
            .unwrap_or(false);
        clear_pending_exception(env);
        if is_held {
            let _ = env.call_method(wl.as_obj(), "release", "()V", &[]);
            clear_pending_exception(env);
        }
        refs.wake_lock_held = false;
    }
}

fn build_launch_pending_intent(
    env: &mut JNIEnv<'_>,
    context: &JObject<'_>,
    sdk_int: jint,
) -> Option<GlobalRef> {
    let pkg = env
        .call_method(context, "getPackageName", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|v| v.l().ok())?;
    let pm = env
        .call_method(
            context,
            "getPackageManager",
            "()Landroid/content/pm/PackageManager;",
            &[],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || pkg.is_null() || pm.is_null() {
        return None;
    }
    let intent = env
        .call_method(
            &pm,
            "getLaunchIntentForPackage",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&pkg)],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || intent.is_null() {
        return None;
    }
    let flags = if sdk_int >= 23 {
        FLAG_UPDATE_CURRENT | FLAG_IMMUTABLE
    } else {
        FLAG_UPDATE_CURRENT
    };
    let pi = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[
                JValue::Object(context),
                JValue::Int(0),
                JValue::Object(&intent),
                JValue::Int(flags),
            ],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || pi.is_null() {
        return None;
    }
    env.new_global_ref(&pi).ok()
}

fn obtain_notification_manager(
    env: &mut JNIEnv<'_>,
    context: &JObject<'_>,
    sdk_int: jint,
) -> Option<GlobalRef> {
    let svc_name = env.new_string("notification").ok()?;
    let mgr = env
        .call_method(
            context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&svc_name)],
        )
        .ok()
        .and_then(|v| v.l().ok())?;
    if clear_pending_exception(env) || mgr.is_null() {
        return None;
    }

    if sdk_int >= 26
        && let (Ok(ch_id), Ok(ch_name)) = (
            env.new_string(NOTIFICATION_CHANNEL_ID),
            env.new_string(NOTIFICATION_CHANNEL_NAME),
        )
        && let Ok(channel) = env.new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[
                JValue::Object(&ch_id),
                JValue::Object(&ch_name),
                JValue::Int(IMPORTANCE_LOW),
            ],
        )
        && !clear_pending_exception(env)
        && !channel.is_null()
    {
        let _ = env.call_method(
            &mgr,
            "createNotificationChannel",
            "(Landroid/app/NotificationChannel;)V",
            &[JValue::Object(&channel)],
        );
        clear_pending_exception(env);
    }

    env.new_global_ref(&mgr).ok()
}

fn resolve_app_icon(env: &mut JNIEnv<'_>, context: &JObject<'_>) -> jint {
    let icon = env
        .call_method(
            context,
            "getApplicationInfo",
            "()Landroid/content/pm/ApplicationInfo;",
            &[],
        )
        .ok()
        .and_then(|v| v.l().ok())
        .and_then(|info| {
            if info.is_null() {
                None
            } else {
                env.get_field(&info, "icon", "I").ok()?.i().ok()
            }
        })
        .unwrap_or(0);
    clear_pending_exception(env);
    if icon != 0 { icon } else { FALLBACK_SMALL_ICON }
}

fn refresh_cached_bitmap(env: &mut JNIEnv<'_>, refs: &mut AndroidSessionRefs, artwork_url: &str) {
    let next_path = artwork_local_path(artwork_url)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if next_path == refs.cached_artwork_path {
        return;
    }
    refs.cached_artwork_path = next_path.clone();
    refs.cached_artwork_bitmap = None;
    if next_path.is_empty() {
        return;
    }
    let Ok(jpath) = env.new_string(&next_path) else {
        return;
    };
    let bitmap = env
        .call_static_method(
            "android/graphics/BitmapFactory",
            "decodeFile",
            "(Ljava/lang/String;)Landroid/graphics/Bitmap;",
            &[JValue::Object(&jpath)],
        )
        .ok()
        .and_then(|v| v.l().ok());
    if clear_pending_exception(env) {
        return;
    }
    if let Some(bmp) = bitmap
        && !bmp.is_null()
    {
        refs.cached_artwork_bitmap = env.new_global_ref(&bmp).ok();
    }
}

fn push_android_metadata(
    env: &mut JNIEnv<'_>,
    refs: &mut AndroidSessionRefs,
    meta: &NowPlayingMetadata,
) {
    refresh_cached_bitmap(env, refs, &meta.artwork_url);

    let Ok(builder) = env.new_object("android/media/MediaMetadata$Builder", "()V", &[]) else {
        clear_pending_exception(env);
        return;
    };
    if clear_pending_exception(env) || builder.is_null() {
        return;
    }

    put_metadata_string(
        env,
        &builder,
        "android.media.metadata.MEDIA_ID",
        &meta.track_id,
    );
    put_metadata_string(env, &builder, "android.media.metadata.TITLE", &meta.title);
    put_metadata_string(env, &builder, "android.media.metadata.ARTIST", &meta.artist);
    put_metadata_string(env, &builder, "android.media.metadata.ALBUM", &meta.album);

    if let Some(http_art) = artwork_http_url(&meta.artwork_url) {
        put_metadata_string(
            env,
            &builder,
            "android.media.metadata.ALBUM_ART_URI",
            http_art,
        );
    }

    if let Ok(key) = env.new_string("android.media.metadata.DURATION") {
        let _ = env.call_method(
            &builder,
            "putLong",
            "(Ljava/lang/String;J)Landroid/media/MediaMetadata$Builder;",
            &[
                JValue::Object(&key),
                JValue::Long(meta.duration_ms.min(i64::MAX as u64) as i64),
            ],
        );
        clear_pending_exception(env);
    }

    if let Some(ref bmp) = refs.cached_artwork_bitmap
        && let Ok(key) = env.new_string("android.media.metadata.ALBUM_ART")
    {
        let _ = env.call_method(
            &builder,
            "putBitmap",
            "(Ljava/lang/String;Landroid/graphics/Bitmap;)Landroid/media/MediaMetadata$Builder;",
            &[JValue::Object(&key), JValue::Object(bmp.as_obj())],
        );
        clear_pending_exception(env);
    }

    if let Ok(val) = env.call_method(&builder, "build", "()Landroid/media/MediaMetadata;", &[])
        && !clear_pending_exception(env)
        && let Ok(metadata_obj) = val.l()
        && !metadata_obj.is_null()
    {
        let _ = env.call_method(
            refs.session.as_obj(),
            "setMetadata",
            "(Landroid/media/MediaMetadata;)V",
            &[JValue::Object(&metadata_obj)],
        );
        clear_pending_exception(env);
        let _ = env.call_method(
            refs.session.as_obj(),
            "setActive",
            "(Z)V",
            &[JValue::Bool(1)],
        );
        if !clear_pending_exception(env) {
            refs.session_active = true;
        }
    }
}

fn put_metadata_string(env: &mut JNIEnv<'_>, builder: &JObject<'_>, key: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if let (Ok(jk), Ok(jv)) = (env.new_string(key), env.new_string(value)) {
        let _ = env.call_method(
            builder,
            "putString",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/media/MediaMetadata$Builder;",
            &[JValue::Object(&jk), JValue::Object(&jv)],
        );
        clear_pending_exception(env);
    }
}

fn push_android_playback(
    env: &mut JNIEnv<'_>,
    refs: &mut AndroidSessionRefs,
    meta: &NowPlayingMetadata,
) {
    let Ok(builder) = env.new_object("android/media/session/PlaybackState$Builder", "()V", &[])
    else {
        clear_pending_exception(env);
        return;
    };
    if clear_pending_exception(env) || builder.is_null() {
        return;
    }

    let _ = env.call_method(
        &builder,
        "setActions",
        "(J)Landroid/media/session/PlaybackState$Builder;",
        &[JValue::Long(android_playback_actions(meta))],
    );
    clear_pending_exception(env);

    let _ = env.call_method(
        &builder,
        "setState",
        "(IJF)Landroid/media/session/PlaybackState$Builder;",
        &[
            JValue::Int(android_playback_state(meta.status)),
            JValue::Long(meta.position_ms.min(i64::MAX as u64) as i64),
            JValue::Float(android_playback_speed(meta)),
        ],
    );
    clear_pending_exception(env);

    let _ = env.call_method(
        &builder,
        "setBufferedPosition",
        "(J)Landroid/media/session/PlaybackState$Builder;",
        &[JValue::Long(meta.buffered_ms.min(i64::MAX as u64) as i64)],
    );
    clear_pending_exception(env);

    // Expose a Favorite toggle in Android 13/14 Quick Settings & Lock Screen media card.
    if let (Ok(act_id), Ok(act_name)) = (env.new_string("favorite"), env.new_string("Favorite")) {
        let fav_icon = if meta.is_favorite {
            IC_STAR_ON
        } else {
            IC_STAR_OFF
        };
        if let Ok(ca_builder) = env.new_object(
            "android/media/session/PlaybackState$CustomAction$Builder",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[
                JValue::Object(&act_id),
                JValue::Object(&act_name),
                JValue::Int(fav_icon),
            ],
        ) && !clear_pending_exception(env)
            && !ca_builder.is_null()
            && let Ok(ca_val) = env.call_method(
                &ca_builder,
                "build",
                "()Landroid/media/session/PlaybackState$CustomAction;",
                &[],
            )
            && !clear_pending_exception(env)
            && let Ok(ca_obj) = ca_val.l()
            && !ca_obj.is_null()
        {
            let _ = env.call_method(
                &builder,
                "addCustomAction",
                "(Landroid/media/session/PlaybackState$CustomAction;)Landroid/media/session/PlaybackState$Builder;",
                &[JValue::Object(&ca_obj)],
            );
            clear_pending_exception(env);
        }
    }

    if let Ok(val) = env.call_method(
        &builder,
        "build",
        "()Landroid/media/session/PlaybackState;",
        &[],
    ) && !clear_pending_exception(env)
        && let Ok(state_obj) = val.l()
        && !state_obj.is_null()
    {
        let _ = env.call_method(
            refs.session.as_obj(),
            "setPlaybackState",
            "(Landroid/media/session/PlaybackState;)V",
            &[JValue::Object(&state_obj)],
        );
        clear_pending_exception(env);
        if !refs.session_active {
            let _ = env.call_method(
                refs.session.as_obj(),
                "setActive",
                "(Z)V",
                &[JValue::Bool(1)],
            );
            if !clear_pending_exception(env) {
                refs.session_active = true;
            }
        }
    }
}

fn can_post_notifications(env: &mut JNIEnv<'_>, refs: &AndroidSessionRefs) -> bool {
    if refs.sdk_int < 33 {
        return true;
    }
    let Ok(perm) = env.new_string("android.permission.POST_NOTIFICATIONS") else {
        return false;
    };
    let granted = env
        .call_method(
            refs.context.as_obj(),
            "checkSelfPermission",
            "(Ljava/lang/String;)I",
            &[JValue::Object(&perm)],
        )
        .ok()
        .and_then(|v| v.i().ok())
        .unwrap_or(-1);
    clear_pending_exception(env);
    granted == 0
}

struct NotifAction<'a> {
    req_code: jint,
    cmd: jint,
    icon_res: jint,
    title: &'a str,
}

fn add_notification_action(
    env: &mut JNIEnv<'_>,
    refs: &AndroidSessionRefs,
    builder: &JObject<'_>,
    pkg: &JObject<'_>,
    action: &NotifAction<'_>,
) {
    let (Ok(action_str), Ok(extra_key), Ok(title_str)) = (
        env.new_string(MEDIA_ACTION_INTENT),
        env.new_string(MEDIA_EXTRA_CMD),
        env.new_string(action.title),
    ) else {
        clear_pending_exception(env);
        return;
    };
    let Ok(intent) = env.new_object(
        "android/content/Intent",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&action_str)],
    ) else {
        clear_pending_exception(env);
        return;
    };
    if clear_pending_exception(env) || intent.is_null() {
        return;
    }
    let _ = env.call_method(
        &intent,
        "setPackage",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(pkg)],
    );
    clear_pending_exception(env);
    let _ = env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;I)Landroid/content/Intent;",
        &[JValue::Object(&extra_key), JValue::Int(action.cmd)],
    );
    clear_pending_exception(env);

    let flags = if refs.sdk_int >= 23 {
        FLAG_UPDATE_CURRENT | FLAG_IMMUTABLE
    } else {
        FLAG_UPDATE_CURRENT
    };
    let Ok(pi_val) = env.call_static_method(
        "android/app/PendingIntent",
        "getBroadcast",
        "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
        &[
            JValue::Object(refs.context.as_obj()),
            JValue::Int(action.req_code),
            JValue::Object(&intent),
            JValue::Int(flags),
        ],
    ) else {
        clear_pending_exception(env);
        return;
    };
    let Ok(pi_obj) = pi_val.l() else {
        clear_pending_exception(env);
        return;
    };
    if clear_pending_exception(env) || pi_obj.is_null() {
        return;
    }

    if let Ok(act_builder) = env.new_object(
        "android/app/Notification$Action$Builder",
        "(ILjava/lang/CharSequence;Landroid/app/PendingIntent;)V",
        &[
            JValue::Int(action.icon_res),
            JValue::Object(&title_str),
            JValue::Object(&pi_obj),
        ],
    ) && !clear_pending_exception(env)
        && !act_builder.is_null()
        && let Ok(act_val) = env.call_method(
            &act_builder,
            "build",
            "()Landroid/app/Notification$Action;",
            &[],
        )
        && !clear_pending_exception(env)
        && let Ok(act_obj) = act_val.l()
        && !act_obj.is_null()
    {
        let _ = env.call_method(
            builder,
            "addAction",
            "(Landroid/app/Notification$Action;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&act_obj)],
        );
        clear_pending_exception(env);
    }
}

fn post_media_notification(
    env: &mut JNIEnv<'_>,
    refs: &AndroidSessionRefs,
    meta: &NowPlayingMetadata,
) -> bool {
    let Some(ref mgr) = refs.notif_mgr else {
        return false;
    };
    if !can_post_notifications(env, refs) {
        return false;
    }

    let builder = if refs.sdk_int >= 26 {
        let Ok(ch_id) = env.new_string(NOTIFICATION_CHANNEL_ID) else {
            return false;
        };
        env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[
                JValue::Object(refs.context.as_obj()),
                JValue::Object(&ch_id),
            ],
        )
    } else {
        env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;)V",
            &[JValue::Object(refs.context.as_obj())],
        )
    };
    let Ok(builder) = builder else {
        clear_pending_exception(env);
        return false;
    };
    if clear_pending_exception(env) || builder.is_null() {
        return false;
    }

    if let Ok(title) = env.new_string(&meta.title) {
        let _ = env.call_method(
            &builder,
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&title)],
        );
        clear_pending_exception(env);
    }
    if !meta.artist.is_empty()
        && let Ok(artist) = env.new_string(&meta.artist)
    {
        let _ = env.call_method(
            &builder,
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&artist)],
        );
        clear_pending_exception(env);
    }
    if !meta.album.is_empty()
        && let Ok(album) = env.new_string(&meta.album)
    {
        let _ = env.call_method(
            &builder,
            "setSubText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&album)],
        );
        clear_pending_exception(env);
    }

    let _ = env.call_method(
        &builder,
        "setSmallIcon",
        "(I)Landroid/app/Notification$Builder;",
        &[JValue::Int(refs.small_icon)],
    );
    clear_pending_exception(env);

    if let Some(ref bmp) = refs.cached_artwork_bitmap {
        let _ = env.call_method(
            &builder,
            "setLargeIcon",
            "(Landroid/graphics/Bitmap;)Landroid/app/Notification$Builder;",
            &[JValue::Object(bmp.as_obj())],
        );
        clear_pending_exception(env);
    }

    let playing = meta.status.is_playing();
    let _ = env.call_method(
        &builder,
        "setOngoing",
        "(Z)Landroid/app/Notification$Builder;",
        &[JValue::Bool(u8::from(playing))],
    );
    clear_pending_exception(env);

    let _ = env.call_method(
        &builder,
        "setOnlyAlertOnce",
        "(Z)Landroid/app/Notification$Builder;",
        &[JValue::Bool(1)],
    );
    clear_pending_exception(env);

    let _ = env.call_method(
        &builder,
        "setVisibility",
        "(I)Landroid/app/Notification$Builder;",
        &[JValue::Int(VISIBILITY_PUBLIC)],
    );
    clear_pending_exception(env);

    if let Ok(category) = env.new_string("transport") {
        let _ = env.call_method(
            &builder,
            "setCategory",
            "(Ljava/lang/String;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&category)],
        );
        clear_pending_exception(env);
    }

    if let Some(ref pi) = refs.launch_intent {
        let _ = env.call_method(
            &builder,
            "setContentIntent",
            "(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;",
            &[JValue::Object(pi.as_obj())],
        );
        clear_pending_exception(env);
    }

    // Add Previous / Play|Pause / Next interactive transport actions.
    if let Ok(pkg_val) = env.call_method(
        refs.context.as_obj(),
        "getPackageName",
        "()Ljava/lang/String;",
        &[],
    ) && !clear_pending_exception(env)
        && let Ok(pkg_obj) = pkg_val.l()
        && !pkg_obj.is_null()
    {
        add_notification_action(
            env,
            refs,
            &builder,
            &pkg_obj,
            &NotifAction {
                req_code: 101,
                cmd: CMD_PREV,
                icon_res: IC_MEDIA_PREVIOUS,
                title: "Previous",
            },
        );
        let play_pause = if playing {
            NotifAction {
                req_code: 102,
                cmd: CMD_PAUSE,
                icon_res: IC_MEDIA_PAUSE,
                title: "Pause",
            }
        } else {
            NotifAction {
                req_code: 102,
                cmd: CMD_PLAY,
                icon_res: IC_MEDIA_PLAY,
                title: "Play",
            }
        };
        add_notification_action(env, refs, &builder, &pkg_obj, &play_pause);
        add_notification_action(
            env,
            refs,
            &builder,
            &pkg_obj,
            &NotifAction {
                req_code: 103,
                cmd: CMD_NEXT,
                icon_res: IC_MEDIA_NEXT,
                title: "Next",
            },
        );
    }

    if let Ok(style) = env.new_object("android/app/Notification$MediaStyle", "()V", &[])
        && !clear_pending_exception(env)
        && !style.is_null()
    {
        let _ = env.call_method(
            &style,
            "setMediaSession",
            "(Landroid/media/session/MediaSession$Token;)Landroid/app/Notification$MediaStyle;",
            &[JValue::Object(refs.session_token.as_obj())],
        );
        clear_pending_exception(env);
        if let Ok(compact_idx) = env.new_int_array(3)
            && env
                .set_int_array_region(&compact_idx, 0, &[0, 1, 2])
                .is_ok()
        {
            let compact_obj = JObject::from(compact_idx);
            let _ = env.call_method(
                &style,
                "setShowActionsInCompactView",
                "([I)Landroid/app/Notification$MediaStyle;",
                &[JValue::Object(&compact_obj)],
            );
            clear_pending_exception(env);
        }
        let _ = env.call_method(
            &builder,
            "setStyle",
            "(Landroid/app/Notification$Style;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&style)],
        );
        clear_pending_exception(env);
    }

    if let Ok(val) = env.call_method(&builder, "build", "()Landroid/app/Notification;", &[])
        && !clear_pending_exception(env)
        && let Ok(notification) = val.l()
        && !notification.is_null()
    {
        let _ = env.call_method(
            mgr.as_obj(),
            "notify",
            "(ILandroid/app/Notification;)V",
            &[JValue::Int(NOTIFICATION_ID), JValue::Object(&notification)],
        );
        return !clear_pending_exception(env);
    }
    false
}

fn clear_android_session(env: &mut JNIEnv<'_>, refs: &mut AndroidSessionRefs) {
    sync_wake_lock(env, refs, false);

    let _ = env.call_method(
        refs.session.as_obj(),
        "setActive",
        "(Z)V",
        &[JValue::Bool(0)],
    );
    clear_pending_exception(env);
    refs.session_active = false;

    if let Some(ref mgr) = refs.notif_mgr {
        let _ = env.call_method(
            mgr.as_obj(),
            "cancel",
            "(I)V",
            &[JValue::Int(NOTIFICATION_ID)],
        );
        clear_pending_exception(env);
    }
    refs.cached_artwork_path.clear();
    refs.cached_artwork_bitmap = None;
    refs.last_notif_playing = None;
    refs.last_notif_track_id.clear();
    refs.last_notif_artwork.clear();
    refs.last_notif_fav = None;
    refs.notif_posted = false;
}

fn release_android_session(env: &mut JNIEnv<'_>, refs: &mut AndroidSessionRefs) {
    sync_wake_lock(env, refs, false);

    if let Some(ref receiver) = refs.action_receiver {
        let _ = env.call_method(
            refs.context.as_obj(),
            "unregisterReceiver",
            "(Landroid/content/BroadcastReceiver;)V",
            &[JValue::Object(receiver.as_obj())],
        );
        clear_pending_exception(env);
    }
    let null_cb = JObject::null();
    let _ = env.call_method(
        refs.session.as_obj(),
        "setCallback",
        "(Landroid/media/session/MediaSession$Callback;)V",
        &[JValue::Object(&null_cb)],
    );
    clear_pending_exception(env);
    let _ = env.call_method(
        refs.session.as_obj(),
        "setActive",
        "(Z)V",
        &[JValue::Bool(0)],
    );
    clear_pending_exception(env);
    refs.session_active = false;
    let _ = env.call_method(refs.session.as_obj(), "release", "()V", &[]);
    clear_pending_exception(env);
    if let Some(ref mgr) = refs.notif_mgr {
        let _ = env.call_method(
            mgr.as_obj(),
            "cancel",
            "(I)V",
            &[JValue::Int(NOTIFICATION_ID)],
        );
        clear_pending_exception(env);
    }
}

fn clear_pending_exception(env: &mut JNIEnv<'_>) -> bool {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
        true
    } else {
        false
    }
}
