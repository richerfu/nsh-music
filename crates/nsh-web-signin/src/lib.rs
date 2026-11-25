//! Standalone NSH native module (`nshm`) for isolated popup WebView sign-in and
//! `HttpOnly` session cookie extraction across all 6 NSH platforms.
//!
//! - **macOS**: Opens an independent `NSWindow` hosting an isolated `WKWebView`
//!   (`WKWebsiteDataStore.nonPersistentDataStore` when `incognito` is enabled)
//!   and polls `WKHTTPCookieStore` for `HttpOnly` cookies, auto-closing once
//!   `until` cookies or `match_url` match.
//! - **iOS**: Presents a native modal `UIViewController` inside a
//!   `UINavigationController` (`PageSheet`) hosting an isolated `WKWebView` and
//!   polling `WKHTTPCookieStore` for `HttpOnly` cookies.
//! - **Windows**: Opens a dedicated STA Win32 popup `HWND` hosting WebView2
//!   (`InPrivate` when `incognito` is enabled) and polls
//!   `ICoreWebView2_2::CookieManager` for `HttpOnly` cookies.
//! - **Linux**: Dynamically loads GTK4 + WebKitGTK 6.0 + libsoup 3.0 via
//!   `dlopen`, opens a `GtkWindow` + ephemeral `WebKitWebView`, and polls
//!   `WebKitCookieManager` for `HttpOnly` cookies.
//! - **Android**: Reads and clears `HttpOnly` cookies from
//!   `android.webkit.CookieManager` via JNI for NSH's embedded `web_view()` sheet.
//! - **HarmonyOS**: Reads and clears `HttpOnly` cookies via
//!   `nsh_platform_harmonyos::run_on_ui_thread` + `@ohos.web.webview`
//!   `WebCookieManager` for NSH's embedded `web_view()` sheet.
//! - **Other platforms / headless tests**: Cleanly reports
//!   [`SignInMode::ManualFallback`] and returns [`SignInError::Unavailable`].

pub mod cookies;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
mod linux;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_env = "ohos")]
mod ohos;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "android",
    target_env = "ohos"
)))]
mod fallback;

#[cfg(target_os = "android")]
pub use android::{register_java_vm, register_java_vm_raw};

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};

use nsh_core::prelude::*;
use serde::{Deserialize, Serialize};

pub use cookies::{
    cookie_matches_target, format_cookie_header, parse_cookie_header, satisfies_until_cookies,
    url_matches_cookies_of, url_matches_pattern,
};

/// Sign-in cookie capture strategy available on the current runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SignInMode {
    /// Independent native popup WebView window/sheet with direct `HttpOnly` cookie access.
    NativePopup,
    /// Embedded NSH `web_view()` sheet paired with native OS cookie-store extraction.
    EmbeddedWithNativeCookies,
    /// No native popup or cookie-store bridge available; fall back to manual cookie input.
    ManualFallback,
}

impl SignInMode {
    /// Camel-case identifier matching JSON / JS serialization.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativePopup => "nativePopup",
            Self::EmbeddedWithNativeCookies => "embeddedWithNativeCookies",
            Self::ManualFallback => "manualFallback",
        }
    }
}

/// Options configuring a native popup WebView sign-in session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSignInOptions {
    pub url: String,
    pub title: String,
    pub cookies_of: Option<String>,
    pub until: Vec<String>,
    pub match_url: Option<String>,
    pub user_agent: Option<String>,
    pub incognito: bool,
    pub timeout_ms: u64,
}

/// Outcome returned when a WebView sign-in session completes or is cancelled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSignInOutcome {
    pub ok: bool,
    pub url: String,
    pub cookie: String,
    pub cookies: HashMap<String, String>,
    pub cancelled: bool,
}

/// Errors produced by [`open_blocking`] or [`open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInError {
    InvalidRequest(String),
    Unavailable(String),
    Timeout,
    Platform(String),
}

impl fmt::Display for SignInError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(msg) => write!(f, "invalid web sign-in request: {msg}"),
            Self::Unavailable(msg) => write!(f, "web sign-in unavailable: {msg}"),
            Self::Timeout => f.write_str("web sign-in timed out"),
            Self::Platform(msg) => write!(f, "web sign-in platform error: {msg}"),
        }
    }
}

impl std::error::Error for SignInError {}

