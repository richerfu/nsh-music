//! Native HarmonyOS `AVSession` driver powering the Notification Bar, Lock
//! Screen, Control Center, and Bluetooth/headset media controls.
//!
//! Bridges `nsh_music_audio::PlatformRemoteDriver` onto the ArkTS UI thread via
//! `nsh_platform_harmonyos::run_on_ui_thread` using dynamic `@ohos.*` N-API
//! module loading (`@ohos.multimedia.avsession`, `@ohos.multimedia.image`,
//! `@ohos.app.ability.wantAgent`, `@ohos.resourceschedule.backgroundTaskManager`).

#![cfg(target_env = "ohos")]

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteController,
    RemoteLoopMode, RemotePlaybackStatus,
};
use nsh_platform_harmonyos::UiThread;
use nsh_platform_harmonyos::napi_ohos::bindgen_prelude::{
    Env, FnArgs, Function, FunctionCallContext, JsObjectValue, Object, ObjectRef, Unknown,
};
use nsh_platform_harmonyos::napi_ohos::{self, JsValue, ValueType};

/// HarmonyOS `avSession.PlaybackState` numeric constants.
const OHOS_PLAYBACK_PLAY: i32 = 2;
const OHOS_PLAYBACK_PAUSE: i32 = 3;
const OHOS_PLAYBACK_STOP: i32 = 6;
const OHOS_PLAYBACK_BUFFERING: i32 = 11;

/// HarmonyOS `avSession.LoopMode` numeric constants.
const OHOS_LOOP_SEQUENCE: i32 = 0;
const OHOS_LOOP_SINGLE: i32 = 1;
const OHOS_LOOP_LIST: i32 = 2;
const OHOS_LOOP_SHUFFLE: i32 = 3;

/// HarmonyOS `backgroundTaskManager.BackgroundMode.AUDIO_PLAYBACK`.
const OHOS_BG_MODE_AUDIO_PLAYBACK: i32 = 2;

/// Fallback `wantAgent.OperationType.START_ABILITY` and `WantAgentFlags.UPDATE_PRESENT_FLAG`.
const FALLBACK_OP_START_ABILITY: i32 = 1;
const FALLBACK_FLAG_UPDATE_PRESENT: i32 = 3;

#[derive(Default)]
struct PendingState {
    metadata: Option<NowPlayingMetadata>,
    meta_dirty: bool,
    playback_dirty: bool,
    clear_requested: bool,
}

struct SharedSync {
    pending: Mutex<PendingState>,
    scheduled: AtomicBool,
    remote: RemoteController,
}

/// Drop guard ensuring `scheduled` is always cleared even if `run_on_ui_thread`
/// fails immediately (e.g. session not started yet) or the host session closes
/// before our queued UI task executes.
struct ScheduleResetGuard {
    shared: Arc<SharedSync>,
    armed: bool,
}

impl ScheduleResetGuard {
    fn new(shared: Arc<SharedSync>) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScheduleResetGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.scheduled.store(false, Ordering::Release);
        }
    }
}

/// HarmonyOS `AVSession` implementation of [`PlatformRemoteDriver`].
pub(crate) struct HarmonyAvSessionDriver {
    shared: Arc<SharedSync>,
}

impl HarmonyAvSessionDriver {
    pub(crate) fn new(remote: RemoteController) -> Self {
        Self {
            shared: Arc::new(SharedSync {
                pending: Mutex::new(PendingState::default()),
                scheduled: AtomicBool::new(false),
                remote,
            }),
        }
    }

    fn schedule_flush(&self) {
        if self.shared.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let mut guard = ScheduleResetGuard::new(Arc::clone(&self.shared));
        drop(nsh_platform_harmonyos::run_on_ui_thread(move |ui| {
            guard.disarm();
            flush_on_ui_thread(ui, shared);
            Ok(())
        }));
    }
}

impl PlatformRemoteDriver for HarmonyAvSessionDriver {
    fn backend(&self) -> RemoteBackend {
        RemoteBackend::HarmonyAvSession
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = Some(meta.clone());
            pending.meta_dirty = true;
            pending.playback_dirty = true;
            pending.clear_requested = false;
        }
        self.schedule_flush();
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
        }
        self.schedule_flush();
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
                    || prev.can_next != meta.can_next
                    || prev.can_prev != meta.can_prev
                    || prev.lyric != meta.lyric
            });
            pending.metadata = Some(meta.clone());
            if meta_changed {
                pending.meta_dirty = true;
            }
            pending.playback_dirty = true;
            pending.clear_requested = false;
        }
        self.schedule_flush();
    }

    fn clear(&self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.metadata = None;
            pending.meta_dirty = false;
            pending.playback_dirty = false;
            pending.clear_requested = true;
        }
        self.schedule_flush();
    }
}

