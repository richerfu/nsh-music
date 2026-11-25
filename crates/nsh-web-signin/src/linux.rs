//! Linux GTK4 + WebKitGTK 6.0 + libsoup 3.0 popup WebView sign-in backend.
//!
//! Uses runtime `libc::dlopen` / `libc::dlsym` with zero build-time `pkg-config`
//! requirements so cross-compilation succeeds without Linux GUI headers. When
//! GTK4 is initialized in the running process, dispatches onto the GTK main
//! loop via `g_main_context_invoke_full`, creates a `GtkWindow` + ephemeral
//! `WebKitWebView`, and polls `WebKitCookieManager` (`soup_cookie_*`) to
//! capture all `HttpOnly` cookies.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome, cookies};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const READY_TIMEOUT_MS: u64 = 400;
const COOKIE_OP_TIMEOUT_MS: u64 = 2_000;
const POLL_INTERVAL_MS: u32 = 500;
const POPUP_WIDTH: c_int = 480;
const POPUP_HEIGHT: c_int = 640;
/// `WEBKIT_LOAD_FINISHED` enum value in WebKitGTK 6.0.
const WEBKIT_LOAD_FINISHED: c_int = 3;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
struct GList {
    data: *mut c_void,
    next: *mut GList,
    prev: *mut GList,
}

type GSourceFunc = unsafe extern "C" fn(*mut c_void) -> c_int;
type GDestroyNotify = unsafe extern "C" fn(*mut c_void);
type GAsyncReadyCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

struct GtkWebKitApi {
    // GLib
    g_main_context_invoke_full:
        unsafe extern "C" fn(*mut c_void, c_int, GSourceFunc, *mut c_void, Option<GDestroyNotify>),
    g_timeout_add_full: unsafe extern "C" fn(
        c_int,
        c_uint,
        GSourceFunc,
        *mut c_void,
        Option<GDestroyNotify>,
    ) -> c_uint,
    g_source_remove: unsafe extern "C" fn(c_uint) -> c_int,
    g_list_free: unsafe extern "C" fn(*mut GList),
    g_error_free: unsafe extern "C" fn(*mut c_void),
    g_date_time_to_unix: unsafe extern "C" fn(*mut c_void) -> i64,

    // GObject
    g_object_new_with_prop:
        unsafe extern "C" fn(usize, *const c_char, *mut c_void, *const c_char) -> *mut c_void,
    g_object_ref: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    g_object_unref: unsafe extern "C" fn(*mut c_void),
    g_signal_connect_data: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_void,
        *mut c_void,
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
        c_uint,
    ) -> c_ulong,
    g_signal_handler_disconnect: unsafe extern "C" fn(*mut c_void, c_ulong),

    // GTK4
    gtk_is_initialized: unsafe extern "C" fn() -> c_int,
    gtk_window_new: unsafe extern "C" fn() -> *mut c_void,
    gtk_window_set_title: unsafe extern "C" fn(*mut c_void, *const c_char),
    gtk_window_set_default_size: unsafe extern "C" fn(*mut c_void, c_int, c_int),
    gtk_window_set_resizable: unsafe extern "C" fn(*mut c_void, c_int),
    gtk_window_set_child: unsafe extern "C" fn(*mut c_void, *mut c_void),
    gtk_window_present: unsafe extern "C" fn(*mut c_void),
    gtk_window_destroy: unsafe extern "C" fn(*mut c_void),

    // WebKitGTK 6.0
    webkit_web_view_get_type: unsafe extern "C" fn() -> usize,
    webkit_web_view_new: unsafe extern "C" fn() -> *mut c_void,
    webkit_web_view_load_uri: unsafe extern "C" fn(*mut c_void, *const c_char),
    webkit_web_view_load_request: unsafe extern "C" fn(*mut c_void, *mut c_void),
    webkit_web_view_stop_loading: unsafe extern "C" fn(*mut c_void),
    webkit_web_view_get_uri: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    webkit_web_view_get_settings: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    webkit_settings_set_user_agent: unsafe extern "C" fn(*mut c_void, *const c_char),
    webkit_navigation_action_get_request: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    webkit_network_session_new_ephemeral: unsafe extern "C" fn() -> *mut c_void,
    webkit_network_session_get_default: unsafe extern "C" fn() -> *mut c_void,
    webkit_network_session_get_cookie_manager: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    webkit_cookie_manager_get_cookies: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        GAsyncReadyCallback,
        *mut c_void,
    ),
    webkit_cookie_manager_get_cookies_finish:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> *mut GList,
    webkit_cookie_manager_get_all_cookies:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, GAsyncReadyCallback, *mut c_void)>,
    webkit_cookie_manager_get_all_cookies_finish:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> *mut GList>,
    webkit_cookie_manager_delete_cookie: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            Option<GAsyncReadyCallback>,
            *mut c_void,
        ),
    >,
    webkit_cookie_manager_delete_cookie_finish:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> c_int>,

    // libsoup 3.0
    soup_cookie_get_name: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_get_value: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_get_domain: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_get_path: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_get_secure: unsafe extern "C" fn(*mut c_void) -> c_int,
    soup_cookie_get_expires: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    soup_cookie_free: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: All function pointers are process-global immutable C symbols loaded via `dlopen`.