/// Pluggable backend trait enabling custom host presenters or deterministic tests.
pub trait WebSignInBackend: Send + Sync {
    fn available(&self) -> bool;
    fn open_blocking(&self, options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError>;
}

static CUSTOM_BACKEND: RwLock<Option<Arc<dyn WebSignInBackend>>> = RwLock::new(None);

/// Installs or clears a custom [`WebSignInBackend`] override.
pub fn set_backend(backend: Option<Arc<dyn WebSignInBackend>>) {
    if let Ok(mut guard) = CUSTOM_BACKEND.write() {
        *guard = backend;
    }
}

fn current_custom_backend() -> Option<Arc<dyn WebSignInBackend>> {
    CUSTOM_BACKEND.read().ok().and_then(|g| g.clone())
}

fn validate_options(options: &WebSignInOptions) -> Result<(), SignInError> {
    let trimmed = options.url.trim();
    if trimmed.is_empty() {
        return Err(SignInError::InvalidRequest("url must not be empty".into()));
    }
    let parsed = url::Url::parse(trimmed)
        .map_err(|err| SignInError::InvalidRequest(format!("invalid url `{trimmed}`: {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SignInError::InvalidRequest(format!(
            "unsupported url scheme `{}`",
            parsed.scheme()
        )));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(SignInError::InvalidRequest(
            "url must include a host".into(),
        ));
    }
    Ok(())
}

/// Returns the best available [`SignInMode`] for the current platform and runtime.
pub fn mode() -> SignInMode {
    if let Some(backend) = current_custom_backend() {
        return if backend.available() {
            SignInMode::NativePopup
        } else {
            SignInMode::ManualFallback
        };
    }
    #[cfg(target_os = "macos")]
    {
        macos::mode()
    }
    #[cfg(target_os = "ios")]
    {
        ios::mode()
    }
    #[cfg(target_os = "windows")]
    {
        windows::mode()
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        linux::mode()
    }
    #[cfg(target_os = "android")]
    {
        android::mode()
    }
    #[cfg(target_env = "ohos")]
    {
        ohos::mode()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos"
    )))]
    {
        fallback::mode()
    }
}

/// Returns whether a native popup WebView sign-in window is available in this runtime.
pub fn available() -> bool {
    if let Some(backend) = current_custom_backend() {
        return backend.available();
    }
    #[cfg(target_os = "macos")]
    {
        macos::available()
    }
    #[cfg(target_os = "ios")]
    {
        ios::available()
    }
    #[cfg(target_os = "windows")]
    {
        windows::available()
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        linux::available()
    }
    #[cfg(target_os = "android")]
    {
        android::available()
    }
    #[cfg(target_env = "ohos")]
    {
        ohos::available()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos"
    )))]
    {
        fallback::available()
    }
}

/// Opens a native popup WebView sign-in window and blocks the calling worker
/// thread until the requested cookies/URL match, the user closes the window,
/// or `timeout_ms` elapses.
pub fn open_blocking(options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    validate_options(&options)?;
    if let Some(backend) = current_custom_backend() {
        return backend.open_blocking(options);
    }
    #[cfg(target_os = "macos")]
    {
        macos::open_blocking(options)
    }
    #[cfg(target_os = "ios")]
    {
        ios::open_blocking(options)
    }
    #[cfg(target_os = "windows")]
    {
        windows::open_blocking(options)
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        linux::open_blocking(options)
    }
    #[cfg(target_os = "android")]
    {
        android::open_blocking(options)
    }
    #[cfg(target_env = "ohos")]
    {
        ohos::open_blocking(options)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos"
    )))]
    {
        fallback::open_blocking(options)
    }
}

/// Asynchronous wrapper around [`open_blocking`] suitable for NSH tasks and JS modules.
pub async fn open(options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    validate_options(&options)?;
    spawn_blocking_thread(move || open_blocking(options)).await
}

/// Fetches cookies for `target_url` (including `HttpOnly` cookies) from the
/// active native WebView cookie store when supported.
pub async fn fetch_session_cookies(
    target_url: &str,
    incognito: bool,
) -> Option<HashMap<String, String>> {
    let trimmed = target_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        macos::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(target_os = "ios")]
    {
        ios::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        linux::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(target_os = "android")]
    {
        android::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(target_env = "ohos")]
    {
        ohos::fetch_session_cookies(trimmed, incognito).await
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos"
    )))]
    {
        fallback::fetch_session_cookies(trimmed, incognito).await
    }
}

/// Clears native WebView session cookies for a clean re-sign-in when supported.
/// Returns `true` when the platform cookie store confirmed clearing.
pub async fn clear_session_cookies(incognito: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::clear_session_cookies(incognito).await
    }
    #[cfg(target_os = "ios")]
    {
        ios::clear_session_cookies(incognito).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::clear_session_cookies(incognito).await
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        linux::clear_session_cookies(incognito).await
    }
    #[cfg(target_os = "android")]
    {
        android::clear_session_cookies(incognito).await
    }
    #[cfg(target_env = "ohos")]
    {
        ohos::clear_session_cookies(incognito).await
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "android",
        target_env = "ohos"
    )))]
    {
        fallback::clear_session_cookies(incognito).await
    }
}

