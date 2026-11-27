//! Native Linux MPRIS2 D-Bus media control driver (`org.mpris.MediaPlayer2.nshmusic`).
//!
//! Uses runtime `libc::dlopen` / `libc::dlsym` against `libgio-2.0.so.0`,
//! `libglib-2.0.so.0`, and `libgobject-2.0.so.0` (`GDBus`) so there is zero
//! build-time `pkg-config` or C header dependency. Runs a dedicated `GMainLoop`
//! thread with its own `GMainContext` and gracefully becomes a no-op in headless
//! environments without a D-Bus session bus.

#![cfg(all(target_os = "linux", not(target_env = "ohos")))]

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use nsh_music_audio::{
    NowPlayingMetadata, PlatformRemoteDriver, RemoteBackend, RemoteCommand, RemoteController,
    RemoteLoopMode, RemotePlaybackStatus,
};

use super::{
    mpris_art_url, mpris_loop_and_shuffle, mpris_loop_from_status, mpris_playback_status,
    mpris_track_object_path, ms_to_us, us_to_ms,
};

const G_BUS_TYPE_SESSION: c_int = 2;
const MPRIS_BUS_NAME: &CStr = c"org.mpris.MediaPlayer2.nshmusic";
const MPRIS_OBJECT_PATH: &CStr = c"/org/mpris/MediaPlayer2";
const MPRIS_ROOT_IFACE: &CStr = c"org.mpris.MediaPlayer2";
const MPRIS_PLAYER_IFACE: &CStr = c"org.mpris.MediaPlayer2.Player";
const DBUS_PROPS_IFACE: &CStr = c"org.freedesktop.DBus.Properties";

const MPRIS_INTROSPECTION_XML: &CStr = c"
<node>
  <interface name='org.mpris.MediaPlayer2'>
    <method name='Raise'/>
    <method name='Quit'/>
    <property name='CanQuit' type='b' access='read'/>
    <property name='CanRaise' type='b' access='read'/>
    <property name='HasTrackList' type='b' access='read'/>
    <property name='Identity' type='s' access='read'/>
    <property name='DesktopEntry' type='s' access='read'/>
    <property name='SupportedUriSchemes' type='as' access='read'/>
    <property name='SupportedMimeTypes' type='as' access='read'/>
  </interface>
  <interface name='org.mpris.MediaPlayer2.Player'>
    <method name='Next'/>
    <method name='Previous'/>
    <method name='Pause'/>
    <method name='PlayPause'/>
    <method name='Stop'/>
    <method name='Play'/>
    <method name='Seek'>
      <arg direction='in' name='Offset' type='x'/>
    </method>
    <method name='SetPosition'>
      <arg direction='in' name='TrackId' type='o'/>
      <arg direction='in' name='Position' type='x'/>
    </method>
    <method name='OpenUri'>
      <arg direction='in' name='Uri' type='s'/>
    </method>
    <signal name='Seeked'>
      <arg name='Position' type='x'/>
    </signal>
    <property name='PlaybackStatus' type='s' access='read'/>
    <property name='LoopStatus' type='s' access='readwrite'/>
    <property name='Rate' type='d' access='readwrite'/>
    <property name='Shuffle' type='b' access='readwrite'/>
    <property name='Metadata' type='a{sv}' access='read'/>
    <property name='Volume' type='d' access='readwrite'/>
    <property name='Position' type='x' access='read'/>
    <property name='MinimumRate' type='d' access='read'/>
    <property name='MaximumRate' type='d' access='read'/>
    <property name='CanGoNext' type='b' access='read'/>
    <property name='CanGoPrevious' type='b' access='read'/>
    <property name='CanPlay' type='b' access='read'/>
    <property name='CanPause' type='b' access='read'/>
    <property name='CanSeek' type='b' access='read'/>
    <property name='CanControl' type='b' access='read'/>
  </interface>
</node>
";

type GSourceFunc = unsafe extern "C" fn(*mut c_void) -> c_int;
type GDestroyNotify = unsafe extern "C" fn(*mut c_void);

#[repr(C)]
struct GDBusInterfaceVTable {
    method_call: Option<
        unsafe extern "C" fn(
            connection: *mut c_void,
            sender: *const c_char,
            object_path: *const c_char,
            interface_name: *const c_char,
            method_name: *const c_char,
            parameters: *mut c_void,
            invocation: *mut c_void,
            user_data: *mut c_void,
        ),
    >,
    get_property: Option<
        unsafe extern "C" fn(
            connection: *mut c_void,
            sender: *const c_char,
            object_path: *const c_char,
            interface_name: *const c_char,
            property_name: *const c_char,
            error: *mut *mut c_void,
            user_data: *mut c_void,
        ) -> *mut c_void,
    >,
    set_property: Option<
        unsafe extern "C" fn(
            connection: *mut c_void,
            sender: *const c_char,
            object_path: *const c_char,
            interface_name: *const c_char,
            property_name: *const c_char,
            value: *mut c_void,
            error: *mut *mut c_void,
            user_data: *mut c_void,
        ) -> c_int,
    >,
    padding: [usize; 8],
}

