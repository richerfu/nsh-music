//! Windows native Win32 STA popup `HWND` + WebView2 sign-in backend.
//!
//! Spawns a dedicated Single-Threaded Apartment (STA) Win32 window hosting
//! WebView2 (`InPrivate` mode when `incognito` is true), polls
//! `ICoreWebView2_2::CookieManager` every 500ms (plus on `NavigationCompleted`)
//! to capture `HttpOnly` cookies, handles `NewWindowRequested` popups in-place,
//! and supports `WM_CLOSE` cancellation, timeout cleanup, and persistent
//! session cookie extraction/clearing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Once, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2_2, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2Environment10, ICoreWebView2Settings2,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    GetCookiesCompletedHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    take_pwstr,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetSystemMetrics, HWND_MESSAGE, KillTimer, MSG,
    PostQuitMessage, RegisterClassExW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SetForegroundWindow,
    SetTimer, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_SIZE,
    WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows_core::{HRESULT, HSTRING, Interface, PCWSTR, PWSTR, w};

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome, cookies};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const COOKIE_OP_TIMEOUT_MS: u64 = 4_000;
const POLL_INTERVAL_MS: u32 = 500;
const POPUP_WIDTH_DIP: i32 = 480;
const POPUP_HEIGHT_DIP: i32 = 640;
const POLL_TIMER_ID: usize = 1;
const TIMEOUT_TIMER_ID: usize = 2;

static REGISTER_CLASS_ONCE: Once = Once::new();
const WINDOW_CLASS: PCWSTR = w!("NshWebSignInPopupWindow");

// Static linking on MSVC (`WebView2LoaderStatic.lib`), dynamic `LoadLibraryW("WebView2Loader.dll")` on GNU.
#[cfg(target_env = "msvc")]
#[link(name = "WebView2LoaderStatic", kind = "static")]
unsafe extern "system" {
    #[link_name = "CreateCoreWebView2EnvironmentWithOptions"]
    fn create_environment(
        browser_folder: PCWSTR,
        user_data_folder: PCWSTR,
        options: *mut c_void,
        handler: *mut c_void,
    ) -> HRESULT;

    #[link_name = "GetAvailableCoreWebView2BrowserVersionString"]
    fn get_available_browser_version(browser_folder: PCWSTR, version_info: *mut PWSTR) -> HRESULT;
}

#[cfg(not(target_env = "msvc"))]
unsafe fn create_environment(
    browser_folder: PCWSTR,
    user_data_folder: PCWSTR,
    options: *mut c_void,
    handler: *mut c_void,
) -> HRESULT {
    use windows::Win32::Foundation::E_NOINTERFACE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_core::s;

    type CreateFn = unsafe extern "system" fn(PCWSTR, PCWSTR, *mut c_void, *mut c_void) -> HRESULT;
    unsafe {
        let Ok(module) = LoadLibraryW(w!("WebView2Loader.dll")) else {
            return E_NOINTERFACE;
        };
        let Some(sym) = GetProcAddress(module, s!("CreateCoreWebView2EnvironmentWithOptions"))
        else {
            return E_NOINTERFACE;
        };
        let f: CreateFn = std::mem::transmute(sym);
        f(browser_folder, user_data_folder, options, handler)
    }
}

#[cfg(not(target_env = "msvc"))]
unsafe fn get_available_browser_version(
    browser_folder: PCWSTR,
    version_info: *mut PWSTR,
) -> HRESULT {
    use windows::Win32::Foundation::E_NOINTERFACE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_core::s;

    type VersionFn = unsafe extern "system" fn(PCWSTR, *mut PWSTR) -> HRESULT;
    unsafe {
        let Ok(module) = LoadLibraryW(w!("WebView2Loader.dll")) else {
            return E_NOINTERFACE;
        };
        let Some(sym) = GetProcAddress(module, s!("GetAvailableCoreWebView2BrowserVersionString"))
        else {
            return E_NOINTERFACE;
        };
        let f: VersionFn = std::mem::transmute(sym);
        f(browser_folder, version_info)
    }
}