// -----------------------------------------------------------------------------
// NSH Native Module (`import { WebSignIn } from "nsh-web-signin"`)
// -----------------------------------------------------------------------------

/// NSH native module registration exposing [`WebSignIn`] to TSX/JS consumers.
#[native_module("nsh-web-signin")]
fn native(registry: &mut Registry) {
    registry.module::<WebSignIn>();
}

/// JS-facing options object for `WebSignIn.open(options)`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct JsWebSignInOptions {
    pub url: String,
    pub title: Option<String>,
    pub cookies_of: Option<String>,
    pub until: Option<Vec<String>>,
    pub match_url: Option<String>,
    pub user_agent: Option<String>,
    pub incognito: Option<bool>,
    pub timeout_ms: Option<u32>,
}

/// JS-facing outcome object resolved by `WebSignIn.open(options)`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct JsWebSignInOutcome {
    pub ok: bool,
    pub url: String,
    pub cookie: String,
    pub cookies: BTreeMap<String, String>,
    pub cancelled: bool,
}

/// Native module exported to TSX as `import { WebSignIn } from "nsh-web-signin"`.
#[module]
pub struct WebSignIn;

#[module]
impl WebSignIn {
    /// Returns whether native popup WebView sign-in is available on this platform.
    pub fn available() -> bool {
        crate::available()
    }

    /// Returns the camelCase [`SignInMode`] name (`"nativePopup"`,
    /// `"embeddedWithNativeCookies"`, or `"manualFallback"`).
    pub fn mode() -> String {
        crate::mode().as_str().to_owned()
    }

    /// Opens an isolated native WebView sign-in popup and resolves with captured cookies.
    pub async fn open(options: JsWebSignInOptions) -> Result<JsWebSignInOutcome, String> {
        let req = WebSignInOptions {
            url: options.url,
            title: options.title.unwrap_or_default(),
            cookies_of: options.cookies_of,
            until: options.until.unwrap_or_default(),
            match_url: options.match_url,
            user_agent: options.user_agent,
            incognito: options.incognito.unwrap_or(true),
            timeout_ms: u64::from(options.timeout_ms.unwrap_or(0)),
        };
        let outcome = crate::open(req).await.map_err(|err| err.to_string())?;
        Ok(JsWebSignInOutcome {
            ok: outcome.ok,
            url: outcome.url,
            cookie: outcome.cookie,
            cookies: outcome.cookies.into_iter().collect(),
            cancelled: outcome.cancelled,
        })
    }
}

struct ThreadSlot<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

pub(crate) struct ThreadFuture<T> {
    slot: Arc<Mutex<ThreadSlot<T>>>,
}

impl<T> Future for ThreadFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(val) = guard.value.take() {
            return Poll::Ready(val);
        }
        guard.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