// SAFETY: `GDBusInterfaceVTable` contains only immutable function pointers and zeroed padding.
unsafe impl Sync for GDBusInterfaceVTable {}

struct GioMprisApi {
    // GLib MainContext / MainLoop
    g_main_context_new: unsafe extern "C" fn() -> *mut c_void,
    g_main_context_unref: unsafe extern "C" fn(*mut c_void),
    g_main_context_push_thread_default: unsafe extern "C" fn(*mut c_void),
    g_main_context_pop_thread_default: unsafe extern "C" fn(*mut c_void),
    g_main_context_invoke_full:
        unsafe extern "C" fn(*mut c_void, c_int, GSourceFunc, *mut c_void, Option<GDestroyNotify>),
    g_main_loop_new: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
    g_main_loop_run: unsafe extern "C" fn(*mut c_void),
    g_main_loop_quit: unsafe extern "C" fn(*mut c_void),
    g_main_loop_unref: unsafe extern "C" fn(*mut c_void),
    g_error_free: unsafe extern "C" fn(*mut c_void),

    // GLib GVariant (strictly non-variadic symbols for cross-arch ABI safety)
    g_variant_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    g_variant_new_object_path: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    g_variant_new_int64: unsafe extern "C" fn(i64) -> *mut c_void,
    g_variant_new_double: unsafe extern "C" fn(f64) -> *mut c_void,
    g_variant_new_boolean: unsafe extern "C" fn(c_int) -> *mut c_void,
    g_variant_new_strv: unsafe extern "C" fn(*const *const c_char, isize) -> *mut c_void,
    g_variant_new_variant: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    g_variant_new_dict_entry: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void,
    g_variant_new_tuple: unsafe extern "C" fn(*const *mut c_void, usize) -> *mut c_void,
    g_variant_type_new: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    g_variant_type_free: unsafe extern "C" fn(*mut c_void),
    g_variant_builder_new: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    g_variant_builder_add_value: unsafe extern "C" fn(*mut c_void, *mut c_void),
    g_variant_builder_end: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    g_variant_builder_unref: unsafe extern "C" fn(*mut c_void),
    g_variant_get_string: unsafe extern "C" fn(*mut c_void, *mut usize) -> *const c_char,
    g_variant_get_int64: unsafe extern "C" fn(*mut c_void) -> i64,
    g_variant_get_boolean: unsafe extern "C" fn(*mut c_void) -> c_int,
    g_variant_get_child_value: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void,
    g_variant_n_children: unsafe extern "C" fn(*mut c_void) -> usize,
    g_variant_ref_sink: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    g_variant_unref: unsafe extern "C" fn(*mut c_void),

    // GObject
    g_object_unref: unsafe extern "C" fn(*mut c_void),

    // GIO GDBus
    g_bus_get_sync: unsafe extern "C" fn(c_int, *mut c_void, *mut *mut c_void) -> *mut c_void,
    g_bus_own_name_on_connection: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        c_uint,
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void)>,
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void)>,
        *mut c_void,
        Option<GDestroyNotify>,
    ) -> c_uint,
    g_bus_unown_name: unsafe extern "C" fn(c_uint),
    g_dbus_node_info_new_for_xml:
        unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut c_void,
    g_dbus_node_info_unref: unsafe extern "C" fn(*mut c_void),
    g_dbus_node_info_lookup_interface:
        unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    g_dbus_connection_register_object: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        *const GDBusInterfaceVTable,
        *mut c_void,
        Option<GDestroyNotify>,
        *mut *mut c_void,
    ) -> c_uint,
    g_dbus_connection_unregister_object: unsafe extern "C" fn(*mut c_void, c_uint) -> c_int,
    g_dbus_connection_emit_signal: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut c_void,
        *mut *mut c_void,
    ) -> c_int,
    g_dbus_method_invocation_return_value: unsafe extern "C" fn(*mut c_void, *mut c_void),
}

// SAFETY: All pointers are process-global immutable C function pointers loaded via `dlopen`.
unsafe impl Send for GioMprisApi {}
unsafe impl Sync for GioMprisApi {}

unsafe fn load_sym<T>(handle: *mut c_void, name: &CStr) -> Option<T> {
    let mut ptr = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    }
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy(&ptr) })
}

