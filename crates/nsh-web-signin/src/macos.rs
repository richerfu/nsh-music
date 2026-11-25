//! macOS native popup `NSWindow` + `WKWebView` sign-in backend.
//!
//! Creates an isolated `WKWebView` (`WKWebsiteDataStore.nonPersistentDataStore`
//! when `incognito` is true) inside an independent `NSWindow`, polls
//! `WKHTTPCookieStore` every 500ms (plus on `didFinishNavigation:`) to capture
//! `HttpOnly` cookies, handles OAuth `target="_blank"` popups in-place via
//! `WKUIDelegate`, and tears down cleanly without leaks or double-close crashes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
    NSURLRequest,
};

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome, cookies};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const READY_TIMEOUT_MS: u64 = 400;
const COOKIE_OP_TIMEOUT_MS: u64 = 2_000;
const POLL_INTERVAL_SECS: f64 = 0.5;
const POPUP_WIDTH: f64 = 480.0;
const POPUP_HEIGHT: f64 = 640.0;
/// `NSViewWidthSizable (2) | NSViewHeightSizable (16)`
const AUTORESIZE_WIDTH_HEIGHT: usize = 18;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    static NSApp: *const AnyObject;
}

thread_local! {
    static ACTIVE_SESSIONS: RefCell<HashMap<u64, Rc<SessionState>>> = RefCell::new(HashMap::new());
    static DEFERRED_DROPS: RefCell<Vec<Rc<SessionState>>> = const { RefCell::new(Vec::new()) };
    static DEFERRED_DELEGATES: RefCell<Vec<Retained<NshWebSignInDelegate>>> =
        const { RefCell::new(Vec::new()) };
}

struct SessionState {
    target_url: String,
    until: Vec<String>,
    match_url: Option<String>,
    latest_url: RefCell<String>,
    settled: Cell<bool>,
    tx: RefCell<Option<mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>>>,
    window: Retained<NSWindow>,
    web: Retained<AnyObject>,
    delegate: RefCell<Option<Retained<NshWebSignInDelegate>>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

#[derive(Default)]
struct DelegateIvars {
    session_id: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    #[name = "NshWebSignInDelegate"]
    struct NshWebSignInDelegate;

    unsafe impl NSObjectProtocol for NshWebSignInDelegate {}