#[derive(Default)]
struct UiSessionState {
    session: Option<ObjectRef<false>>,
    creating: bool,
    activating: bool,
    active: bool,
    want_agent: Option<ObjectRef<false>>,
    want_agent_creating: bool,
    launch_ability_set: bool,
    bg_running: bool,
    ability_ctx: Option<ObjectRef<false>>,
    bundle_name: String,
    cached_artwork_key: Option<String>,
    cached_artwork_pixelmap: Option<ObjectRef<false>>,
    last_sent_asset_id: Option<String>,
}

thread_local! {
    static SESSION_STATE: RefCell<UiSessionState> = RefCell::new(UiSessionState::default());
}

fn flush_on_ui_thread(ui: &UiThread<'_>, shared: Arc<SharedSync>) {
    let env = ui.env();

    // Cache bundle name and host UIAbilityContext on the UI thread for async callbacks.
    SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        if st.bundle_name.is_empty() && !ui.bundle_name().is_empty() {
            st.bundle_name = ui.bundle_name().to_owned();
        }
        if st.ability_ctx.is_none()
            && let Ok(ctx_obj) = ui.ability_context()
            && let Ok(ctx_ref) = ctx_obj.create_ref::<false>()
        {
            st.ability_ctx = Some(ctx_ref);
        }
    });

    let (has_session, creating) = SESSION_STATE.with(|cell| {
        let st = cell.borrow();
        (st.session.is_some(), st.creating)
    });

    if !has_session {
        // Peek whether there is anything other than `clear_requested` before allocating a session.
        let needs_session = {
            let Ok(mut pending) = shared.pending.lock() else {
                shared.scheduled.store(false, Ordering::Release);
                return;
            };
            if pending.clear_requested {
                pending.clear_requested = false;
                shared.scheduled.store(false, Ordering::Release);
                false
            } else {
                shared.scheduled.store(false, Ordering::Release);
                pending.metadata.is_some()
            }
        };
        if !needs_session {
            stop_background_task_if_running(env);
            return;
        }
        if !creating {
            start_create_session(ui, shared);
        }
        return;
    }

    drain_and_apply(env, &shared);
}

fn start_create_session(ui: &UiThread<'_>, shared: Arc<SharedSync>) {
    let env = ui.env();
    let av_mod = match ui.load_module("@ohos.multimedia.avsession") {
        Ok(m) => m,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "load @ohos.multimedia.avsession failed: {err}");
            return;
        }
    };
    let ability_ctx = match ui.ability_context() {
        Ok(ctx) => ctx,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "ability_context unavailable for AVSession: {err}");
            return;
        }
    };

    SESSION_STATE.with(|cell| {
        cell.borrow_mut().creating = true;
    });

    let shared_cb = Arc::clone(&shared);
    let cb = match env.create_function_from_closure::<(), (), _>(
        "onCreateAVSession",
        move |ctx: FunctionCallContext<'_>| {
            on_session_created(ctx, &shared_cb);
            Ok(())
        },
    ) {
        Ok(f) => f,
        Err(err) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
            log::warn!(target: "nsh_music_remote", "create onCreateAVSession callback failed: {err}");
            return;
        }
    };

    let call_res = av_mod.call::<_, _, Unknown<'_>>(
        "createAVSession",
        FnArgs::from((ability_ctx, "NSHMusic", "audio", cb)),
    );
    if let Err(err) = call_res {
        SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
        log::warn!(target: "nsh_music_remote", "avSession.createAVSession call failed: {err}");
    }
}

fn on_session_created(ctx: FunctionCallContext<'_>, shared: &Arc<SharedSync>) {
    if let Some(err_msg) = extract_callback_error(&ctx) {
        SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
        log::warn!(target: "nsh_music_remote", "createAVSession callback error: {err_msg}");
        return;
    }

    let session_obj = match ctx.get::<Unknown<'_>>(1) {
        Ok(val) if val.get_type().ok() == Some(ValueType::Object) => match val.coerce_to_object() {
            Ok(obj) => obj,
            Err(err) => {
                SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
                log::warn!(target: "nsh_music_remote", "coerce AVSession object failed: {err}");
                return;
            }
        },
        _ => {
            SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
            log::warn!(target: "nsh_music_remote", "createAVSession returned non-object session");
            return;
        }
    };
    let env: &Env = ctx.env;

    let session_ref = match session_obj.create_ref::<false>() {
        Ok(r) => r,
        Err(err) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().creating = false);
            log::warn!(target: "nsh_music_remote", "create_ref for AVSession failed: {err}");
            return;
        }
    };

    register_session_callbacks(env, &session_obj, &shared.remote);

    SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        st.session = Some(session_ref);
        st.creating = false;
        st.activating = false;
        st.active = false;
        st.launch_ability_set = false;
    });

    log::info!(target: "nsh_music_remote", "HarmonyOS AVSession created and callbacks registered");

    ensure_want_agent(env, shared);
    activate_session(env, &session_obj);
    drain_and_apply(env, shared);
}