fn api() -> Option<&'static GioMprisApi> {
    static API: OnceLock<Option<GioMprisApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let flags = libc::RTLD_LAZY | libc::RTLD_LOCAL;
        let glib = libc::dlopen(c"libglib-2.0.so.0".as_ptr(), flags);
        let gobj = libc::dlopen(c"libgobject-2.0.so.0".as_ptr(), flags);
        let gio = libc::dlopen(c"libgio-2.0.so.0".as_ptr(), flags);
        if glib.is_null() || gobj.is_null() || gio.is_null() {
            return None;
        }
        Some(GioMprisApi {
            g_main_context_new: load_sym(glib, c"g_main_context_new")?,
            g_main_context_unref: load_sym(glib, c"g_main_context_unref")?,
            g_main_context_push_thread_default: load_sym(
                glib,
                c"g_main_context_push_thread_default",
            )?,
            g_main_context_pop_thread_default: load_sym(
                glib,
                c"g_main_context_pop_thread_default",
            )?,
            g_main_context_invoke_full: load_sym(glib, c"g_main_context_invoke_full")?,
            g_main_loop_new: load_sym(glib, c"g_main_loop_new")?,
            g_main_loop_run: load_sym(glib, c"g_main_loop_run")?,
            g_main_loop_quit: load_sym(glib, c"g_main_loop_quit")?,
            g_main_loop_unref: load_sym(glib, c"g_main_loop_unref")?,
            g_error_free: load_sym(glib, c"g_error_free")?,

            g_variant_new_string: load_sym(glib, c"g_variant_new_string")?,
            g_variant_new_object_path: load_sym(glib, c"g_variant_new_object_path")?,
            g_variant_new_int64: load_sym(glib, c"g_variant_new_int64")?,
            g_variant_new_double: load_sym(glib, c"g_variant_new_double")?,
            g_variant_new_boolean: load_sym(glib, c"g_variant_new_boolean")?,
            g_variant_new_strv: load_sym(glib, c"g_variant_new_strv")?,
            g_variant_new_variant: load_sym(glib, c"g_variant_new_variant")?,
            g_variant_new_dict_entry: load_sym(glib, c"g_variant_new_dict_entry")?,
            g_variant_new_tuple: load_sym(glib, c"g_variant_new_tuple")?,
            g_variant_type_new: load_sym(glib, c"g_variant_type_new")?,
            g_variant_type_free: load_sym(glib, c"g_variant_type_free")?,
            g_variant_builder_new: load_sym(glib, c"g_variant_builder_new")?,
            g_variant_builder_add_value: load_sym(glib, c"g_variant_builder_add_value")?,
            g_variant_builder_end: load_sym(glib, c"g_variant_builder_end")?,
            g_variant_builder_unref: load_sym(glib, c"g_variant_builder_unref")?,
            g_variant_get_string: load_sym(glib, c"g_variant_get_string")?,
            g_variant_get_int64: load_sym(glib, c"g_variant_get_int64")?,
            g_variant_get_boolean: load_sym(glib, c"g_variant_get_boolean")?,
            g_variant_get_child_value: load_sym(glib, c"g_variant_get_child_value")?,
            g_variant_n_children: load_sym(glib, c"g_variant_n_children")?,
            g_variant_ref_sink: load_sym(glib, c"g_variant_ref_sink")?,
            g_variant_unref: load_sym(glib, c"g_variant_unref")?,

            g_object_unref: load_sym(gobj, c"g_object_unref")?,

            g_bus_get_sync: load_sym(gio, c"g_bus_get_sync")?,
            g_bus_own_name_on_connection: load_sym(gio, c"g_bus_own_name_on_connection")?,
            g_bus_unown_name: load_sym(gio, c"g_bus_unown_name")?,
            g_dbus_node_info_new_for_xml: load_sym(gio, c"g_dbus_node_info_new_for_xml")?,
            g_dbus_node_info_unref: load_sym(gio, c"g_dbus_node_info_unref")?,
            g_dbus_node_info_lookup_interface: load_sym(gio, c"g_dbus_node_info_lookup_interface")?,
            g_dbus_connection_register_object: load_sym(gio, c"g_dbus_connection_register_object")?,
            g_dbus_connection_unregister_object: load_sym(
                gio,
                c"g_dbus_connection_unregister_object",
            )?,
            g_dbus_connection_emit_signal: load_sym(gio, c"g_dbus_connection_emit_signal")?,
            g_dbus_method_invocation_return_value: load_sym(
                gio,
                c"g_dbus_method_invocation_return_value",
            )?,
        })
    })
    .as_ref()
}

struct MprisState {
    meta: NowPlayingMetadata,
    updated_at: Instant,
    has_track: bool,
    last_track_id: String,
    last_position_ms: u64,
    last_position_at: Instant,
}

impl Default for MprisState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            meta: NowPlayingMetadata::default(),
            updated_at: now,
            has_track: false,
            last_track_id: String::new(),
            last_position_ms: 0,
            last_position_at: now,
        }
    }
}

struct LoopHandle {
    ctx_addr: usize,
    loop_addr: usize,
    conn_addr: usize,
}

struct MprisShared {
    remote: RemoteController,
    state: Mutex<MprisState>,
    loop_handle: Mutex<Option<LoopHandle>>,
}