pub(crate) fn spawn_blocking_thread<T, F>(f: F) -> ThreadFuture<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let slot = Arc::new(Mutex::new(ThreadSlot {
        value: None,
        waker: None,
    }));
    let slot_bg = Arc::clone(&slot);
    std::thread::spawn(move || {
        let out = f();
        let waker = {
            let mut guard = slot_bg.lock().unwrap_or_else(|p| p.into_inner());
            guard.value = Some(out);
            guard.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    });
    ThreadFuture { slot }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::task::Wake;

    struct ThreadWake {
        ready: Mutex<bool>,
        cv: Condvar,
    }

    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            let mut flag = self.ready.lock().unwrap_or_else(|p| p.into_inner());
            *flag = true;
            self.cv.notify_one();
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let wake = Arc::new(ThreadWake {
            ready: Mutex::new(false),
            cv: Condvar::new(),
        });
        let waker = Waker::from(Arc::clone(&wake));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => {
                    let mut flag = wake.ready.lock().unwrap_or_else(|p| p.into_inner());
                    while !*flag {
                        flag = wake.cv.wait(flag).unwrap_or_else(|p| p.into_inner());
                    }
                    *flag = false;
                }
            }
        }
    }

    static BACKEND_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct BackendTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl BackendTestGuard {
        fn acquire() -> Self {
            let lock = BACKEND_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            set_backend(None);
            Self { _lock: lock }
        }
    }

    impl Drop for BackendTestGuard {
        fn drop(&mut self) {
            set_backend(None);
        }
    }

    struct MockBackend;

    impl WebSignInBackend for MockBackend {
        fn available(&self) -> bool {
            true
        }

        fn open_blocking(
            &self,
            options: WebSignInOptions,
        ) -> Result<WebSignInOutcome, SignInError> {
            let mut cookies = HashMap::new();
            cookies.insert("SAPISID".into(), "mock-sap".into());
            Ok(WebSignInOutcome {
                ok: true,
                url: options.url,
                cookie: "SAPISID=mock-sap".into(),
                cookies,
                cancelled: false,
            })
        }
    }

    struct DisabledBackend;

    impl WebSignInBackend for DisabledBackend {
        fn available(&self) -> bool {
            false
        }

        fn open_blocking(
            &self,
            _options: WebSignInOptions,
        ) -> Result<WebSignInOutcome, SignInError> {
            Err(SignInError::Unavailable("disabled in test".into()))
        }
    }

    #[test]
    fn sign_in_mode_serializes_as_camel_case() {
        assert_eq!(SignInMode::NativePopup.as_str(), "nativePopup");
        assert_eq!(
            SignInMode::EmbeddedWithNativeCookies.as_str(),
            "embeddedWithNativeCookies"
        );
        assert_eq!(SignInMode::ManualFallback.as_str(), "manualFallback");

        assert_eq!(
            serde_json::to_string(&SignInMode::NativePopup).unwrap(),
            "\"nativePopup\""
        );
        assert_eq!(
            serde_json::to_string(&SignInMode::EmbeddedWithNativeCookies).unwrap(),
            "\"embeddedWithNativeCookies\""
        );
        assert_eq!(
            serde_json::to_string(&SignInMode::ManualFallback).unwrap(),
            "\"manualFallback\""
        );
        assert_eq!(
            serde_json::from_str::<SignInMode>("\"embeddedWithNativeCookies\"").unwrap(),
            SignInMode::EmbeddedWithNativeCookies
        );
    }

    #[test]
    fn validates_request_urls() {
        let _guard = BackendTestGuard::acquire();
        let err = open_blocking(WebSignInOptions {
            url: "javascript:alert(1)".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, SignInError::InvalidRequest(_)));

        let err2 = open_blocking(WebSignInOptions::default()).unwrap_err();
        assert!(matches!(err2, SignInError::InvalidRequest(_)));
    }

    #[test]
    fn headless_open_blocking_returns_unavailable_without_deadlocking() {
        let _guard = BackendTestGuard::acquire();
        assert!(!available());
        assert_eq!(mode(), SignInMode::ManualFallback);
        assert_eq!(WebSignIn::mode(), "manualFallback");
        let start = std::time::Instant::now();
        let res = open_blocking(WebSignInOptions {
            url: "https://music.youtube.com/".into(),
            title: "YouTube Music".into(),
            cookies_of: Some("https://music.youtube.com".into()),
            until: vec!["SAPISID".into()],
            incognito: true,
            ..Default::default()
        });
        assert!(
            matches!(res, Err(SignInError::Unavailable(_))),
            "expected Unavailable in headless cargo test, got {res:?}"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "headless open_blocking must return immediately without hanging"
        );
    }

    #[test]
    fn headless_cookie_fetch_and_clear_return_promptly() {
        let _guard = BackendTestGuard::acquire();
        let start = std::time::Instant::now();
        assert!(block_on(fetch_session_cookies("https://music.youtube.com/", false)).is_none());
        assert!(block_on(fetch_session_cookies("https://music.youtube.com/", true)).is_none());
        assert!(block_on(fetch_session_cookies("   ", false)).is_none());
        assert!(!block_on(clear_session_cookies(false)));
        assert!(!block_on(clear_session_cookies(true)));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "headless cookie helpers must return immediately without hanging"
        );
    }

    #[test]
    fn custom_backend_override_works() {
        let _guard = BackendTestGuard::acquire();
        set_backend(Some(Arc::new(MockBackend)));
        assert!(available());
        assert_eq!(mode(), SignInMode::NativePopup);
        assert_eq!(WebSignIn::mode(), "nativePopup");
        let outcome = open_blocking(WebSignInOptions {
            url: "https://music.youtube.com/".into(),
            ..Default::default()
        })
        .expect("mock backend succeeds");
        assert!(outcome.ok);
        assert_eq!(
            outcome.cookies.get("SAPISID").map(String::as_str),
            Some("mock-sap")
        );

        set_backend(Some(Arc::new(DisabledBackend)));
        assert!(!available());
        assert_eq!(mode(), SignInMode::ManualFallback);
        assert_eq!(WebSignIn::mode(), "manualFallback");
    }
}