fn register_session_callbacks(env: &Env, session: &Object<'_>, remote: &RemoteController) {
    let register = |event: &str, cb: Function<'_, (), ()>, warn_on_err: bool| {
        if let Err(err) = call_method_2(session, "on", event, cb) {
            if warn_on_err {
                log::warn!(
                    target: "nsh_music_remote",
                    "AVSession.on('{event}') registration failed: {err}"
                );
            } else {
                log::debug!(
                    target: "nsh_music_remote",
                    "AVSession.on('{event}') unavailable: {err}"
                );
            }
        }
    };

    // play
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avPlay", move |_| {
            log::info!(target: "nsh_music_remote", "AVSession cmd: play");
            r.dispatch(RemoteCommand::Play);
            Ok(())
        }) {
            register("play", cb, true);
        }
    }
    // pause
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avPause", move |_| {
            log::info!(target: "nsh_music_remote", "AVSession cmd: pause");
            r.dispatch(RemoteCommand::Pause);
            Ok(())
        }) {
            register("pause", cb, true);
        }
    }
    // stop
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avStop", move |_| {
            log::info!(target: "nsh_music_remote", "AVSession cmd: stop");
            r.dispatch(RemoteCommand::Stop);
            Ok(())
        }) {
            register("stop", cb, true);
        }
    }
    // playNext
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avPlayNext", move |_| {
            log::info!(target: "nsh_music_remote", "AVSession cmd: playNext");
            r.dispatch(RemoteCommand::Next);
            Ok(())
        }) {
            register("playNext", cb, true);
        }
    }
    // playPrevious
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avPlayPrev", move |_| {
            log::info!(target: "nsh_music_remote", "AVSession cmd: playPrevious");
            r.dispatch(RemoteCommand::Prev);
            Ok(())
        }) {
            register("playPrevious", cb, true);
        }
    }
    // seek(time: number)
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avSeek", move |ctx| {
            if let Some(time_ms) = read_optional_f64(&ctx, 0) {
                let target = time_ms.max(0.0).round() as u64;
                log::info!(target: "nsh_music_remote", "AVSession cmd: seek({target}ms)");
                r.dispatch(RemoteCommand::Seek(target));
            }
            Ok(())
        }) {
            register("seek", cb, true);
        }
    }
    // fastForward(time?: number)
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avFastForward", move |ctx| {
            let delta_ms = read_optional_f64(&ctx, 0)
                .filter(|s| *s > 0.0)
                .map(|s| (s * 1000.0).round() as i64)
                .unwrap_or(15_000);
            log::info!(
                target: "nsh_music_remote",
                "AVSession cmd: fastForward(+{delta_ms}ms)"
            );
            r.dispatch(RemoteCommand::SeekRelative(delta_ms));
            Ok(())
        }) {
            register("fastForward", cb, true);
        }
    }
    // rewind(time?: number)
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avRewind", move |ctx| {
            let delta_ms = read_optional_f64(&ctx, 0)
                .filter(|s| *s > 0.0)
                .map(|s| -((s * 1000.0).round() as i64))
                .unwrap_or(-15_000);
            log::info!(
                target: "nsh_music_remote",
                "AVSession cmd: rewind({delta_ms}ms)"
            );
            r.dispatch(RemoteCommand::SeekRelative(delta_ms));
            Ok(())
        }) {
            register("rewind", cb, true);
        }
    }
    // toggleFavorite(assetId: string)
    {
        let r = remote.clone();
        if let Ok(cb) =
            env.create_function_from_closure::<(), (), _>("avToggleFavorite", move |_ctx| {
                log::info!(target: "nsh_music_remote", "AVSession cmd: toggleFavorite");
                r.dispatch(RemoteCommand::ToggleFavorite);
                Ok(())
            })
        {
            register("toggleFavorite", cb, true);
        }
    }
    // setLoopMode(currentMode: LoopMode) -> advance to next_cycle()
    {
        let r = remote.clone();
        if let Ok(cb) = env.create_function_from_closure::<(), (), _>("avSetLoopMode", move |ctx| {
            let current = read_optional_f64(&ctx, 0)
                .map(|v| ohos_to_loop_mode(v.round() as i32))
                .unwrap_or_else(|| r.metadata().loop_mode);
            let next = current.next_cycle();
            log::info!(
                target: "nsh_music_remote",
                "AVSession cmd: setLoopMode({current:?} -> {next:?})"
            );
            r.dispatch(RemoteCommand::SetLoopMode(next));
            Ok(())
        }) {
            register("setLoopMode", cb, true);
        }
    }
    // setTargetLoopMode(targetMode: LoopMode) -> best-effort (API 12+)
    {
        let r = remote.clone();
        if let Ok(cb) =
            env.create_function_from_closure::<(), (), _>("avSetTargetLoopMode", move |ctx| {
                if let Some(raw) = read_optional_f64(&ctx, 0) {
                    let target = ohos_to_loop_mode(raw.round() as i32);
                    log::info!(
                        target: "nsh_music_remote",
                        "AVSession cmd: setTargetLoopMode({target:?})"
                    );
                    r.dispatch(RemoteCommand::SetLoopMode(target));
                }
                Ok(())
            })
        {
            register("setTargetLoopMode", cb, false);
        }
    }
}