/// Linux MPRIS2 D-Bus implementation of [`PlatformRemoteDriver`].
pub(crate) struct LinuxMprisDriver {
    shared: Arc<MprisShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LinuxMprisDriver {
    pub(crate) fn new(remote: RemoteController) -> Self {
        let shared = Arc::new(MprisShared {
            remote,
            state: Mutex::new(MprisState::default()),
            loop_handle: Mutex::new(None),
        });
        let worker = if let Some(a) = api() {
            let shared_bg = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("nsh-linux-mpris".into())
                .spawn(move || run_mpris_loop(a, shared_bg))
                .ok()
        } else {
            None
        };
        Self {
            shared,
            worker: Mutex::new(worker),
        }
    }

    fn emit_updates(&self, emit_meta: bool, emit_playback: bool, seeked_pos_ms: Option<u64>) {
        let Some(a) = api() else {
            return;
        };
        let (ctx_addr, conn_addr) = {
            let Ok(guard) = self.shared.loop_handle.lock() else {
                return;
            };
            let Some(handle) = guard.as_ref() else {
                return;
            };
            (handle.ctx_addr, handle.conn_addr)
        };
        let shared = Arc::clone(&self.shared);
        invoke_on_context(a, ctx_addr as *mut c_void, move || {
            let conn = conn_addr as *mut c_void;
            if conn.is_null() {
                return;
            }
            let meta = {
                let Ok(st) = shared.state.lock() else {
                    return;
                };
                if st.has_track {
                    Some(st.meta.clone())
                } else {
                    None
                }
            };
            if emit_meta || emit_playback {
                unsafe {
                    emit_properties_changed(a, conn, meta.as_ref(), emit_meta, emit_playback);
                }
            }
            if let Some(pos_ms) = seeked_pos_ms {
                unsafe {
                    emit_seeked(a, conn, pos_ms);
                }
            }
        });
    }
}

impl PlatformRemoteDriver for LinuxMprisDriver {
    fn backend(&self) -> RemoteBackend {
        RemoteBackend::LinuxMpris
    }

    fn update_metadata(&self, meta: &NowPlayingMetadata) {
        if let Ok(mut st) = self.shared.state.lock() {
            let now = Instant::now();
            st.meta = meta.clone();
            st.updated_at = now;
            st.has_track = true;
            st.last_track_id = meta.track_id.clone();
            st.last_position_ms = meta.position_ms;
            st.last_position_at = now;
        }
        self.emit_updates(true, true, None);
    }

    fn update_playback(&self, is_playing: bool, position_ms: u64, duration_ms: u64) {
        let mut meta = self.shared.remote.metadata();
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
        self.update_playback_detail(&meta);
    }

    fn update_playback_detail(&self, meta: &NowPlayingMetadata) {
        let (meta_changed, playback_changed, seeked_pos) = {
            let Ok(mut st) = self.shared.state.lock() else {
                return;
            };
            let now = Instant::now();
            let same_track = st.has_track && st.last_track_id == meta.track_id;
            let meta_changed = !st.has_track
                || st.meta.track_id != meta.track_id
                || st.meta.title != meta.title
                || st.meta.artist != meta.artist
                || st.meta.album != meta.album
                || st.meta.artwork_url != meta.artwork_url
                || st.meta.duration_ms != meta.duration_ms
                || st.meta.lyric != meta.lyric;
            let playback_changed = !st.has_track
                || st.meta.status != meta.status
                || st.meta.loop_mode != meta.loop_mode
                || st.meta.can_next != meta.can_next
                || st.meta.can_prev != meta.can_prev;

            let seeked_pos = if same_track {
                let elapsed_ms = if st.meta.status.is_playing() {
                    now.saturating_duration_since(st.last_position_at)
                        .as_millis() as u64
                } else {
                    0
                };
                let expected = st.last_position_ms.saturating_add(elapsed_ms);
                if meta.position_ms.abs_diff(expected) > 1_500 {
                    Some(meta.position_ms)
                } else {
                    None
                }
            } else {
                None
            };

            st.meta = meta.clone();
            st.updated_at = now;
            st.has_track = true;
            st.last_track_id = meta.track_id.clone();
            st.last_position_ms = meta.position_ms;
            st.last_position_at = now;
            (meta_changed, playback_changed, seeked_pos)
        };

        if meta_changed || playback_changed || seeked_pos.is_some() {
            self.emit_updates(meta_changed, playback_changed, seeked_pos);
        }
    }

    fn clear(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            *st = MprisState::default();
        }
        self.emit_updates(true, true, None);
    }
}