unsafe impl Send for GtkWebKitApi {}
unsafe impl Sync for GtkWebKitApi {}

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

fn api() -> Option<&'static GtkWebKitApi> {
    static API: OnceLock<Option<GtkWebKitApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let flags = libc::RTLD_LAZY | libc::RTLD_LOCAL;
        let glib = libc::dlopen(c"libglib-2.0.so.0".as_ptr(), flags);
        let gobj = libc::dlopen(c"libgobject-2.0.so.0".as_ptr(), flags);
        let gtk = libc::dlopen(c"libgtk-4.so.1".as_ptr(), flags);
        let webkit = libc::dlopen(c"libwebkitgtk-6.0.so.4".as_ptr(), flags);
        let soup = libc::dlopen(c"libsoup-3.0.so.0".as_ptr(), flags);

        if glib.is_null() || gobj.is_null() || gtk.is_null() || webkit.is_null() || soup.is_null() {
            return None;
        }

        Some(GtkWebKitApi {
            g_main_context_invoke_full: load_sym(glib, c"g_main_context_invoke_full")?,
            g_timeout_add_full: load_sym(glib, c"g_timeout_add_full")?,
            g_source_remove: load_sym(glib, c"g_source_remove")?,
            g_list_free: load_sym(glib, c"g_list_free")?,
            g_error_free: load_sym(glib, c"g_error_free")?,
            g_date_time_to_unix: load_sym(glib, c"g_date_time_to_unix")?,

            g_object_new_with_prop: load_sym(gobj, c"g_object_new")?,
            g_object_ref: load_sym(gobj, c"g_object_ref")?,
            g_object_unref: load_sym(gobj, c"g_object_unref")?,
            g_signal_connect_data: load_sym(gobj, c"g_signal_connect_data")?,
            g_signal_handler_disconnect: load_sym(gobj, c"g_signal_handler_disconnect")?,

            gtk_is_initialized: load_sym(gtk, c"gtk_is_initialized")?,
            gtk_window_new: load_sym(gtk, c"gtk_window_new")?,
            gtk_window_set_title: load_sym(gtk, c"gtk_window_set_title")?,
            gtk_window_set_default_size: load_sym(gtk, c"gtk_window_set_default_size")?,
            gtk_window_set_resizable: load_sym(gtk, c"gtk_window_set_resizable")?,
            gtk_window_set_child: load_sym(gtk, c"gtk_window_set_child")?,
            gtk_window_present: load_sym(gtk, c"gtk_window_present")?,
            gtk_window_destroy: load_sym(gtk, c"gtk_window_destroy")?,

            webkit_web_view_get_type: load_sym(webkit, c"webkit_web_view_get_type")?,
            webkit_web_view_new: load_sym(webkit, c"webkit_web_view_new")?,
            webkit_web_view_load_uri: load_sym(webkit, c"webkit_web_view_load_uri")?,
            webkit_web_view_load_request: load_sym(webkit, c"webkit_web_view_load_request")?,
            webkit_web_view_stop_loading: load_sym(webkit, c"webkit_web_view_stop_loading")?,
            webkit_web_view_get_uri: load_sym(webkit, c"webkit_web_view_get_uri")?,
            webkit_web_view_get_settings: load_sym(webkit, c"webkit_web_view_get_settings")?,
            webkit_settings_set_user_agent: load_sym(webkit, c"webkit_settings_set_user_agent")?,
            webkit_navigation_action_get_request: load_sym(
                webkit,
                c"webkit_navigation_action_get_request",
            )?,
            webkit_network_session_new_ephemeral: load_sym(
                webkit,
                c"webkit_network_session_new_ephemeral",
            )?,
            webkit_network_session_get_default: load_sym(
                webkit,
                c"webkit_network_session_get_default",
            )?,
            webkit_network_session_get_cookie_manager: load_sym(
                webkit,
                c"webkit_network_session_get_cookie_manager",
            )?,
            webkit_cookie_manager_get_cookies: load_sym(
                webkit,
                c"webkit_cookie_manager_get_cookies",
            )?,
            webkit_cookie_manager_get_cookies_finish: load_sym(
                webkit,
                c"webkit_cookie_manager_get_cookies_finish",
            )?,
            webkit_cookie_manager_get_all_cookies: load_sym(
                webkit,
                c"webkit_cookie_manager_get_all_cookies",
            ),
            webkit_cookie_manager_get_all_cookies_finish: load_sym(
                webkit,
                c"webkit_cookie_manager_get_all_cookies_finish",
            ),
            webkit_cookie_manager_delete_cookie: load_sym(
                webkit,
                c"webkit_cookie_manager_delete_cookie",
            ),
            webkit_cookie_manager_delete_cookie_finish: load_sym(
                webkit,
                c"webkit_cookie_manager_delete_cookie_finish",
            ),

            soup_cookie_get_name: load_sym(soup, c"soup_cookie_get_name")?,
            soup_cookie_get_value: load_sym(soup, c"soup_cookie_get_value")?,
            soup_cookie_get_domain: load_sym(soup, c"soup_cookie_get_domain")?,
            soup_cookie_get_path: load_sym(soup, c"soup_cookie_get_path")?,
            soup_cookie_get_secure: load_sym(soup, c"soup_cookie_get_secure")?,
            soup_cookie_get_expires: load_sym(soup, c"soup_cookie_get_expires")?,
            soup_cookie_free: load_sym(soup, c"soup_cookie_free")?,
        })
    })
    .as_ref()
}