    impl NshWebSignInDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let sid = self.ivars().session_id.get();
            let last_url = ACTIVE_SESSIONS.with(|map| {
                map.borrow()
                    .get(&sid)
                    .map(|s| s.latest_url.borrow().clone())
                    .unwrap_or_default()
            });
            finish_session(
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

        #[unsafe(method(webView:didFinishNavigation:))]
        fn web_view_did_finish_navigation(
            &self,
            web: &AnyObject,
            _navigation: Option<&AnyObject>,
        ) {
            let sid = self.ivars().session_id.get();
            update_latest_url(sid, web);
            poll_session(sid);
        }

        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn web_view_create_popup(
            &self,
            web: &AnyObject,
            _configuration: &AnyObject,
            action: &AnyObject,
            _features: &AnyObject,
        ) -> *mut AnyObject {
            // Redirect `target="_blank"` OAuth popups (Google, 2FA, etc.) into the same WKWebView.
            unsafe {
                let req: Option<Retained<AnyObject>> = msg_send![action, request];
                if let Some(req) = req {
                    let _: Option<Retained<AnyObject>> = msg_send![web, loadRequest: &*req];
                }
            }
            std::ptr::null_mut()
        }

        #[unsafe(method(nshWebSignInPollTimer:))]
        fn on_poll_timer(&self, _timer: &AnyObject) {
            poll_session(self.ivars().session_id.get());
        }
    }
);

impl NshWebSignInDelegate {
    fn new(mtm: MainThreadMarker, session_id: u64) -> Retained<Self> {
        let ivars = DelegateIvars {
            session_id: Cell::new(session_id),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: `NSObject` designated initializer.
        unsafe { msg_send![super(this), init] }
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
    // SAFETY: `NSApp` is AppKit's global pointer to the shared `NSApplication` (null in headless tests).
    if unsafe { NSApp }.is_null() {
        return false;
    }
    if let Some(mtm) = MainThreadMarker::new() {
        return NSApplication::sharedApplication(mtm).isRunning();
    }
    true
}

pub(crate) fn open_blocking(options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    // Fast 0ms headless check so `cargo test` never touches AppKit or deadlocks.
    if unsafe { NSApp }.is_null() {
        return Err(SignInError::Unavailable(
            "NSApplication is not initialized (headless runtime)".into(),
        ));
    }
    if MainThreadMarker::new().is_some() {
        return Err(SignInError::Platform(
            "open_blocking must not be called from the macOS main thread".into(),
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

    DispatchQueue::main().exec_async(move || {
        match open_on_main_thread(session_id, options, result_tx) {
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
                "macOS main runloop did not respond within 400ms (headless runtime)".into(),
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(SignInError::Unavailable(
                "macOS popup setup aborted before window creation".into(),
            ));
        }
    }

    match result_rx.recv_timeout(timeout) {
        Ok(res) => res,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            DispatchQueue::main().exec_async(move || {
                finish_session(session_id, Err(SignInError::Timeout), true);
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
    // Non-persistent WKWebsiteDataStores are per-WKWebView; there is no global incognito jar.
    if incognito || unsafe { NSApp }.is_null() {
        return None;
    }
    let target = target_url.trim().to_owned();
    if target.is_empty() {
        return None;
    }
    crate::spawn_blocking_thread(move || fetch_default_cookies_blocking(target)).await
}

pub(crate) async fn clear_session_cookies(incognito: bool) -> bool {
    if unsafe { NSApp }.is_null() {
        return false;
    }
    crate::spawn_blocking_thread(move || clear_default_cookies_blocking(incognito)).await
}

fn fetch_default_cookies_blocking(target_url: String) -> Option<HashMap<String, String>> {
    if MainThreadMarker::new().is_some() {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel::<Option<HashMap<String, String>>>(1);
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            let _ = tx.try_send(None);
            return;
        };
        if !NSApplication::sharedApplication(mtm).isRunning() || !ensure_webkit_loaded() {
            let _ = tx.try_send(None);
            return;
        }
        let Some(cookie_store) = default_http_cookie_store() else {
            let _ = tx.try_send(None);
            return;
        };
        let block = RcBlock::new(move |cookies_ptr: *mut AnyObject| {
            let map = unsafe { collect_matching_cookies(cookies_ptr, &target_url) };
            let _ = tx.try_send(Some(map));
        });
        // SAFETY: `-[WKHTTPCookieStore getAllCookies:]` called on the main thread.
        unsafe {
            let _: () = msg_send![&cookie_store, getAllCookies: &*block];
        }
    });
    rx.recv_timeout(Duration::from_millis(COOKIE_OP_TIMEOUT_MS))
        .ok()
        .flatten()
}

fn clear_default_cookies_blocking(incognito: bool) -> bool {
    if MainThreadMarker::new().is_some() {
        return false;
    }
    let (tx, rx) = mpsc::sync_channel::<bool>(1);
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            let _ = tx.try_send(false);
            return;
        };
        if !NSApplication::sharedApplication(mtm).isRunning() || !ensure_webkit_loaded() {
            let _ = tx.try_send(false);
            return;
        }
        // Each incognito popup allocates a fresh `nonPersistentDataStore`, so it starts empty.
        if incognito {
            let _ = tx.try_send(true);
            return;
        }
        let Some(cookie_store) = default_http_cookie_store() else {
            let _ = tx.try_send(false);
            return;
        };
        let store_for_block = cookie_store.clone();
        let block = RcBlock::new(move |cookies_ptr: *mut AnyObject| {
            let Some(array) = (unsafe { cookies_ptr.as_ref() }) else {
                let _ = tx.try_send(true);
                return;
            };
            let count: usize = unsafe { msg_send![array, count] };
            if count == 0 {
                let _ = tx.try_send(true);
                return;
            }
            let remaining = Arc::new(AtomicUsize::new(count));
            for idx in 0..count {
                let cookie: Option<Retained<AnyObject>> =
                    unsafe { msg_send![array, objectAtIndex: idx] };
                let Some(cookie) = cookie else {
                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                        let _ = tx.try_send(true);
                    }
                    continue;
                };
                let rem = Arc::clone(&remaining);
                let tx_done = tx.clone();
                let done_block = RcBlock::new(move || {
                    if rem.fetch_sub(1, Ordering::AcqRel) == 1 {
                        let _ = tx_done.try_send(true);
                    }
                });
                // SAFETY: `-[WKHTTPCookieStore deleteCookie:completionHandler:]` on main thread.
                unsafe {
                    let _: () = msg_send![
                        &store_for_block,
                        deleteCookie: &*cookie,
                        completionHandler: &*done_block
                    ];
                }
            }
        });
        unsafe {
            let _: () = msg_send![&cookie_store, getAllCookies: &*block];
        }
    });
    rx.recv_timeout(Duration::from_millis(COOKIE_OP_TIMEOUT_MS))
        .unwrap_or(false)
}

fn default_http_cookie_store() -> Option<Retained<AnyObject>> {
    let store_cls = AnyClass::get(c"WKWebsiteDataStore")?;
    unsafe {
        let data_store: Option<Retained<AnyObject>> = msg_send![store_cls, defaultDataStore];
        data_store.and_then(|ds| msg_send![&ds, httpCookieStore])
    }
}

fn ensure_webkit_loaded() -> bool {
    if AnyClass::get(c"WKWebView").is_some()
        && AnyClass::get(c"WKWebViewConfiguration").is_some()
        && AnyClass::get(c"WKWebsiteDataStore").is_some()
    {
        return true;
    }
    if let Some(bundle_cls) = AnyClass::get(c"NSBundle") {
        let path = NSString::from_str("/System/Library/Frameworks/WebKit.framework");
        // SAFETY: Loads the macOS system WebKit framework on the main thread.
        unsafe {
            let bundle: Option<Retained<AnyObject>> = msg_send![bundle_cls, bundleWithPath: &*path];
            if let Some(bundle) = bundle {
                let _: Bool = msg_send![&bundle, load];
            }
        }
    }
    AnyClass::get(c"WKWebView").is_some()
        && AnyClass::get(c"WKWebViewConfiguration").is_some()
        && AnyClass::get(c"WKWebsiteDataStore").is_some()
}

fn open_on_main_thread(
    session_id: u64,
    options: WebSignInOptions,
    result_tx: mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>,
) -> Result<(), SignInError> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(SignInError::Platform(
            "not executing on macOS main thread".into(),
        ));
    };
    let app = NSApplication::sharedApplication(mtm);
    if !app.isRunning() {
        return Err(SignInError::Unavailable(
            "NSApplication event loop is not running".into(),
        ));
    }
    if !ensure_webkit_loaded() {
        return Err(SignInError::Unavailable(
            "WebKit.framework could not be loaded".into(),
        ));
    }

    let Some(cfg_cls) = AnyClass::get(c"WKWebViewConfiguration") else {
        return Err(SignInError::Unavailable(
            "missing WKWebViewConfiguration".into(),
        ));
    };
    let Some(store_cls) = AnyClass::get(c"WKWebsiteDataStore") else {
        return Err(SignInError::Unavailable(
            "missing WKWebsiteDataStore".into(),
        ));
    };
    let Some(web_cls) = AnyClass::get(c"WKWebView") else {
        return Err(SignInError::Unavailable("missing WKWebView".into()));
    };

    let content_rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(POPUP_WIDTH, POPUP_HEIGHT),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;

    // SAFETY: Standard AppKit NSWindow creation on the main thread.
    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // CRITICAL: Keep NSWindow owned exclusively by `Retained<NSWindow>` so closing never double-frees.
    unsafe { window.setReleasedWhenClosed(false) };

    let title_str = if options.title.trim().is_empty() {
        "Sign In"
    } else {
        options.title.trim()
    };
    window.setTitle(&NSString::from_str(title_str));
    window.center();

    // Build isolated WKWebViewConfiguration + WKWebView.
    let config: Retained<AnyObject> = unsafe { msg_send![cfg_cls, new] };
    let data_store: Option<Retained<AnyObject>> = unsafe {
        if options.incognito {
            msg_send![store_cls, nonPersistentDataStore]
        } else {
            msg_send![store_cls, defaultDataStore]
        }
    };
    if let Some(ds) = data_store {
        unsafe {
            let _: () = msg_send![&config, setWebsiteDataStore: &*ds];
        }
    }

    let web: Option<Retained<AnyObject>> = unsafe {
        let alloc: Allocated<AnyObject> = msg_send![web_cls, alloc];
        msg_send![alloc, initWithFrame: content_rect, configuration: &*config]
    };
    let Some(web) = web else {
        return Err(SignInError::Platform(
            "failed to initialize WKWebView".into(),
        ));
    };

    if let Some(ua) = options
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let ns_ua = NSString::from_str(ua);
        unsafe {
            let _: () = msg_send![&web, setCustomUserAgent: &*ns_ua];
        }
    }
    unsafe {
        let _: () = msg_send![&web, setAutoresizingMask: AUTORESIZE_WIDTH_HEIGHT];
    }