impl Drop for LinuxMprisDriver {
    fn drop(&mut self) {
        if let Some(a) = api()
            && let Ok(guard) = self.shared.loop_handle.lock()
            && let Some(handle) = guard.as_ref()
        {
            let ctx = handle.ctx_addr as *mut c_void;
            let main_loop = handle.loop_addr;
            invoke_on_context(a, ctx, move || unsafe {
                let ml = main_loop as *mut c_void;
                if !ml.is_null() {
                    (a.g_main_loop_quit)(ml);
                }
            });
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

fn invoke_on_context<F>(a: &'static GioMprisApi, ctx: *mut c_void, f: F)
where
    F: FnOnce() + Send + 'static,
{
    if ctx.is_null() {
        return;
    }
    unsafe extern "C" fn trampoline(data: *mut c_void) -> c_int {
        let boxed: Box<Box<dyn FnOnce() + Send>> = unsafe { Box::from_raw(data as *mut _) };
        boxed();
        0 // G_SOURCE_REMOVE
    }
    let payload: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    unsafe {
        (a.g_main_context_invoke_full)(
            ctx,
            0,
            trampoline,
            Box::into_raw(payload) as *mut c_void,
            None,
        );
    }
}

unsafe extern "C" fn drop_mpris_shared(data: *mut c_void) {
    if !data.is_null() {
        drop(unsafe { Arc::from_raw(data as *const MprisShared) });
    }
}

static ROOT_VTABLE: GDBusInterfaceVTable = GDBusInterfaceVTable {
    method_call: Some(on_root_method_call),
    get_property: Some(on_root_get_property),
    set_property: None,
    padding: [0; 8],
};

static PLAYER_VTABLE: GDBusInterfaceVTable = GDBusInterfaceVTable {
    method_call: Some(on_player_method_call),
    get_property: Some(on_player_get_property),
    set_property: Some(on_player_set_property),
    padding: [0; 8],
};

fn run_mpris_loop(a: &'static GioMprisApi, shared: Arc<MprisShared>) {
    unsafe {
        let mut err: *mut c_void = std::ptr::null_mut();
        let conn = (a.g_bus_get_sync)(G_BUS_TYPE_SESSION, std::ptr::null_mut(), &mut err);
        if !err.is_null() {
            (a.g_error_free)(err);
        }
        if conn.is_null() {
            // Headless environment without D-Bus session bus: gracefully no-op.
            return;
        }

        let mut xml_err: *mut c_void = std::ptr::null_mut();
        let node_info =
            (a.g_dbus_node_info_new_for_xml)(MPRIS_INTROSPECTION_XML.as_ptr(), &mut xml_err);
        if !xml_err.is_null() {
            (a.g_error_free)(xml_err);
        }
        if node_info.is_null() {
            (a.g_object_unref)(conn);
            return;
        }

        let root_iface =
            (a.g_dbus_node_info_lookup_interface)(node_info, MPRIS_ROOT_IFACE.as_ptr());
        let player_iface =
            (a.g_dbus_node_info_lookup_interface)(node_info, MPRIS_PLAYER_IFACE.as_ptr());
        if root_iface.is_null() || player_iface.is_null() {
            (a.g_dbus_node_info_unref)(node_info);
            (a.g_object_unref)(conn);
            return;
        }

        let ctx = (a.g_main_context_new)();
        (a.g_main_context_push_thread_default)(ctx);
        let main_loop = (a.g_main_loop_new)(ctx, 0);

        let mut reg_err1: *mut c_void = std::ptr::null_mut();
        let root_reg_id = (a.g_dbus_connection_register_object)(
            conn,
            MPRIS_OBJECT_PATH.as_ptr(),
            root_iface,
            &ROOT_VTABLE,
            Arc::into_raw(Arc::clone(&shared)) as *mut c_void,
            Some(drop_mpris_shared),
            &mut reg_err1,
        );
        if !reg_err1.is_null() {
            (a.g_error_free)(reg_err1);
        }

        let mut reg_err2: *mut c_void = std::ptr::null_mut();
        let player_reg_id = (a.g_dbus_connection_register_object)(
            conn,
            MPRIS_OBJECT_PATH.as_ptr(),
            player_iface,
            &PLAYER_VTABLE,
            Arc::into_raw(Arc::clone(&shared)) as *mut c_void,
            Some(drop_mpris_shared),
            &mut reg_err2,
        );
        if !reg_err2.is_null() {
            (a.g_error_free)(reg_err2);
        }

        let name_owner_id = (a.g_bus_own_name_on_connection)(
            conn,
            MPRIS_BUS_NAME.as_ptr(),
            0,
            None,
            None,
            std::ptr::null_mut(),
            None,
        );

        if let Ok(mut guard) = shared.loop_handle.lock() {
            *guard = Some(LoopHandle {
                ctx_addr: ctx as usize,
                loop_addr: main_loop as usize,
                conn_addr: conn as usize,
            });
        }

        (a.g_main_loop_run)(main_loop);

        if let Ok(mut guard) = shared.loop_handle.lock() {
            *guard = None;
        }
        if name_owner_id != 0 {
            (a.g_bus_unown_name)(name_owner_id);
        }
        if player_reg_id != 0 {
            (a.g_dbus_connection_unregister_object)(conn, player_reg_id);
        }
        if root_reg_id != 0 {
            (a.g_dbus_connection_unregister_object)(conn, root_reg_id);
        }
        (a.g_main_loop_unref)(main_loop);
        (a.g_main_context_pop_thread_default)(ctx);
        (a.g_main_context_unref)(ctx);
        (a.g_dbus_node_info_unref)(node_info);
        (a.g_object_unref)(conn);
    }
}

unsafe extern "C" fn on_root_method_call(
    _conn: *mut c_void,
    _sender: *const c_char,
    _path: *const c_char,
    _iface: *const c_char,
    _method: *const c_char,
    _params: *mut c_void,
    invocation: *mut c_void,
    _user_data: *mut c_void,
) {
    if let Some(a) = api() {
        unsafe { (a.g_dbus_method_invocation_return_value)(invocation, std::ptr::null_mut()) };
    }
}

unsafe extern "C" fn on_root_get_property(
    _conn: *mut c_void,
    _sender: *const c_char,
    _path: *const c_char,
    _iface: *const c_char,
    property_name: *const c_char,
    _error: *mut *mut c_void,
    _user_data: *mut c_void,
) -> *mut c_void {
    let Some(a) = api() else {
        return std::ptr::null_mut();
    };
    if property_name.is_null() {
        return std::ptr::null_mut();
    }
    let prop = unsafe { CStr::from_ptr(property_name) }
        .to_str()
        .unwrap_or("");
    let var = unsafe {
        match prop {
            "CanQuit" | "CanRaise" | "HasTrackList" => (a.g_variant_new_boolean)(0),
            "Identity" => (a.g_variant_new_string)(c"NSH Music".as_ptr()),
            "DesktopEntry" => (a.g_variant_new_string)(c"nsh-music".as_ptr()),
            "SupportedUriSchemes" | "SupportedMimeTypes" => {
                (a.g_variant_new_strv)(std::ptr::null(), 0)
            }
            _ => return std::ptr::null_mut(),
        }
    };
    unsafe { (a.g_variant_ref_sink)(var) }
}

unsafe extern "C" fn on_player_method_call(
    _conn: *mut c_void,
    _sender: *const c_char,
    _path: *const c_char,
    _iface: *const c_char,
    method_name: *const c_char,
    parameters: *mut c_void,
    invocation: *mut c_void,
    user_data: *mut c_void,
) {
    let Some(a) = api() else {
        return;
    };
    if !method_name.is_null() && !user_data.is_null() {
        let method = unsafe { CStr::from_ptr(method_name) }
            .to_str()
            .unwrap_or("");
        let shared = unsafe { &*(user_data as *const MprisShared) };
        match method {
            "Play" => shared.remote.dispatch(RemoteCommand::Play),
            "Pause" => shared.remote.dispatch(RemoteCommand::Pause),
            "PlayPause" => shared.remote.dispatch(RemoteCommand::TogglePlayPause),
            "Stop" => shared.remote.dispatch(RemoteCommand::Stop),
            "Next" => shared.remote.dispatch(RemoteCommand::Next),
            "Previous" => shared.remote.dispatch(RemoteCommand::Prev),
            "Seek" => {
                if !parameters.is_null() && unsafe { (a.g_variant_n_children)(parameters) } >= 1 {
                    let child = unsafe { (a.g_variant_get_child_value)(parameters, 0) };
                    let offset_us = unsafe { (a.g_variant_get_int64)(child) };
                    unsafe { (a.g_variant_unref)(child) };
                    shared
                        .remote
                        .dispatch(RemoteCommand::SeekRelative(offset_us / 1_000));
                }
            }
            "SetPosition" => {
                if !parameters.is_null() && unsafe { (a.g_variant_n_children)(parameters) } >= 2 {
                    let child = unsafe { (a.g_variant_get_child_value)(parameters, 1) };
                    let pos_us = unsafe { (a.g_variant_get_int64)(child) };
                    unsafe { (a.g_variant_unref)(child) };
                    shared
                        .remote
                        .dispatch(RemoteCommand::Seek(us_to_ms(pos_us)));
                }
            }
            _ => {}
        }
    }
    unsafe { (a.g_dbus_method_invocation_return_value)(invocation, std::ptr::null_mut()) };
}

unsafe extern "C" fn on_player_get_property(
    _conn: *mut c_void,
    _sender: *const c_char,
    _path: *const c_char,
    _iface: *const c_char,
    property_name: *const c_char,
    _error: *mut *mut c_void,
    user_data: *mut c_void,
) -> *mut c_void {
    let Some(a) = api() else {
        return std::ptr::null_mut();
    };
    if property_name.is_null() || user_data.is_null() {
        return std::ptr::null_mut();
    }
    let prop = unsafe { CStr::from_ptr(property_name) }
        .to_str()
        .unwrap_or("");
    let shared = unsafe { &*(user_data as *const MprisShared) };
    let (meta, has_track, interpolated_pos_ms) = {
        let Ok(st) = shared.state.lock() else {
            return std::ptr::null_mut();
        };
        let elapsed_ms = if st.has_track && st.meta.status.is_playing() {
            Instant::now()
                .saturating_duration_since(st.updated_at)
                .as_millis() as u64
        } else {
            0
        };
        let pos = st.meta.position_ms.saturating_add(elapsed_ms);
        let clamped = if st.meta.duration_ms > 0 {
            pos.min(st.meta.duration_ms)
        } else {
            pos
        };
        (st.meta.clone(), st.has_track, clamped)
    };

    let var = unsafe {
        match prop {
            "PlaybackStatus" => {
                let status = if has_track {
                    mpris_playback_status(meta.status)
                } else {
                    "Stopped"
                };
                new_gstring(a, status)
            }
            "LoopStatus" => {
                let (loop_str, _) = mpris_loop_and_shuffle(meta.loop_mode);
                new_gstring(a, loop_str)
            }
            "Shuffle" => {
                let (_, shuffle) = mpris_loop_and_shuffle(meta.loop_mode);
                (a.g_variant_new_boolean)(i32::from(shuffle))
            }
            "Rate" | "MinimumRate" | "MaximumRate" | "Volume" => (a.g_variant_new_double)(1.0),
            "Position" => (a.g_variant_new_int64)(if has_track {
                ms_to_us(interpolated_pos_ms)
            } else {
                0
            }),
            "Metadata" => build_metadata_variant(a, has_track.then_some(&meta)),
            "CanGoNext" => (a.g_variant_new_boolean)(i32::from(has_track && meta.can_next)),
            "CanGoPrevious" => (a.g_variant_new_boolean)(i32::from(has_track && meta.can_prev)),
            "CanPlay" | "CanPause" | "CanControl" => (a.g_variant_new_boolean)(1),
            "CanSeek" => (a.g_variant_new_boolean)(i32::from(has_track && meta.duration_ms > 0)),
            _ => return std::ptr::null_mut(),
        }
    };
    unsafe { (a.g_variant_ref_sink)(var) }
}

unsafe extern "C" fn on_player_set_property(
    _conn: *mut c_void,
    _sender: *const c_char,
    _path: *const c_char,
    _iface: *const c_char,
    property_name: *const c_char,
    value: *mut c_void,
    _error: *mut *mut c_void,
    user_data: *mut c_void,
) -> c_int {
    let Some(a) = api() else {
        return 0;
    };
    if property_name.is_null() || value.is_null() || user_data.is_null() {
        return 0;
    }
    let prop = unsafe { CStr::from_ptr(property_name) }
        .to_str()
        .unwrap_or("");
    let shared = unsafe { &*(user_data as *const MprisShared) };

    match prop {
        "LoopStatus" => {
            let mut len = 0usize;
            let s_ptr = unsafe { (a.g_variant_get_string)(value, &mut len) };
            if !s_ptr.is_null() {
                let text = unsafe { CStr::from_ptr(s_ptr) }.to_str().unwrap_or("");
                if let Some(mode) = mpris_loop_from_status(text) {
                    shared.remote.dispatch(RemoteCommand::SetLoopMode(mode));
                    return 1;
                }
            }
            0
        }
        "Shuffle" => {
            let enabled = unsafe { (a.g_variant_get_boolean)(value) } != 0;
            let current = shared.remote.metadata().loop_mode;
            let next = if enabled {
                RemoteLoopMode::Shuffle
            } else if matches!(current, RemoteLoopMode::Shuffle) {
                RemoteLoopMode::Sequence
            } else {
                current
            };
            shared.remote.dispatch(RemoteCommand::SetLoopMode(next));
            1
        }
        _ => 1,
    }
}

unsafe fn new_gstring(a: &'static GioMprisApi, s: &str) -> *mut c_void {
    let sanitized = s.replace('\0', "");
    let c = CString::new(sanitized).unwrap_or_default();
    unsafe { (a.g_variant_new_string)(c.as_ptr()) }
}

unsafe fn add_dict_entry(
    a: &'static GioMprisApi,
    builder: *mut c_void,
    key: &CStr,
    val_variant: *mut c_void,
) {
    unsafe {
        let k = (a.g_variant_new_string)(key.as_ptr());
        let boxed = (a.g_variant_new_variant)(val_variant);
        let entry = (a.g_variant_new_dict_entry)(k, boxed);
        (a.g_variant_builder_add_value)(builder, entry);
    }
}

unsafe fn build_metadata_variant(
    a: &'static GioMprisApi,
    meta: Option<&NowPlayingMetadata>,
) -> *mut c_void {
    unsafe {
        let dict_ty = (a.g_variant_type_new)(c"a{sv}".as_ptr());
        let builder = (a.g_variant_builder_new)(dict_ty);
        (a.g_variant_type_free)(dict_ty);

        if let Some(meta) = meta {
            let track_path = CString::new(mpris_track_object_path(&meta.track_id))
                .unwrap_or_else(|_| c"/org/mpris/MediaPlayer2/Track/empty".to_owned());
            add_dict_entry(
                a,
                builder,
                c"mpris:trackid",
                (a.g_variant_new_object_path)(track_path.as_ptr()),
            );

            add_dict_entry(
                a,
                builder,
                c"mpris:length",
                (a.g_variant_new_int64)(ms_to_us(meta.duration_ms)),
            );

            if let Some(art_url) = mpris_art_url(&meta.artwork_url) {
                add_dict_entry(a, builder, c"mpris:artUrl", new_gstring(a, &art_url));
            }

            add_dict_entry(a, builder, c"xesam:title", new_gstring(a, &meta.title));

            if !meta.artist.is_empty() {
                let artist_c = CString::new(meta.artist.replace('\0', "")).unwrap_or_default();
                let ptrs = [artist_c.as_ptr()];
                let strv = (a.g_variant_new_strv)(ptrs.as_ptr(), 1);
                add_dict_entry(a, builder, c"xesam:artist", strv);
            }

            if !meta.album.is_empty() {
                add_dict_entry(a, builder, c"xesam:album", new_gstring(a, &meta.album));
            }

            if let Some(lyric) = meta.lyric.as_deref()
                && !lyric.is_empty()
            {
                add_dict_entry(a, builder, c"xesam:asText", new_gstring(a, lyric));
            }
        }

        let dict = (a.g_variant_builder_end)(builder);
        (a.g_variant_builder_unref)(builder);
        dict
    }
}

unsafe fn emit_properties_changed(
    a: &'static GioMprisApi,
    conn: *mut c_void,
    meta: Option<&NowPlayingMetadata>,
    include_meta: bool,
    include_playback: bool,
) {
    unsafe {
        let dict_ty = (a.g_variant_type_new)(c"a{sv}".as_ptr());
        let builder = (a.g_variant_builder_new)(dict_ty);
        (a.g_variant_type_free)(dict_ty);

        if include_meta {
            let md = build_metadata_variant(a, meta);
            add_dict_entry(a, builder, c"Metadata", md);
            let can_seek = meta.is_some_and(|m| m.duration_ms > 0);
            add_dict_entry(
                a,
                builder,
                c"CanSeek",
                (a.g_variant_new_boolean)(i32::from(can_seek)),
            );
        }

        if include_playback {
            let status = meta
                .map(|m| mpris_playback_status(m.status))
                .unwrap_or("Stopped");
            add_dict_entry(a, builder, c"PlaybackStatus", new_gstring(a, status));

            let loop_mode = meta.map(|m| m.loop_mode).unwrap_or_default();
            let (loop_str, shuffle) = mpris_loop_and_shuffle(loop_mode);
            add_dict_entry(a, builder, c"LoopStatus", new_gstring(a, loop_str));
            add_dict_entry(
                a,
                builder,
                c"Shuffle",
                (a.g_variant_new_boolean)(i32::from(shuffle)),
            );

            let can_next = meta.is_some_and(|m| m.can_next);
            let can_prev = meta.is_some_and(|m| m.can_prev);
            add_dict_entry(
                a,
                builder,
                c"CanGoNext",
                (a.g_variant_new_boolean)(i32::from(can_next)),
            );
            add_dict_entry(
                a,
                builder,
                c"CanGoPrevious",
                (a.g_variant_new_boolean)(i32::from(can_prev)),
            );
        }

        let changed_dict = (a.g_variant_builder_end)(builder);
        (a.g_variant_builder_unref)(builder);

        let iface_var = (a.g_variant_new_string)(MPRIS_PLAYER_IFACE.as_ptr());
        let invalidated = (a.g_variant_new_strv)(std::ptr::null(), 0);
        let children = [iface_var, changed_dict, invalidated];
        let params = (a.g_variant_new_tuple)(children.as_ptr(), children.len());

        let mut err: *mut c_void = std::ptr::null_mut();
        (a.g_dbus_connection_emit_signal)(
            conn,
            std::ptr::null(),
            MPRIS_OBJECT_PATH.as_ptr(),
            DBUS_PROPS_IFACE.as_ptr(),
            c"PropertiesChanged".as_ptr(),
            params,
            &mut err,
        );
        if !err.is_null() {
            (a.g_error_free)(err);
        }
    }
}

unsafe fn emit_seeked(a: &'static GioMprisApi, conn: *mut c_void, pos_ms: u64) {
    unsafe {
        let pos_var = (a.g_variant_new_int64)(ms_to_us(pos_ms));
        let children = [pos_var];
        let params = (a.g_variant_new_tuple)(children.as_ptr(), children.len());

        let mut err: *mut c_void = std::ptr::null_mut();
        (a.g_dbus_connection_emit_signal)(
            conn,
            std::ptr::null(),
            MPRIS_OBJECT_PATH.as_ptr(),
            MPRIS_PLAYER_IFACE.as_ptr(),
            c"Seeked".as_ptr(),
            params,
            &mut err,
        );
        if !err.is_null() {
            (a.g_error_free)(err);
        }
    }
}