fn ensure_want_agent(env: &Env, shared: &Arc<SharedSync>) {
    let (already_ready, creating, bundle_name) = SESSION_STATE.with(|cell| {
        let st = cell.borrow();
        (
            st.want_agent.is_some(),
            st.want_agent_creating,
            st.bundle_name.clone(),
        )
    });
    if already_ready || creating || bundle_name.is_empty() {
        return;
    }

    let wa_mod = match env.load("@ohos.app.ability.wantAgent") {
        Ok(m) => m,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "load @ohos.app.ability.wantAgent failed: {err}");
            return;
        }
    };

    let start_ability = wa_mod
        .get::<_, Object<'_>>("OperationType")
        .ok()
        .and_then(|o| o.get::<i32>("START_ABILITY").ok().flatten())
        .unwrap_or(FALLBACK_OP_START_ABILITY);
    let update_present = wa_mod
        .get::<_, Object<'_>>("WantAgentFlags")
        .ok()
        .and_then(|o| o.get::<i32>("UPDATE_PRESENT_FLAG").ok().flatten())
        .unwrap_or(FALLBACK_FLAG_UPDATE_PRESENT);

    let info_res = (|| -> napi_ohos::Result<Object<'_>> {
        let mut want = Object::new(env)?;
        want.set("bundleName", bundle_name.as_str())?;
        want.set("abilityName", "EntryAbility")?;

        let mut wants = env.create_array(1)?;
        wants.set(0, want)?;

        let mut flags = env.create_array(1)?;
        flags.set(0, update_present)?;

        let mut info = Object::new(env)?;
        info.set("wants", wants)?;
        info.set("actionType", start_ability)?;
        info.set("operationType", start_ability)?;
        info.set("requestCode", 0i32)?;
        info.set("actionFlags", flags)?;
        Ok(info)
    })();

    let info = match info_res {
        Ok(obj) => obj,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "build WantAgentInfo failed: {err}");
            return;
        }
    };

    SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = true);

    let shared_cb = Arc::clone(shared);
    let cb = match env.create_function_from_closure::<(), (), _>("onGetWantAgent", move |ctx| {
        on_want_agent_ready(ctx, &shared_cb);
        Ok(())
    }) {
        Ok(f) => f,
        Err(err) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
            log::warn!(target: "nsh_music_remote", "create onGetWantAgent callback failed: {err}");
            return;
        }
    };

    if let Err(err) = wa_mod.call::<_, _, Unknown<'_>>("getWantAgent", FnArgs::from((info, cb))) {
        SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
        log::warn!(target: "nsh_music_remote", "wantAgent.getWantAgent failed: {err}");
    }
}

fn on_want_agent_ready(ctx: FunctionCallContext<'_>, shared: &Arc<SharedSync>) {
    if let Some(err_msg) = extract_callback_error(&ctx) {
        SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
        log::warn!(target: "nsh_music_remote", "getWantAgent callback error: {err_msg}");
        return;
    }

    let wa_obj = match ctx.get::<Unknown<'_>>(1) {
        Ok(val) if val.get_type().ok() == Some(ValueType::Object) => match val.coerce_to_object() {
            Ok(obj) => obj,
            Err(err) => {
                SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
                log::warn!(target: "nsh_music_remote", "coerce WantAgent failed: {err}");
                return;
            }
        },
        _ => {
            SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
            return;
        }
    };
    let env: &Env = ctx.env;

    let wa_ref = match wa_obj.create_ref::<false>() {
        Ok(r) => r,
        Err(err) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().want_agent_creating = false);
            log::warn!(target: "nsh_music_remote", "create_ref for WantAgent failed: {err}");
            return;
        }
    };

    SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        st.want_agent = Some(wa_ref);
        st.want_agent_creating = false;
    });

    attach_launch_ability_if_ready(env);

    // If playback is already Playing/Buffering, start background continuous task now.
    let should_bg = shared
        .pending
        .lock()
        .ok()
        .and_then(|p| p.metadata.as_ref().map(|m| m.status))
        .unwrap_or_else(|| shared.remote.metadata().status);
    if matches!(
        should_bg,
        RemotePlaybackStatus::Playing | RemotePlaybackStatus::Buffering
    ) {
        start_background_task_if_needed(env);
    }
}