    let delegate = NshWebSignInDelegate::new(mtm, session_id);
    unsafe {
        let _: () = msg_send![&web, setNavigationDelegate: &*delegate];
        let _: () = msg_send![&web, setUIDelegate: &*delegate];
        let _: () = msg_send![&window, setDelegate: &*delegate];
    }

    // SAFETY: `WKWebView` inherits from `NSView`.
    let web_as_view: &NSView = unsafe { &*(&*web as *const AnyObject as *const NSView) };
    window.setContentView(Some(web_as_view));

    let initial_url = options.url.trim().to_owned();
    let target_url = options
        .cookies_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&initial_url)
        .to_owned();

    let timer: Option<Retained<NSTimer>> = AnyClass::get(c"NSTimer").and_then(|timer_cls| unsafe {
        msg_send![
            timer_cls,
            scheduledTimerWithTimeInterval: POLL_INTERVAL_SECS,
            target: &*delegate,
            selector: sel!(nshWebSignInPollTimer:),
            userInfo: std::ptr::null::<AnyObject>(),
            repeats: Bool::YES
        ]
    });

    let state = Rc::new(SessionState {
        target_url,
        until: options.until,
        match_url: options.match_url,
        latest_url: RefCell::new(initial_url.clone()),
        settled: Cell::new(false),
        tx: RefCell::new(Some(result_tx)),
        window: window.clone(),
        web: web.clone(),
        delegate: RefCell::new(Some(delegate)),
        timer: RefCell::new(timer),
    });