thread_local! {
    static ACTIVE_SESSIONS: RefCell<HashMap<u64, Rc<GtkSession>>> = RefCell::new(HashMap::new());
}

struct GtkSession {
    target_url: String,
    until: Vec<String>,
    match_url: Option<String>,
    latest_url: RefCell<String>,
    settled: Cell<bool>,
    window: *mut c_void,
    web_view: *mut c_void,
    cookie_manager: *mut c_void,
    network_session: *mut c_void,
    owns_network_session: bool,
    timer_id: Cell<c_uint>,
    close_sig: Cell<c_ulong>,
    load_sig: Cell<c_ulong>,
    create_sig: Cell<c_ulong>,
    tx: RefCell<Option<mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>>>,
}

fn invoke_on_main_loop<F>(api: &'static GtkWebKitApi, f: F)
where
    F: FnOnce() + Send + 'static,
{
    unsafe extern "C" fn trampoline(data: *mut c_void) -> c_int {
        let boxed: Box<Box<dyn FnOnce() + Send>> = unsafe { Box::from_raw(data as *mut _) };
        boxed();
        0 // G_SOURCE_REMOVE
    }
    let payload: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    unsafe {
        (api.g_main_context_invoke_full)(
            std::ptr::null_mut(),
            0, // G_PRIORITY_DEFAULT
            trampoline,
            Box::into_raw(payload) as *mut c_void,
            None,
        );
    }
}

pub(crate) fn mode() -> SignInMode {
    if available() {
        SignInMode::NativePopup
    } else {
        SignInMode::ManualFallback
    }
}

pub(crate) fn available() -> bool {
    api().is_some_and(|a| unsafe { (a.gtk_is_initialized)() != 0 })
}

pub(crate) fn open_blocking(options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    let Some(a) = api() else {
        return Err(SignInError::Unavailable(
            "GTK4 / WebKitGTK 6.0 / libsoup 3.0 libraries not found".into(),
        ));
    };
    if unsafe { (a.gtk_is_initialized)() == 0 } {
        return Err(SignInError::Unavailable(
            "GTK4 is not initialized (headless runtime)".into(),
        ));
    }

    let timeout = Duration::from_millis(if options.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        options.timeout_ms
    });

    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), SignInError>>(1);
    let (result_tx, result_rx) = mpsc::sync_channel::<Result<WebSignInOutcome, SignInError>>(1);

    invoke_on_main_loop(a, move || {
        match open_on_gtk_thread(a, session_id, options, result_tx) {
            Ok(()) => {
                let _ = ready_tx.try_send(Ok(()));
            }
            Err(err) => {
                let _ = ready_tx.try_send(Err(err));
            }
        }
    });

    match ready_rx.recv_timeout(Duration::from_millis(READY_TIMEOUT_MS)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(SignInError::Unavailable(
                "GTK4 main loop did not respond within 400ms (headless runtime)".into(),
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(SignInError::Unavailable(
                "Linux popup setup aborted before window creation".into(),
            ));
        }
    }

    match result_rx.recv_timeout(timeout) {
        Ok(res) => res,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            invoke_on_main_loop(a, move || {
                finish_session(a, session_id, Err(SignInError::Timeout), true);
            });
            Err(SignInError::Timeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(WebSignInOutcome {
            ok: false,
            url: String::new(),
            cookie: String::new(),
            cookies: HashMap::new(),
            cancelled: true,
        }),
    }
}

pub(crate) async fn fetch_session_cookies(
    target_url: &str,
    incognito: bool,
) -> Option<HashMap<String, String>> {
    if incognito || !available() {
        return None;
    }
    let target = target_url.trim().to_owned();
    if target.is_empty() {
        return None;
    }
    crate::spawn_blocking_thread(move || fetch_default_cookies_blocking(target)).await
}

pub(crate) async fn clear_session_cookies(incognito: bool) -> bool {
    if !available() {
        return false;
    }
    if incognito {
        // Ephemeral `WebKitNetworkSession` instances keep cookies in memory per popup.
        return true;
    }
    crate::spawn_blocking_thread(clear_default_cookies_blocking).await
}

fn open_on_gtk_thread(
    a: &'static GtkWebKitApi,
    session_id: u64,
    options: WebSignInOptions,
    result_tx: mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>,
) -> Result<(), SignInError> {
    let window = unsafe { (a.gtk_window_new)() };
    if window.is_null() {
        return Err(SignInError::Platform("gtk_window_new returned null".into()));
    }

    let title_str = if options.title.trim().is_empty() {
        "Sign In"
    } else {
        options.title.trim()
    };
    if let Ok(title_c) = CString::new(title_str) {
        unsafe { (a.gtk_window_set_title)(window, title_c.as_ptr()) };
    }
    unsafe {
        (a.gtk_window_set_default_size)(window, POPUP_WIDTH, POPUP_HEIGHT);
        (a.gtk_window_set_resizable)(window, 1);
    }

    let (network_session, owns_network_session) = if options.incognito {
        let s = unsafe { (a.webkit_network_session_new_ephemeral)() };
        (s, true)
    } else {
        let s = unsafe { (a.webkit_network_session_get_default)() };
        (s, false)
    };
    if network_session.is_null() {
        unsafe { (a.gtk_window_destroy)(window) };
        return Err(SignInError::Platform(
            "failed to create WebKitNetworkSession".into(),
        ));
    }

    let cookie_manager = unsafe { (a.webkit_network_session_get_cookie_manager)(network_session) };
    let web_view = if options.incognito {
        unsafe {
            let ty = (a.webkit_web_view_get_type)();
            (a.g_object_new_with_prop)(
                ty,
                c"network-session".as_ptr(),
                network_session,
                std::ptr::null(),
            )
        }
    } else {
        unsafe { (a.webkit_web_view_new)() }
    };
    if web_view.is_null() {
        if owns_network_session {
            unsafe { (a.g_object_unref)(network_session) };
        }
        unsafe { (a.gtk_window_destroy)(window) };
        return Err(SignInError::Platform(
            "failed to create WebKitWebView".into(),
        ));
    }

    if let Some(ua) = options
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Ok(ua_c) = CString::new(ua)
    {
        unsafe {
            let settings = (a.webkit_web_view_get_settings)(web_view);
            if !settings.is_null() {
                (a.webkit_settings_set_user_agent)(settings, ua_c.as_ptr());
            }
        }
    }

    let sid_ptr = session_id as usize as *mut c_void;
    let close_sig = unsafe {
        (a.g_signal_connect_data)(
            window,
            c"close-request".as_ptr(),
            on_window_close_request as *const c_void,
            sid_ptr,
            None,
            0,
        )
    };
    let load_sig = unsafe {
        (a.g_signal_connect_data)(
            web_view,
            c"load-changed".as_ptr(),
            on_web_load_changed as *const c_void,
            sid_ptr,
            None,
            0,
        )
    };
    let create_sig = unsafe {
        (a.g_signal_connect_data)(
            web_view,
            c"create".as_ptr(),
            on_web_create_popup as *const c_void,
            sid_ptr,
            None,
            0,
        )
    };

    unsafe {
        (a.gtk_window_set_child)(window, web_view);
    }

    let initial_url = options.url.trim().to_owned();
    let target_url = options
        .cookies_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&initial_url)
        .to_owned();

    let timer_id =
        unsafe { (a.g_timeout_add_full)(0, POLL_INTERVAL_MS, on_poll_timer, sid_ptr, None) };

    let state = Rc::new(GtkSession {
        target_url,
        until: options.until,
        match_url: options.match_url,
        latest_url: RefCell::new(initial_url.clone()),
        settled: Cell::new(false),
        window,
        web_view,
        cookie_manager,
        network_session,
        owns_network_session,
        timer_id: Cell::new(timer_id),
        close_sig: Cell::new(close_sig),
        load_sig: Cell::new(load_sig),
        create_sig: Cell::new(create_sig),
        tx: RefCell::new(Some(result_tx)),
    });

    ACTIVE_SESSIONS.with(|map| {
        map.borrow_mut().insert(session_id, state);
    });

    if let Ok(url_c) = CString::new(initial_url) {
        unsafe { (a.webkit_web_view_load_uri)(web_view, url_c.as_ptr()) };
    }
    unsafe { (a.gtk_window_present)(window) };
    Ok(())
}