thread_local! {
    static ACTIVE_WINDOWS: RefCell<HashMap<isize, Rc<PopupSession>>> = RefCell::new(HashMap::new());
}

struct PopupSession {
    hwnd: HWND,
    target_url: String,
    initial_url: String,
    until: Vec<String>,
    match_url: Option<String>,
    user_agent: Option<String>,
    incognito: bool,
    timeout_ms: u32,
    latest_url: RefCell<String>,
    settled: Cell<bool>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    webview: RefCell<Option<ICoreWebView2>>,
    tx: RefCell<Option<mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>>>,
}

fn user_data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("nsh-web-signin");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn ensure_window_class() {
    REGISTER_CLASS_ONCE.call_once(|| unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

pub(crate) fn mode() -> SignInMode {
    if available() {
        SignInMode::NativePopup
    } else {
        SignInMode::ManualFallback
    }
}

pub(crate) fn available() -> bool {
    let mut version = PWSTR::null();
    let hr = unsafe { get_available_browser_version(PCWSTR::null(), &mut version) };
    if hr.is_err() || version.is_null() {
        return false;
    }
    let text = take_pwstr(version);
    !text.trim().is_empty()
}

pub(crate) fn open_blocking(options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    if !available() {
        return Err(SignInError::Unavailable(
            "WebView2 runtime is not installed or WebView2Loader.dll is unavailable".into(),
        ));
    }

    let timeout_ms = if options.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        options.timeout_ms
    };
    let (tx, rx) = mpsc::sync_channel::<Result<WebSignInOutcome, SignInError>>(1);

    let handle = std::thread::Builder::new()
        .name("nsh-web-signin-win32".into())
        .spawn(move || run_popup_sta(options, timeout_ms, tx));

    let Ok(join) = handle else {
        return Err(SignInError::Platform(
            "failed to spawn Windows STA thread".into(),
        ));
    };

    let outcome = match rx.recv_timeout(Duration::from_millis(timeout_ms.saturating_add(5_000))) {
        Ok(res) => res,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(SignInError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(WebSignInOutcome {
            ok: false,
            url: String::new(),
            cookie: String::new(),
            cookies: HashMap::new(),
            cancelled: true,
        }),
    };
    let _ = join.join();
    outcome
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
    crate::spawn_blocking_thread(move || fetch_persistent_cookies_blocking(target)).await
}

pub(crate) async fn clear_session_cookies(incognito: bool) -> bool {
    if !available() {
        return false;
    }
    if incognito {
        // InPrivate WebView2 controllers keep cookies strictly in memory per session.
        return true;
    }
    crate::spawn_blocking_thread(clear_persistent_cookies_blocking).await
}

fn run_popup_sta(
    options: WebSignInOptions,
    timeout_ms: u64,
    tx: mpsc::SyncSender<Result<WebSignInOutcome, SignInError>>,
) {
    let com_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let com_initialized = com_hr.is_ok();

    ensure_window_class();

    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    let client_w = (POPUP_WIDTH_DIP * dpi) / 96;
    let client_h = (POPUP_HEIGHT_DIP * dpi) / 96;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    let _ =
        unsafe { AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)) };
    let win_w = (rect.right - rect.left).max(360);
    let win_h = (rect.bottom - rect.top).max(480);

    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let pos_x = if screen_w > win_w {
        (screen_w - win_w) / 2
    } else {
        CW_USEDEFAULT
    };
    let pos_y = if screen_h > win_h {
        (screen_h - win_h) / 2
    } else {
        CW_USEDEFAULT
    };

    let title_str = if options.title.trim().is_empty() {
        "Sign In"
    } else {
        options.title.trim()
    };
    let title_h = HSTRING::from(title_str);
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }.unwrap_or_default();

    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            &title_h,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            pos_x,
            pos_y,
            win_w,
            win_h,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    } {
        Ok(h) => h,
        Err(err) => {
            let _ = tx.try_send(Err(SignInError::Platform(format!(
                "CreateWindowExW failed: {err}"
            ))));
            if com_initialized {
                unsafe { CoUninitialize() };
            }
            return;
        }
    };

    let initial_url = options.url.trim().to_owned();
    let target_url = options
        .cookies_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&initial_url)
        .to_owned();

    let session = Rc::new(PopupSession {
        hwnd,
        target_url,
        initial_url: initial_url.clone(),
        until: options.until,
        match_url: options.match_url,
        user_agent: options
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        incognito: options.incognito,
        timeout_ms: timeout_ms.min(u32::MAX as u64) as u32,
        latest_url: RefCell::new(initial_url),
        settled: Cell::new(false),
        controller: RefCell::new(None),
        webview: RefCell::new(None),
        tx: RefCell::new(Some(tx)),
    });

    let hwnd_key = hwnd.0 as isize;
    ACTIVE_WINDOWS.with(|map| {
        map.borrow_mut().insert(hwnd_key, Rc::clone(&session));
    });

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(Some(hwnd), TIMEOUT_TIMER_ID, session.timeout_ms, None);
    }

    let data_dir_h = HSTRING::from(user_data_dir().to_string_lossy().as_ref());
    let session_env = Rc::clone(&session);
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |hr, env: Option<ICoreWebView2Environment>| {
            if let Err(err) = hr {
                settle_popup(
                    &session_env,
                    Err(SignInError::Platform(format!(
                        "WebView2 environment failed: {err}"
                    ))),
                    true,
                );
                return Ok(());
            }
            let Some(env) = env else {
                settle_popup(
                    &session_env,
                    Err(SignInError::Unavailable(
                        "WebView2 returned null environment".into(),
                    )),
                    true,
                );
                return Ok(());
            };
            attach_controller(&session_env, &env);
            Ok(())
        },
    ));

    let create_hr = unsafe {
        create_environment(
            PCWSTR::null(),
            PCWSTR(data_dir_h.as_ptr()),
            std::ptr::null_mut(),
            env_handler.as_raw(),
        )
    };
    if create_hr.is_err() {
        settle_popup(
            &session,
            Err(SignInError::Unavailable(format!(
                "CreateCoreWebView2EnvironmentWithOptions failed: {create_hr:?}"
            ))),
            true,
        );
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    ACTIVE_WINDOWS.with(|map| {
        map.borrow_mut().remove(&hwnd_key);
    });

    if com_initialized {
        unsafe { CoUninitialize() };
    }
}

