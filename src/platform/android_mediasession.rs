//! Native Android `MediaSession` + `Notification.MediaStyle` driver via JNI.
//!
//! Spawns a dedicated JNI worker thread, discovers the process `JavaVM` via
//! `JNI_GetCreatedJavaVMs` (with `libart.so` fallback), acquires the host
//! `Application` context from `ActivityThread.currentApplication()`, and manages:
//! - `android.media.session.MediaSession` (metadata, playback state, launch `PendingIntent`)
//! - `android.media.MediaMetadata` (title, artist, album, duration, `BitmapFactory.decodeFile` artwork)
//! - `android.media.session.PlaybackState` (state, position, speed, buffered position, actions)
//! - `android.app.Notification` (`Notification.MediaStyle`) on channel `"nsh_music_playback"`,
//!   handling Android API >= 26 (`NotificationChannel`) and API >= 33 (`POST_NOTIFICATIONS`)
//!   cleanly via `Build.VERSION.SDK_INT`.

#![cfg(target_os = "android")]

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use jni::objects::{GlobalRef, JObject, JValue};
use jni::sys::{JNI_OK, jint, jsize};
use jni::{JNIEnv, JavaVM};
use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteController, RemotePlaybackStatus,
};

use super::{
    android_playback_actions, android_playback_speed, android_playback_state, artwork_local_path,
};

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
/// `android.R.drawable.ic_media_play` (`17_301_540`) fallback small icon resource ID
const FALLBACK_SMALL_ICON: jint = 0x0108_0024;

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

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
    small_icon: jint,
    sdk_int: jint,
    cached_artwork_path: String,
    cached_artwork_bitmap: Option<GlobalRef>,
    last_notif_playing: Option<bool>,
    last_notif_track_id: String,
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

        let _ = env.with_local_frame(48, |env| -> jni::errors::Result<()> {
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

            let mut need_notif = meta_dirty;
            if meta_dirty {
                push_android_metadata(env, refs, &meta);
            }
            if playback_dirty {
                push_android_playback(env, refs, &meta);
                let playing = meta.status.is_playing();
                if refs.last_notif_playing != Some(playing)
                    || refs.last_notif_track_id != meta.track_id
                {
                    need_notif = true;
                }
            }
            if need_notif {
                post_media_notification(env, refs, &meta);
                refs.last_notif_playing = Some(meta.status.is_playing());
                refs.last_notif_track_id = meta.track_id.clone();
            }
            Ok(())
        });
    }

    if let Some(refs) = session_refs
        && let Some(vm) = discover_java_vm()
        && let Ok(mut env) = vm.attach_current_thread()
    {
        let _ = env.with_local_frame(24, |env| -> jni::errors::Result<()> {
            release_android_session(env, &refs);
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
    let small_icon = resolve_app_icon(env, &app_obj);

    Some(AndroidSessionRefs {
        context: env.new_global_ref(&app_obj).ok()?,
        session: env.new_global_ref(&session_obj).ok()?,
        session_token: env.new_global_ref(&token_obj).ok()?,
        notif_mgr,
        launch_intent,
        small_icon,
        sdk_int,
        cached_artwork_path: String::new(),
        cached_artwork_bitmap: None,
        last_notif_playing: None,
        last_notif_track_id: String::new(),
    })
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
        clear_pending_exception(env);
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
    refs: &AndroidSessionRefs,
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

fn post_media_notification(
    env: &mut JNIEnv<'_>,
    refs: &AndroidSessionRefs,
    meta: &NowPlayingMetadata,
) {
    let Some(ref mgr) = refs.notif_mgr else {
        return;
    };
    if !can_post_notifications(env, refs) {
        return;
    }

    let builder = if refs.sdk_int >= 26 {
        let Ok(ch_id) = env.new_string(NOTIFICATION_CHANNEL_ID) else {
            return;
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
        return;
    };
    if clear_pending_exception(env) || builder.is_null() {
        return;
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

    let _ = env.call_method(
        &builder,
        "setOngoing",
        "(Z)Landroid/app/Notification$Builder;",
        &[JValue::Bool(u8::from(meta.status.is_playing()))],
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

    if let Some(ref pi) = refs.launch_intent {
        let _ = env.call_method(
            &builder,
            "setContentIntent",
            "(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;",
            &[JValue::Object(pi.as_obj())],
        );
        clear_pending_exception(env);
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
        clear_pending_exception(env);
    }
}

fn clear_android_session(env: &mut JNIEnv<'_>, refs: &mut AndroidSessionRefs) {
    let _ = env.call_method(
        refs.session.as_obj(),
        "setActive",
        "(Z)V",
        &[JValue::Bool(0)],
    );
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
    refs.cached_artwork_path.clear();
    refs.cached_artwork_bitmap = None;
    refs.last_notif_playing = None;
    refs.last_notif_track_id.clear();
}

fn release_android_session(env: &mut JNIEnv<'_>, refs: &AndroidSessionRefs) {
    let _ = env.call_method(
        refs.session.as_obj(),
        "setActive",
        "(Z)V",
        &[JValue::Bool(0)],
    );
    clear_pending_exception(env);
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