fn attach_launch_ability_if_ready(env: &Env) {
    let pair = SESSION_STATE.with(|cell| {
        let st = cell.borrow();
        if st.launch_ability_set {
            return None;
        }
        let s = st.session.as_ref()?.get_value(env).ok()?;
        let wa = st.want_agent.as_ref()?.get_value(env).ok()?;
        Some((s, wa))
    });
    let Some((session, wa)) = pair else {
        return;
    };

    if let Ok(cb) = make_log_callback(env, "setLaunchAbility") {
        if let Err(err) = call_method_2(&session, "setLaunchAbility", wa, cb) {
            log::warn!(target: "nsh_music_remote", "AVSession.setLaunchAbility failed: {err}");
        } else {
            SESSION_STATE.with(|cell| cell.borrow_mut().launch_ability_set = true);
        }
    }
}

fn activate_session(env: &Env, session: &Object<'_>) {
    let should_activate = SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        if st.active || st.activating {
            false
        } else {
            st.activating = true;
            true
        }
    });
    if !should_activate {
        return;
    }

    let cb = match env.create_function_from_closure::<(), (), _>("onActivateAVSession", |ctx| {
        if let Some(err_msg) = extract_callback_error(&ctx) {
            SESSION_STATE.with(|cell| {
                let mut st = cell.borrow_mut();
                st.activating = false;
                st.active = false;
            });
            log::warn!(target: "nsh_music_remote", "AVSession.activate error: {err_msg}");
        } else {
            SESSION_STATE.with(|cell| {
                let mut st = cell.borrow_mut();
                st.activating = false;
                st.active = true;
            });
            log::info!(target: "nsh_music_remote", "AVSession activated");
        }
        Ok(())
    }) {
        Ok(f) => f,
        Err(_) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().activating = false);
            return;
        }
    };
    if let Err(err) = call_method_1(session, "activate", cb) {
        SESSION_STATE.with(|cell| cell.borrow_mut().activating = false);
        log::warn!(target: "nsh_music_remote", "AVSession.activate call failed: {err}");
    }
}

fn drain_and_apply(env: &Env, shared: &Arc<SharedSync>) {
    let (meta, meta_dirty, playback_dirty, clear_requested) = {
        let Ok(mut pending) = shared.pending.lock() else {
            shared.scheduled.store(false, Ordering::Release);
            return;
        };
        let snapshot = (
            pending.metadata.clone(),
            pending.meta_dirty,
            pending.playback_dirty,
            pending.clear_requested,
        );
        pending.meta_dirty = false;
        pending.playback_dirty = false;
        pending.clear_requested = false;
        shared.scheduled.store(false, Ordering::Release);
        snapshot
    };

    let session = SESSION_STATE.with(|cell| {
        cell.borrow()
            .session
            .as_ref()
            .and_then(|r| r.get_value(env).ok())
    });
    let Some(session) = session else {
        return;
    };

    if clear_requested {
        push_stopped_state(env, &session);
        stop_background_task_if_running(env);
        return;
    }

    let Some(meta) = meta else {
        return;
    };

    attach_launch_ability_if_ready(env);

    if meta_dirty {
        push_metadata(env, &session, &meta);
    }
    if playback_dirty {
        push_playback_state(env, &session, &meta);
    }

    match meta.status {
        RemotePlaybackStatus::Playing | RemotePlaybackStatus::Buffering => {
            activate_session(env, &session);
            ensure_want_agent(env, shared);
            start_background_task_if_needed(env);
        }
        RemotePlaybackStatus::Stopped => {
            stop_background_task_if_running(env);
        }
        RemotePlaybackStatus::Paused => {}
    }
}