unsafe extern "C" fn on_window_close_request(_window: *mut c_void, data: *mut c_void) -> c_int {
    let sid = data as usize as u64;
    if let Some(a) = api() {
        let last_url = ACTIVE_SESSIONS.with(|map| {
            map.borrow()
                .get(&sid)
                .map(|s| s.latest_url.borrow().clone())
                .unwrap_or_default()
        });
        finish_session(
            a,
            sid,
            Ok(WebSignInOutcome {
                ok: false,
                url: last_url,
                cookie: String::new(),
                cookies: HashMap::new(),
                cancelled: true,
            }),
            false,
        );
    }
    0 // Allow GTK to close and destroy the window.
}

unsafe extern "C" fn on_web_load_changed(
    web_view: *mut c_void,
    load_event: c_int,
    data: *mut c_void,
) {
    let sid = data as usize as u64;
    let Some(a) = api() else {
        return;
    };
    let uri = unsafe { read_c_string((a.webkit_web_view_get_uri)(web_view)) };
    if !uri.is_empty() {
        ACTIVE_SESSIONS.with(|map| {
            if let Some(sess) = map.borrow().get(&sid) {
                *sess.latest_url.borrow_mut() = uri;
            }
        });
    }
    if load_event == WEBKIT_LOAD_FINISHED {
        poll_session(a, sid);
    }
}