fn attach_controller(session: &Rc<PopupSession>, env: &ICoreWebView2Environment) {
    let sess = Rc::clone(session);
    let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |hr, controller: Option<ICoreWebView2Controller>| {
            if let Err(err) = hr {
                settle_popup(
                    &sess,
                    Err(SignInError::Platform(format!(
                        "WebView2 controller failed: {err}"
                    ))),
                    true,
                );
                return Ok(());
            }
            let Some(controller) = controller else {
                settle_popup(
                    &sess,
                    Err(SignInError::Platform(
                        "WebView2 returned null controller".into(),
                    )),
                    true,
                );
                return Ok(());
            };

            let mut bounds = RECT::default();
            unsafe {
                let _ = GetClientRect(sess.hwnd, &mut bounds);
                let _ = controller.SetBounds(bounds);
                let _ = controller.SetIsVisible(true);
            }

            let Ok(webview) = (unsafe { controller.CoreWebView2() }) else {
                settle_popup(
                    &sess,
                    Err(SignInError::Platform("CoreWebView2 unavailable".into())),
                    true,
                );
                return Ok(());
            };

            if let Some(ref ua) = sess.user_agent
                && let Ok(settings) = unsafe { webview.Settings() }
                && let Ok(s2) = settings.cast::<ICoreWebView2Settings2>()
            {
                let _ = unsafe { s2.SetUserAgent(&HSTRING::from(ua.as_str())) };
            }

            // Redirect `target="_blank"` OAuth popups into the same WebView2 instance.
            let wv_popup = webview.clone();
            let mut new_win_token = 0i64;
            let new_win_handler =
                NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
                    if let Some(args) = args {
                        let _ = unsafe { args.SetHandled(true) };
                        let mut uri = PWSTR::null();
                        if unsafe { args.Uri(&mut uri) }.is_ok() && !uri.is_null() {
                            let target = take_pwstr(uri);
                            if !target.trim().is_empty() {
                                let _ = unsafe { wv_popup.Navigate(&HSTRING::from(target)) };
                            }
                        }
                    }
                    Ok(())
                }));
            let _ = unsafe { webview.add_NewWindowRequested(&new_win_handler, &mut new_win_token) };

            let sess_nav = Rc::clone(&sess);
            let mut nav_token = 0i64;
            let nav_handler =
                NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
                    poll_popup_cookies(&sess_nav);
                    Ok(())
                }));
            let _ = unsafe { webview.add_NavigationCompleted(&nav_handler, &mut nav_token) };

            *sess.controller.borrow_mut() = Some(controller);
            *sess.webview.borrow_mut() = Some(webview.clone());

            let _ = unsafe { webview.Navigate(&HSTRING::from(sess.initial_url.as_str())) };
            let _ = unsafe { SetTimer(Some(sess.hwnd), POLL_TIMER_ID, POLL_INTERVAL_MS, None) };
            Ok(())
        },
    ));

    let result = unsafe {
        if let Ok(env10) = env.cast::<ICoreWebView2Environment10>() {
            env10
                .CreateCoreWebView2ControllerOptions()
                .and_then(|opts| {
                    opts.SetIsInPrivateModeEnabled(session.incognito)?;
                    env10.CreateCoreWebView2ControllerWithOptions(
                        session.hwnd,
                        &opts,
                        &ctrl_handler,
                    )
                })
        } else {
            env.CreateCoreWebView2Controller(session.hwnd, &ctrl_handler)
        }
    };
    if let Err(err) = result {
        settle_popup(
            session,
            Err(SignInError::Platform(format!(
                "CreateCoreWebView2Controller failed: {err}"
            ))),
            true,
        );
    }
}