    ACTIVE_SESSIONS.with(|map| {
        map.borrow_mut().insert(session_id, state);
    });

    if let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(&initial_url)) {
        let request = NSURLRequest::requestWithURL(&ns_url);
        unsafe {
            let _: Option<Retained<AnyObject>> = msg_send![&web, loadRequest: &*request];
        }
    }

    window.makeKeyAndOrderFront(None);
    unsafe {
        let _: () = msg_send![&app, activateIgnoringOtherApps: Bool::YES];
    }

    Ok(())
}

fn update_latest_url(session_id: u64, web: &AnyObject) {
    let current = read_web_url(web);
    if current.is_empty() {
        return;
    }
    ACTIVE_SESSIONS.with(|map| {
        if let Some(session) = map.borrow().get(&session_id) {
            *session.latest_url.borrow_mut() = current;
        }
    });
}

fn read_web_url(web: &AnyObject) -> String {
    unsafe {
        let url: Option<Retained<NSURL>> = msg_send![web, URL];
        url.and_then(|u| u.absoluteString())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

fn poll_session(session_id: u64) {
    let session = ACTIVE_SESSIONS.with(|map| map.borrow().get(&session_id).cloned());
    let Some(session) = session else {
        return;
    };
    if session.settled.get() {
        return;
    }

    let current_url = read_web_url(&session.web);
    if !current_url.is_empty() {
        *session.latest_url.borrow_mut() = current_url;
    }

    // Read `httpCookieStore` directly from this WKWebView's own websiteDataStore (incognito or default).
    let cookie_store: Option<Retained<AnyObject>> = unsafe {
        let config: Option<Retained<AnyObject>> = msg_send![&session.web, configuration];
        let data_store: Option<Retained<AnyObject>> =
            config.and_then(|cfg| msg_send![&cfg, websiteDataStore]);
        data_store.and_then(|ds| msg_send![&ds, httpCookieStore])
    };
    let Some(cookie_store) = cookie_store else {
        return;
    };

    let target_url = session.target_url.clone();
    let block = RcBlock::new(move |cookies_ptr: *mut AnyObject| {
        let Some(sess) = ACTIVE_SESSIONS.with(|map| map.borrow().get(&session_id).cloned()) else {
            return;
        };
        if sess.settled.get() {
            return;
        }

        let cookie_map = unsafe { collect_matching_cookies(cookies_ptr, &target_url) };
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
                session_id,
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
    });

    // SAFETY: `-[WKHTTPCookieStore getAllCookies:]` on main thread.
    unsafe {
        let _: () = msg_send![&cookie_store, getAllCookies: &*block];
    }
}

unsafe fn collect_matching_cookies(
    cookies_ptr: *mut AnyObject,
    target_url: &str,
) -> HashMap<String, String> {
    let mut matched: HashMap<String, (String, (usize, bool, usize))> = HashMap::new();
    // SAFETY: WebKit invokes `getAllCookies:` completion on main thread with `NSArray<NSHTTPCookie *>`.
    if let Some(array) = unsafe { cookies_ptr.as_ref() } {
        let count: usize = unsafe { msg_send![array, count] };
        for idx in 0..count {
            let cookie: Option<Retained<AnyObject>> =
                unsafe { msg_send![array, objectAtIndex: idx] };
            let Some(cookie) = cookie else {
                continue;
            };
            if unsafe { is_cookie_expired(&cookie) } {
                continue;
            }
            let name = unsafe { read_cookie_string(&cookie, "name") };
            let value = unsafe { read_cookie_string(&cookie, "value") };
            if name.trim().is_empty() || value.trim().is_empty() {
                continue;
            }
            let domain = unsafe { read_cookie_string(&cookie, "domain") };
            let path = unsafe { read_cookie_string(&cookie, "path") };
            let is_secure: Bool = unsafe { msg_send![&cookie, isSecure] };
            if !cookies::cookie_matches_target(&domain, &path, is_secure.as_bool(), target_url) {
                continue;
            }
            let spec = cookies::cookie_specificity(&domain, &path);
            let replace = matched
                .get(&name)
                .is_none_or(|(_, prev_spec)| spec >= *prev_spec);
            if replace {
                matched.insert(name, (value, spec));
            }
        }
    }
    matched.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn finish_session(
    session_id: u64,
    outcome: Result<WebSignInOutcome, SignInError>,
    close_window: bool,
) {
    let session = ACTIVE_SESSIONS.with(|map| map.borrow_mut().remove(&session_id));
    let Some(session) = session else {
        return;
    };
    if session.settled.replace(true) {
        return;
    }

    if let Some(timer) = session.timer.borrow_mut().take() {
        unsafe {
            let _: () = msg_send![&timer, invalidate];
        }
    }

    unsafe {
        let _: () = msg_send![&session.web, setNavigationDelegate: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![&session.web, setUIDelegate: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![&session.web, stopLoading];
        // Clear NSWindow delegate BEFORE closing so programmatic close never re-enters `windowWillClose:`.
        let _: () = msg_send![&session.window, setDelegate: std::ptr::null::<AnyObject>()];
    }

    if close_window {
        session.window.close();
    }

    if let Some(tx) = session.tx.borrow_mut().take() {
        let _ = tx.try_send(outcome);
    }

    // Keep delegate/session alive until the current AppKit/WebKit callback frame unwinds.
    if let Some(detached_delegate) = session.delegate.borrow_mut().take() {
        DEFERRED_DELEGATES.with(|v| v.borrow_mut().push(detached_delegate));
    }
    DEFERRED_DROPS.with(|v| v.borrow_mut().push(session));
    DispatchQueue::main().exec_async(|| {
        DEFERRED_DELEGATES.with(|v| v.borrow_mut().clear());
        DEFERRED_DROPS.with(|v| v.borrow_mut().clear());
    });
}

unsafe fn read_cookie_string(cookie: &AnyObject, prop: &str) -> String {
    let val: Option<Retained<NSString>> = unsafe {
        match prop {
            "name" => msg_send![cookie, name],
            "value" => msg_send![cookie, value],
            "domain" => msg_send![cookie, domain],
            "path" => msg_send![cookie, path],
            _ => None,
        }
    };
    val.map(|s| s.to_string()).unwrap_or_default()
}

unsafe fn is_cookie_expired(cookie: &AnyObject) -> bool {
    let expires: Option<Retained<AnyObject>> = unsafe { msg_send![cookie, expiresDate] };
    if let Some(exp) = expires {
        let remaining: f64 = unsafe { msg_send![&exp, timeIntervalSinceNow] };
        return remaining <= 0.0;
    }
    false
}