unsafe extern "C" fn on_web_create_popup(
    web_view: *mut c_void,
    navigation_action: *mut c_void,
    _data: *mut c_void,
) -> *mut c_void {
    if let Some(a) = api()
        && !navigation_action.is_null()
    {
        unsafe {
            let req = (a.webkit_navigation_action_get_request)(navigation_action);
            if !req.is_null() {
                (a.webkit_web_view_load_request)(web_view, req);
            }
        }
    }
    std::ptr::null_mut()
}

unsafe extern "C" fn on_poll_timer(data: *mut c_void) -> c_int {
    let sid = data as usize as u64;
    let Some(a) = api() else {
        return 0;
    };
    let alive =
        ACTIVE_SESSIONS.with(|map| map.borrow().get(&sid).is_some_and(|s| !s.settled.get()));
    if !alive {
        return 0; // G_SOURCE_REMOVE
    }
    poll_session(a, sid);
    1 // G_SOURCE_CONTINUE
}

struct PollCookieReq {
    session_id: u64,
    target_url: String,
}

fn poll_session(a: &'static GtkWebKitApi, session_id: u64) {
    let session = ACTIVE_SESSIONS.with(|map| map.borrow().get(&session_id).cloned());
    let Some(session) = session else {
        return;
    };
    if session.settled.get() || session.cookie_manager.is_null() {
        return;
    }

    let uri = unsafe { read_c_string((a.webkit_web_view_get_uri)(session.web_view)) };
    if !uri.is_empty() {
        *session.latest_url.borrow_mut() = uri;
    }

    let Ok(target_c) = CString::new(session.target_url.as_str()) else {
        return;
    };
    let req = Box::new(PollCookieReq {
        session_id,
        target_url: session.target_url.clone(),
    });
    unsafe {
        (a.webkit_cookie_manager_get_cookies)(
            session.cookie_manager,
            target_c.as_ptr(),
            std::ptr::null_mut(),
            on_poll_cookies_ready,
            Box::into_raw(req) as *mut c_void,
        );
    }
}