fn push_metadata(env: &Env, session: &Object<'_>, meta: &NowPlayingMetadata) {
    let asset_id = if meta.track_id.trim().is_empty() {
        "nsh-track"
    } else {
        meta.track_id.as_str()
    };

    let media_image_pm = resolve_artwork_pixelmap(env, &meta.artwork_url);

    let obj_res = (|| -> napi_ohos::Result<Object<'_>> {
        let mut av_meta = Object::new(env)?;
        av_meta.set("assetId", asset_id)?;
        if !meta.title.is_empty() {
            av_meta.set("title", meta.title.as_str())?;
        }
        if !meta.artist.is_empty() {
            av_meta.set("artist", meta.artist.as_str())?;
        }
        if !meta.album.is_empty() {
            av_meta.set("album", meta.album.as_str())?;
        }
        if meta.duration_ms > 0 {
            av_meta.set("duration", meta.duration_ms as f64)?;
        }
        if meta.can_prev {
            av_meta.set("previousAssetId", "prev")?;
        }
        if meta.can_next {
            av_meta.set("nextAssetId", "next")?;
        }
        if let Some(ref lyric) = meta.lyric
            && !lyric.is_empty()
        {
            av_meta.set("lyric", lyric.as_str())?;
        }
        if let Some(pm) = media_image_pm {
            av_meta.set("mediaImage", pm)?;
        } else if meta.artwork_url.starts_with("http://")
            || meta.artwork_url.starts_with("https://")
        {
            av_meta.set("mediaImage", meta.artwork_url.as_str())?;
        }
        Ok(av_meta)
    })();

    let av_meta = match obj_res {
        Ok(o) => o,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "build AVMetadata failed: {err}");
            return;
        }
    };

    if let Ok(cb) = make_log_callback(env, "setAVMetadata") {
        if let Err(err) = call_method_2(session, "setAVMetadata", av_meta, cb) {
            log::warn!(target: "nsh_music_remote", "AVSession.setAVMetadata failed: {err}");
        } else {
            SESSION_STATE.with(|cell| {
                let mut st = cell.borrow_mut();
                if st.last_sent_asset_id.as_deref() != Some(asset_id) {
                    st.last_sent_asset_id = Some(asset_id.to_owned());
                    log::info!(
                        target: "nsh_music_remote",
                        "AVSession metadata pushed: assetId={asset_id} title='{}'",
                        meta.title
                    );
                }
            });
        }
    }
}

fn push_playback_state(env: &Env, session: &Object<'_>, meta: &NowPlayingMetadata) {
    let state_code = match meta.status {
        RemotePlaybackStatus::Playing => OHOS_PLAYBACK_PLAY,
        RemotePlaybackStatus::Paused => OHOS_PLAYBACK_PAUSE,
        RemotePlaybackStatus::Buffering => OHOS_PLAYBACK_BUFFERING,
        RemotePlaybackStatus::Stopped => OHOS_PLAYBACK_STOP,
    };
    // OpenHarmony `AVPlaybackState::IsValid()` rejects `speed <= 0.0` (code=401)
    // even when `state` is PAUSE, BUFFERING, or STOP; always report a positive speed.
    let speed = if meta.playback_rate_milli > 0 {
        (meta.playback_rate_milli as f64 / 1000.0).max(0.1)
    } else {
        1.0
    };
    let loop_code = loop_mode_to_ohos(meta.loop_mode);

    let state_res = (|| -> napi_ohos::Result<Object<'_>> {
        let mut pos = Object::new(env)?;
        pos.set("elapsedTime", meta.position_ms as f64)?;
        pos.set("updateTime", unix_epoch_ms_f64())?;

        let mut pb = Object::new(env)?;
        pb.set("state", state_code)?;
        pb.set("speed", speed)?;
        pb.set("position", pos)?;
        if meta.duration_ms > 0 {
            pb.set("duration", meta.duration_ms as f64)?;
        }
        if meta.buffered_ms > 0 {
            pb.set("bufferedTime", meta.buffered_ms as f64)?;
        }
        pb.set("loopMode", loop_code)?;
        pb.set("isFavorite", meta.is_favorite)?;
        Ok(pb)
    })();

    let pb = match state_res {
        Ok(o) => o,
        Err(err) => {
            log::warn!(target: "nsh_music_remote", "build AVPlaybackState failed: {err}");
            return;
        }
    };

    if let Ok(cb) = make_log_callback(env, "setAVPlaybackState")
        && let Err(err) = call_method_2(session, "setAVPlaybackState", pb, cb)
    {
        log::warn!(target: "nsh_music_remote", "AVSession.setAVPlaybackState failed: {err}");
    }
}

fn push_stopped_state(env: &Env, session: &Object<'_>) {
    let state_res = (|| -> napi_ohos::Result<Object<'_>> {
        let mut pos = Object::new(env)?;
        pos.set("elapsedTime", 0.0f64)?;
        pos.set("updateTime", unix_epoch_ms_f64())?;

        let mut pb = Object::new(env)?;
        pb.set("state", OHOS_PLAYBACK_STOP)?;
        pb.set("speed", 1.0f64)?;
        pb.set("position", pos)?;
        Ok(pb)
    })();

    if let Ok(pb) = state_res
        && let Ok(cb) = make_log_callback(env, "setAVPlaybackState(STOP)")
    {
        let _ = call_method_2(session, "setAVPlaybackState", pb, cb);
    }
}