fn poll_popup_cookies(session: &Rc<PopupSession>) {
    if session.settled.get() {
        return;
    }
    let Some(webview) = session.webview.borrow().clone() else {
        return;
    };

    let mut src_pwstr = PWSTR::null();
    if unsafe { webview.Source(&mut src_pwstr) }.is_ok() && !src_pwstr.is_null() {
        let src = take_pwstr(src_pwstr);
        if !src.trim().is_empty() {
            *session.latest_url.borrow_mut() = src;
        }
    }

    let Ok(wv2) = webview.cast::<ICoreWebView2_2>() else {
        return;
    };
    let Ok(cookie_mgr) = (unsafe { wv2.CookieManager() }) else {
        return;
    };

    let sess = Rc::clone(session);
    let target_url = session.target_url.clone();
    let handler = GetCookiesCompletedHandler::create(Box::new(move |hr, list| {
        if hr.is_err() || sess.settled.get() {
            return Ok(());
        }
        let cookie_map = extract_cookie_list(list.as_ref(), &target_url);
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
            settle_popup(
                &sess,
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
        Ok(())
    }));

    let _ = unsafe { cookie_mgr.GetCookies(&HSTRING::from(session.target_url.as_str()), &handler) };
}

fn extract_cookie_list(
    list: Option<&webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CookieList>,
    target_url: &str,
) -> HashMap<String, String> {
    let mut matched: HashMap<String, (String, (usize, bool, usize))> = HashMap::new();
    let Some(list) = list else {
        return HashMap::new();
    };
    let mut count = 0u32;
    if unsafe { list.Count(&mut count) }.is_err() {
        return HashMap::new();
    }
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    for idx in 0..count {
        let Ok(cookie) = (unsafe { list.GetValueAtIndex(idx) }) else {
            continue;
        };
        let mut is_session = windows_core::BOOL(0);
        let _ = unsafe { cookie.IsSession(&mut is_session) };
        if !is_session.as_bool() {
            let mut expires = 0.0f64;
            if unsafe { cookie.Expires(&mut expires) }.is_ok()
                && expires > 0.0
                && expires <= now_secs
            {
                continue;
            }
        }
        let mut name_p = PWSTR::null();
        let mut val_p = PWSTR::null();
        let mut dom_p = PWSTR::null();
        let mut path_p = PWSTR::null();
        if unsafe { cookie.Name(&mut name_p) }.is_err() || name_p.is_null() {
            continue;
        }
        if unsafe { cookie.Value(&mut val_p) }.is_err() || val_p.is_null() {
            let _ = take_pwstr(name_p);
            continue;
        }
        let name = take_pwstr(name_p);
        let value = take_pwstr(val_p);
        let domain = if unsafe { cookie.Domain(&mut dom_p) }.is_ok() && !dom_p.is_null() {
            take_pwstr(dom_p)
        } else {
            String::new()
        };
        let path = if unsafe { cookie.Path(&mut path_p) }.is_ok() && !path_p.is_null() {
            take_pwstr(path_p)
        } else {
            "/".into()
        };
        if name.trim().is_empty() || value.trim().is_empty() {
            continue;
        }
        let mut is_secure = windows_core::BOOL(0);
        let _ = unsafe { cookie.IsSecure(&mut is_secure) };
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
    matched.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn settle_popup(
    session: &Rc<PopupSession>,
    outcome: Result<WebSignInOutcome, SignInError>,
    destroy_window: bool,
) {
    if session.settled.replace(true) {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(session.hwnd), POLL_TIMER_ID);
        let _ = KillTimer(Some(session.hwnd), TIMEOUT_TIMER_ID);
    }
    if let Some(ctrl) = session.controller.borrow_mut().take() {
        let _ = unsafe { ctrl.Close() };
    }
    session.webview.borrow_mut().take();
    if let Some(tx) = session.tx.borrow_mut().take() {
        let _ = tx.try_send(outcome);
    }
    if destroy_window {
        let _ = unsafe { DestroyWindow(session.hwnd) };
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let key = hwnd.0 as isize;
    match msg {
        WM_SIZE => {
            ACTIVE_WINDOWS.with(|map| {
                if let Some(sess) = map.borrow().get(&key)
                    && let Some(ctrl) = sess.controller.borrow().as_ref()
                {
                    let mut bounds = RECT::default();
                    unsafe {
                        let _ = GetClientRect(hwnd, &mut bounds);
                        let _ = ctrl.SetBounds(bounds);
                    }
                }
            });
            LRESULT(0)
        }
        WM_TIMER => {
            let sess = ACTIVE_WINDOWS.with(|map| map.borrow().get(&key).cloned());
            if let Some(sess) = sess {
                if wparam.0 == POLL_TIMER_ID {
                    poll_popup_cookies(&sess);
                } else if wparam.0 == TIMEOUT_TIMER_ID {
                    settle_popup(&sess, Err(SignInError::Timeout), true);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let sess = ACTIVE_WINDOWS.with(|map| map.borrow().get(&key).cloned());
            if let Some(sess) = sess {
                let last_url = sess.latest_url.borrow().clone();
                settle_popup(
                    &sess,
                    Ok(WebSignInOutcome {
                        ok: false,
                        url: last_url,
                        cookie: String::new(),
                        cookies: HashMap::new(),
                        cancelled: true,
                    }),
                    true,
                );
            } else {
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn fetch_persistent_cookies_blocking(target_url: String) -> Option<HashMap<String, String>> {
    run_headless_cookie_op(move |webview, done_tx| {
        let Ok(wv2) = webview.cast::<ICoreWebView2_2>() else {
            let _ = done_tx.try_send(None);
            return;
        };
        let Ok(cookie_mgr) = (unsafe { wv2.CookieManager() }) else {
            let _ = done_tx.try_send(None);
            return;
        };
        let target = target_url.clone();
        let done_tx_cb = done_tx.clone();
        let handler = GetCookiesCompletedHandler::create(Box::new(move |hr, list| {
            let out = if hr.is_ok() {
                Some(extract_cookie_list(list.as_ref(), &target))
            } else {
                None
            };
            let _ = done_tx_cb.try_send(out);
            unsafe { PostQuitMessage(0) };
            Ok(())
        }));
        if unsafe { cookie_mgr.GetCookies(&HSTRING::from(target_url.as_str()), &handler) }.is_err()
        {
            let _ = done_tx.try_send(None);
            unsafe { PostQuitMessage(0) };
        }
    })
    .flatten()
}

fn clear_persistent_cookies_blocking() -> bool {
    run_headless_cookie_op(move |webview, done_tx| {
        let ok = webview
            .cast::<ICoreWebView2_2>()
            .and_then(|wv2| unsafe { wv2.CookieManager() })
            .and_then(|mgr| unsafe { mgr.DeleteAllCookies() })
            .is_ok();
        let _ = done_tx.try_send(ok);
        unsafe { PostQuitMessage(0) };
    })
    .unwrap_or(false)
}

fn run_headless_cookie_op<T, F>(op: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(ICoreWebView2, mpsc::SyncSender<T>) + 'static,
{
    let com_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let com_initialized = com_hr.is_ok();
    ensure_window_class();

    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }.unwrap_or_default();
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            w!("NshWebSignInCookieHost"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            1,
            1,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        )
    }) else {
        if com_initialized {
            unsafe { CoUninitialize() };
        }
        return None;
    };

    let (tx, rx) = mpsc::sync_channel::<T>(1);
    let op_slot = Rc::new(RefCell::new(Some(op)));
    let controller_hold: Rc<RefCell<Option<ICoreWebView2Controller>>> = Rc::new(RefCell::new(None));
    let ctrl_hold_env = Rc::clone(&controller_hold);

    let data_dir_h = HSTRING::from(user_data_dir().to_string_lossy().as_ref());
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |hr, env: Option<ICoreWebView2Environment>| {
            let (Ok(()), Some(env)) = (hr, env) else {
                unsafe { PostQuitMessage(0) };
                return Ok(());
            };
            let op_slot_inner = Rc::clone(&op_slot);
            let ctrl_hold_inner = Rc::clone(&ctrl_hold_env);
            let tx_inner = tx.clone();
            let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |hr2, controller: Option<ICoreWebView2Controller>| {
                    let (Ok(()), Some(controller)) = (hr2, controller) else {
                        unsafe { PostQuitMessage(0) };
                        return Ok(());
                    };
                    let Ok(webview) = (unsafe { controller.CoreWebView2() }) else {
                        unsafe { PostQuitMessage(0) };
                        return Ok(());
                    };
                    *ctrl_hold_inner.borrow_mut() = Some(controller);
                    if let Some(f) = op_slot_inner.borrow_mut().take() {
                        f(webview, tx_inner.clone());
                    }
                    Ok(())
                },
            ));
            if unsafe { env.CreateCoreWebView2Controller(hwnd, &ctrl_handler) }.is_err() {
                unsafe { PostQuitMessage(0) };
            }
            Ok(())
        },
    ));

    unsafe {
        let _ = SetTimer(
            Some(hwnd),
            TIMEOUT_TIMER_ID,
            COOKIE_OP_TIMEOUT_MS as u32,
            None,
        );
    }

    let hr = unsafe {
        create_environment(
            PCWSTR::null(),
            PCWSTR(data_dir_h.as_ptr()),
            std::ptr::null_mut(),
            env_handler.as_raw(),
        )
    };
    if hr.is_ok() {
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_TIMER && msg.wParam.0 == TIMEOUT_TIMER_ID {
                    PostQuitMessage(0);
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    if let Some(ctrl) = controller_hold.borrow_mut().take() {
        let _ = unsafe { ctrl.Close() };
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMEOUT_TIMER_ID);
        let _ = DestroyWindow(hwnd);
        if com_initialized {
            CoUninitialize();
        }
    }
    rx.try_recv().ok()
}