unsafe extern "C" fn on_poll_cookies_ready(
    source_obj: *mut c_void,
    res: *mut c_void,
    user_data: *mut c_void,
) {
    let req: Box<PollCookieReq> = unsafe { Box::from_raw(user_data as *mut PollCookieReq) };
    let Some(a) = api() else {
        return;
    };
    let mut err: *mut c_void = std::ptr::null_mut();
    let list = unsafe { (a.webkit_cookie_manager_get_cookies_finish)(source_obj, res, &mut err) };
    if !err.is_null() {
        unsafe { (a.g_error_free)(err) };
        return;
    }
    let cookie_map = unsafe { extract_soup_cookie_list(a, list, &req.target_url) };

    let Some(sess) = ACTIVE_SESSIONS.with(|map| map.borrow().get(&req.session_id).cloned()) else {
        return;
    };
    if sess.settled.get() {
        return;
    }

    let latest = sess.latest_url.borrow().clone();
    let done = if !sess.until.is_empty() {
        cookies::satisfies_until_cookies(&cookie_map, &sess.until)
    } else if let Some(ref pat) = sess.match_url {
        cookies::url_matches_pattern(&latest, pat)
    } else {
        false
    };

    if done {
        let header = cookies::format_cookie_header(&cookie_map);
        finish_session(
            a,
            req.session_id,
            Ok(WebSignInOutcome {
                ok: true,
                url: latest,
                cookie: header,
                cookies: cookie_map,
                cancelled: false,
            }),
            true,
        );
    }
}