/// Decodes a local cached artwork image into an ArkTS `image.PixelMap` (reusing
/// the cached `PixelMap` when `artwork_url` hasn't changed, and releasing any
/// previous `PixelMap` to prevent GPU/ASHMEM leaks).
fn resolve_artwork_pixelmap<'env>(env: &'env Env, artwork_url: &str) -> Option<Object<'env>> {
    let trimmed = artwork_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let local_path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    if !local_path.starts_with('/') || !Path::new(local_path).is_file() {
        return None;
    }

    // Fast path: reuse cached PixelMap for the exact same file path.
    let cached = SESSION_STATE.with(|cell| {
        let st = cell.borrow();
        if st.cached_artwork_key.as_deref() == Some(local_path) {
            st.cached_artwork_pixelmap
                .as_ref()
                .and_then(|r| r.get_value(env).ok())
        } else {
            None
        }
    });
    if cached.is_some() {
        return cached;
    }

    let image_mod = env.load("@ohos.multimedia.image").ok()?;
    let src_raw: Unknown<'_> = image_mod.call("createImageSource", local_path).ok()?;
    if src_raw.get_type().ok() != Some(ValueType::Object) {
        return None;
    }
    let img_src = src_raw.coerce_to_object().ok()?;

    let pm_obj = decode_pixelmap_from_source(env, &img_src);

    // Always release the temporary `ImageSource` handle.
    release_js_handle(&img_src);

    let pm_obj = pm_obj?;
    let pm_ref = pm_obj.create_ref::<false>().ok()?;

    // Release previous cached PixelMap cleanly before storing the new one.
    let old_ref = SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        st.cached_artwork_key = Some(local_path.to_owned());
        st.cached_artwork_pixelmap.replace(pm_ref)
    });
    if let Some(old) = old_ref {
        if let Ok(old_obj) = old.get_value(env) {
            release_js_handle(&old_obj);
        }
        let _ = old.unref(env);
    }

    Some(pm_obj)
}

fn decode_pixelmap_from_source<'env>(
    env: &'env Env,
    img_src: &Object<'env>,
) -> Option<Object<'env>> {
    let opts = (|| -> napi_ohos::Result<Object<'env>> {
        let mut size = Object::new(env)?;
        size.set("width", 512i32)?;
        size.set("height", 512i32)?;
        let mut opts = Object::new(env)?;
        opts.set("desiredSize", size)?;
        Ok(opts)
    })()
    .ok();

    if let Some(opts) = opts
        && let Ok(func) = img_src
            .get_named_property::<Function<'_, Object<'env>, Unknown<'env>>>("createPixelMapSync")
        && let Ok(val) = func.apply(img_src, opts)
        && val.get_type().ok() == Some(ValueType::Object)
        && let Ok(obj) = val.coerce_to_object()
    {
        return Some(obj);
    }

    // Fallback: call `createPixelMapSync()` with no arguments.
    let func = img_src
        .get_named_property::<Function<'_, (), Unknown<'env>>>("createPixelMapSync")
        .ok()?;
    let val = func.apply(img_src, ()).ok()?;
    if val.get_type().ok() == Some(ValueType::Object) {
        val.coerce_to_object().ok()
    } else {
        None
    }
}

fn release_js_handle(obj: &Object<'_>) {
    if let Ok(rel) = obj.get_named_property::<Function<'_, (), Unknown<'_>>>("release") {
        let _ = rel.apply(obj, ());
    }
}

fn start_background_task_if_needed(env: &Env) {
    let pair = SESSION_STATE.with(|cell| {
        let st = cell.borrow();
        if st.bg_running {
            return None;
        }
        let ctx = st.ability_ctx.as_ref()?.get_value(env).ok()?;
        let wa = st.want_agent.as_ref()?.get_value(env).ok()?;
        Some((ctx, wa))
    });
    let Some((ability_ctx, want_agent)) = pair else {
        return;
    };

    let bg_mod = match env.load("@ohos.resourceschedule.backgroundTaskManager") {
        Ok(m) => m,
        Err(err) => {
            log::warn!(
                target: "nsh_music_remote",
                "load @ohos.resourceschedule.backgroundTaskManager failed: {err}"
            );
            return;
        }
    };

    // Mark optimistic in-flight so repeated playback ticks don't spam requests; callback reconciles.
    SESSION_STATE.with(|cell| cell.borrow_mut().bg_running = true);

    let cb = match env.create_function_from_closure::<(), (), _>("onStartBgRunning", |ctx| {
        if let Some(err_msg) = extract_callback_error(&ctx) {
            SESSION_STATE.with(|cell| cell.borrow_mut().bg_running = false);
            log::warn!(
                target: "nsh_music_remote",
                "backgroundTaskManager.startBackgroundRunning failed: {err_msg}"
            );
        } else {
            SESSION_STATE.with(|cell| cell.borrow_mut().bg_running = true);
            log::info!(
                target: "nsh_music_remote",
                "backgroundTaskManager AUDIO_PLAYBACK continuous task started"
            );
        }
        Ok(())
    }) {
        Ok(f) => f,
        Err(_) => {
            SESSION_STATE.with(|cell| cell.borrow_mut().bg_running = false);
            return;
        }
    };

    if let Err(err) = bg_mod.call::<_, _, Unknown<'_>>(
        "startBackgroundRunning",
        FnArgs::from((ability_ctx, OHOS_BG_MODE_AUDIO_PLAYBACK, want_agent, cb)),
    ) {
        SESSION_STATE.with(|cell| cell.borrow_mut().bg_running = false);
        log::warn!(
            target: "nsh_music_remote",
            "backgroundTaskManager.startBackgroundRunning call failed: {err}"
        );
    }
}