unsafe fn extract_soup_cookie_list(
    a: &'static GtkWebKitApi,
    list: *mut GList,
    target_url: &str,
) -> HashMap<String, String> {
    let mut matched: HashMap<String, (String, (usize, bool, usize))> = HashMap::new();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut node = list;
    while !node.is_null() {
        let cookie = unsafe { (*node).data };
        if !cookie.is_null() {
            let expires = unsafe { (a.soup_cookie_get_expires)(cookie) };
            let expired =
                !expires.is_null() && unsafe { (a.g_date_time_to_unix)(expires) } <= now_secs;
            if !expired {
                let name = unsafe { read_c_string((a.soup_cookie_get_name)(cookie)) };
                let value = unsafe { read_c_string((a.soup_cookie_get_value)(cookie)) };
                let domain = unsafe { read_c_string((a.soup_cookie_get_domain)(cookie)) };
                let path = unsafe { read_c_string((a.soup_cookie_get_path)(cookie)) };
                let is_secure = unsafe { (a.soup_cookie_get_secure)(cookie) != 0 };
                if !name.trim().is_empty()
                    && !value.trim().is_empty()
                    && cookies::cookie_matches_target(&domain, &path, is_secure, target_url)
                {
                    let spec = cookies::cookie_specificity(&domain, &path);
                    let replace = matched
                        .get(&name)
                        .is_none_or(|(_, prev_spec)| spec >= *prev_spec);
                    if replace {
                        matched.insert(name, (value, spec));
                    }
                }
            }
            unsafe { (a.soup_cookie_free)(cookie) };
        }
        node = unsafe { (*node).next };
    }
    if !list.is_null() {
        unsafe { (a.g_list_free)(list) };
    }
    matched.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn finish_session(
    a: &'static GtkWebKitApi,
    session_id: u64,
    outcome: Result<WebSignInOutcome, SignInError>,
    destroy_window: bool,
) {
    let session = ACTIVE_SESSIONS.with(|map| map.borrow_mut().remove(&session_id));
    let Some(session) = session else {
        return;
    };
    if session.settled.replace(true) {
        return;
    }

    let tid = session.timer_id.replace(0);
    if tid != 0 {
        unsafe { (a.g_source_remove)(tid) };
    }

    let close_sig = session.close_sig.replace(0);
    if close_sig != 0 {
        unsafe { (a.g_signal_handler_disconnect)(session.window, close_sig) };
    }
    let load_sig = session.load_sig.replace(0);
    if load_sig != 0 {
        unsafe { (a.g_signal_handler_disconnect)(session.web_view, load_sig) };
    }
    let create_sig = session.create_sig.replace(0);
    if create_sig != 0 {
        unsafe { (a.g_signal_handler_disconnect)(session.web_view, create_sig) };
    }

    unsafe {
        (a.webkit_web_view_stop_loading)(session.web_view);
        if destroy_window {
            (a.gtk_window_destroy)(session.window);
        }
        if session.owns_network_session && !session.network_session.is_null() {
            (a.g_object_unref)(session.network_session);
        }
    }

    if let Some(tx) = session.tx.borrow_mut().take() {
        let _ = tx.try_send(outcome);
    }
}

struct FetchCookiesCtx {
    target_url: String,
    tx: mpsc::SyncSender<Option<HashMap<String, String>>>,
}

fn fetch_default_cookies_blocking(target_url: String) -> Option<HashMap<String, String>> {
    let a = api()?;
    let (tx, rx) = mpsc::sync_channel::<Option<HashMap<String, String>>>(1);
    invoke_on_main_loop(a, move || {
        let session = unsafe { (a.webkit_network_session_get_default)() };
        if session.is_null() {
            let _ = tx.try_send(None);
            return;
        }
        let mgr = unsafe { (a.webkit_network_session_get_cookie_manager)(session) };
        let Ok(target_c) = CString::new(target_url.as_str()) else {
            let _ = tx.try_send(None);
            return;
        };
        if mgr.is_null() {
            let _ = tx.try_send(None);
            return;
        }
        let ctx = Box::new(FetchCookiesCtx { target_url, tx });
        unsafe {
            (a.webkit_cookie_manager_get_cookies)(
                mgr,
                target_c.as_ptr(),
                std::ptr::null_mut(),
                on_fetch_cookies_ready,
                Box::into_raw(ctx) as *mut c_void,
            );
        }
    });
    rx.recv_timeout(Duration::from_millis(COOKIE_OP_TIMEOUT_MS))
        .ok()
        .flatten()
}

unsafe extern "C" fn on_fetch_cookies_ready(
    source_obj: *mut c_void,
    res: *mut c_void,
    user_data: *mut c_void,
) {
    let ctx: Box<FetchCookiesCtx> = unsafe { Box::from_raw(user_data as *mut FetchCookiesCtx) };
    let Some(a) = api() else {
        let _ = ctx.tx.try_send(None);
        return;
    };
    let mut err: *mut c_void = std::ptr::null_mut();
    let list = unsafe { (a.webkit_cookie_manager_get_cookies_finish)(source_obj, res, &mut err) };
    if !err.is_null() {
        unsafe { (a.g_error_free)(err) };
        let _ = ctx.tx.try_send(None);
        return;
    }
    let map = unsafe { extract_soup_cookie_list(a, list, &ctx.target_url) };
    let _ = ctx.tx.try_send(Some(map));
}

struct ClearAllCtx {
    tx: mpsc::SyncSender<bool>,
}

struct DeleteOneCtx {
    remaining: Arc<AtomicUsize>,
    tx: mpsc::SyncSender<bool>,
}

fn clear_default_cookies_blocking() -> bool {
    let Some(a) = api() else {
        return false;
    };
    let (Some(get_all), Some(_get_all_fin), Some(_del), Some(_del_fin)) = (
        a.webkit_cookie_manager_get_all_cookies,
        a.webkit_cookie_manager_get_all_cookies_finish,
        a.webkit_cookie_manager_delete_cookie,
        a.webkit_cookie_manager_delete_cookie_finish,
    ) else {
        return false;
    };

    let (tx, rx) = mpsc::sync_channel::<bool>(1);
    invoke_on_main_loop(a, move || {
        let session = unsafe { (a.webkit_network_session_get_default)() };
        if session.is_null() {
            let _ = tx.try_send(false);
            return;
        }
        let mgr = unsafe { (a.webkit_network_session_get_cookie_manager)(session) };
        if mgr.is_null() {
            let _ = tx.try_send(false);
            return;
        }
        unsafe { (a.g_object_ref)(mgr) };
        let ctx = Box::new(ClearAllCtx { tx });
        unsafe {
            get_all(
                mgr,
                std::ptr::null_mut(),
                on_clear_all_fetched,
                Box::into_raw(ctx) as *mut c_void,
            );
        }
    });
    rx.recv_timeout(Duration::from_millis(COOKIE_OP_TIMEOUT_MS))
        .unwrap_or(false)
}

unsafe extern "C" fn on_clear_all_fetched(
    source_obj: *mut c_void,
    res: *mut c_void,
    user_data: *mut c_void,
) {
    let ctx: Box<ClearAllCtx> = unsafe { Box::from_raw(user_data as *mut ClearAllCtx) };
    let Some(a) = api() else {
        let _ = ctx.tx.try_send(false);
        return;
    };
    let (Some(get_all_fin), Some(del_cookie)) = (
        a.webkit_cookie_manager_get_all_cookies_finish,
        a.webkit_cookie_manager_delete_cookie,
    ) else {
        unsafe { (a.g_object_unref)(source_obj) };
        let _ = ctx.tx.try_send(false);
        return;
    };

    let mut err: *mut c_void = std::ptr::null_mut();
    let list = unsafe { get_all_fin(source_obj, res, &mut err) };
    if !err.is_null() {
        unsafe {
            (a.g_error_free)(err);
            (a.g_object_unref)(source_obj);
        }
        let _ = ctx.tx.try_send(false);
        return;
    }

    let mut cookies_vec = Vec::new();
    let mut node = list;
    while !node.is_null() {
        let c = unsafe { (*node).data };
        if !c.is_null() {
            cookies_vec.push(c);
        }
        node = unsafe { (*node).next };
    }
    if !list.is_null() {
        unsafe { (a.g_list_free)(list) };
    }

    if cookies_vec.is_empty() {
        unsafe { (a.g_object_unref)(source_obj) };
        let _ = ctx.tx.try_send(true);
        return;
    }

    let remaining = Arc::new(AtomicUsize::new(cookies_vec.len()));
    for c in cookies_vec {
        let del_ctx = Box::new(DeleteOneCtx {
            remaining: Arc::clone(&remaining),
            tx: ctx.tx.clone(),
        });
        unsafe {
            del_cookie(
                source_obj,
                c,
                std::ptr::null_mut(),
                Some(on_delete_cookie_finished),
                Box::into_raw(del_ctx) as *mut c_void,
            );
            (a.soup_cookie_free)(c);
        }
    }
    unsafe { (a.g_object_unref)(source_obj) };
}

unsafe extern "C" fn on_delete_cookie_finished(
    source_obj: *mut c_void,
    res: *mut c_void,
    user_data: *mut c_void,
) {
    let ctx: Box<DeleteOneCtx> = unsafe { Box::from_raw(user_data as *mut DeleteOneCtx) };
    if let Some(a) = api()
        && let Some(del_fin) = a.webkit_cookie_manager_delete_cookie_finish
    {
        let mut err: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = del_fin(source_obj, res, &mut err);
            if !err.is_null() {
                (a.g_error_free)(err);
            }
        }
    }
    if ctx.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
        let _ = ctx.tx.try_send(true);
    }
}

unsafe fn read_c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