fn stop_background_task_if_running(env: &Env) {
    let ctx_obj = SESSION_STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        if !st.bg_running {
            return None;
        }
        st.bg_running = false;
        st.ability_ctx.as_ref().and_then(|r| r.get_value(env).ok())
    });
    let Some(ability_ctx) = ctx_obj else {
        return;
    };

    let Ok(bg_mod) = env.load("@ohos.resourceschedule.backgroundTaskManager") else {
        return;
    };
    let Ok(cb) = make_log_callback(env, "stopBackgroundRunning") else {
        return;
    };
    if let Err(err) =
        bg_mod.call::<_, _, Unknown<'_>>("stopBackgroundRunning", FnArgs::from((ability_ctx, cb)))
    {
        log::warn!(
            target: "nsh_music_remote",
            "backgroundTaskManager.stopBackgroundRunning failed: {err}"
        );
    }
}

fn call_method_1<A>(obj: &Object<'_>, name: &str, arg: A) -> napi_ohos::Result<()>
where
    A: napi_ohos::bindgen_prelude::ToNapiValue,
{
    let func: Function<'_, A, Unknown<'_>> = obj.get_named_property(name)?;
    let _ = func.apply(obj, arg)?;
    Ok(())
}

fn call_method_2<A, B>(obj: &Object<'_>, name: &str, a: A, b: B) -> napi_ohos::Result<()>
where
    A: napi_ohos::bindgen_prelude::ToNapiValue,
    B: napi_ohos::bindgen_prelude::ToNapiValue,
{
    let func: Function<'_, FnArgs<(A, B)>, Unknown<'_>> = obj.get_named_property(name)?;
    let _ = func.apply(obj, FnArgs::from((a, b)))?;
    Ok(())
}

fn make_log_callback<'env>(
    env: &'env Env,
    op: &'static str,
) -> napi_ohos::Result<Function<'env, (), ()>> {
    env.create_function_from_closure::<(), (), _>(op, move |ctx| {
        if let Some(err_msg) = extract_callback_error(&ctx) {
            log::warn!(target: "nsh_music_remote", "AVSession {op} callback error: {err_msg}");
        }
        Ok(())
    })
}

fn extract_callback_error(ctx: &FunctionCallContext<'_>) -> Option<String> {
    if ctx.length() == 0 {
        return None;
    }
    let arg0 = ctx.get::<Unknown<'_>>(0).ok()?;
    let ty = arg0.get_type().ok()?;
    if matches!(ty, ValueType::Undefined | ValueType::Null) {
        return None;
    }
    if ty == ValueType::Object
        && let Ok(obj) = arg0.coerce_to_object()
    {
        let code = obj.get::<i32>("code").ok().flatten();
        let message = obj.get::<String>("message").ok().flatten();
        match (code, message) {
            (Some(c), Some(m)) => return Some(format!("code={c} {m}")),
            (Some(c), None) => return Some(format!("code={c}")),
            (None, Some(m)) => return Some(m),
            (None, None) => {}
        }
    }
    arg0.coerce_to_string()
        .ok()
        .and_then(|s| s.into_utf8().ok())
        .and_then(|u| u.as_str().ok().map(str::to_owned))
        .or_else(|| Some(format!("{ty:?}")))
}

fn read_optional_f64(ctx: &FunctionCallContext<'_>, idx: usize) -> Option<f64> {
    if ctx.length() <= idx {
        return None;
    }
    let raw = ctx.get::<Unknown<'_>>(idx).ok()?;
    if raw.get_type().ok()? != ValueType::Number {
        return None;
    }
    ctx.get::<f64>(idx).ok()
}

const fn loop_mode_to_ohos(mode: RemoteLoopMode) -> i32 {
    match mode {
        RemoteLoopMode::Sequence => OHOS_LOOP_SEQUENCE,
        RemoteLoopMode::Single => OHOS_LOOP_SINGLE,
        RemoteLoopMode::List => OHOS_LOOP_LIST,
        RemoteLoopMode::Shuffle => OHOS_LOOP_SHUFFLE,
    }
}

const fn ohos_to_loop_mode(code: i32) -> RemoteLoopMode {
    match code {
        OHOS_LOOP_SINGLE => RemoteLoopMode::Single,
        OHOS_LOOP_LIST => RemoteLoopMode::List,
        OHOS_LOOP_SHUFFLE => RemoteLoopMode::Shuffle,
        _ => RemoteLoopMode::Sequence,
    }
}

fn unix_epoch_ms_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}
