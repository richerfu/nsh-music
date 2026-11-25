//! HarmonyOS / OpenHarmony WebView cookie-store backend using unmodified
//! `nsh-platform-harmonyos` extension points (`run_on_ui_thread` +
//! `ui.load_module("@ohos.web.webview")`).

use std::collections::HashMap;

use nsh_platform_harmonyos::napi_ohos::bindgen_prelude::FnArgs;
use nsh_platform_harmonyos::napi_ohos::module::Module;

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome, cookies};

pub(crate) fn mode() -> SignInMode {
    SignInMode::EmbeddedWithNativeCookies
}

pub(crate) fn available() -> bool {
    false
}

pub(crate) fn open_blocking(_options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    Err(SignInError::Unavailable(
        "HarmonyOS uses embedded incognito sheet with WebCookieManager".into(),
    ))
}

pub(crate) async fn fetch_session_cookies(
    target_url: &str,
    incognito: bool,
) -> Option<HashMap<String, String>> {
    let target = target_url.trim().to_owned();
    if target.is_empty() {
        return None;
    }

    let result = nsh_platform_harmonyos::run_on_ui_thread(move |ui| {
        let webview_mod = ui.load_module("@ohos.web.webview")?;
        let cookie_mgr: Module = webview_mod.get("WebCookieManager")?;
        match cookie_mgr.call::<_, _, String>(
            "fetchCookieSync",
            FnArgs::from((target.as_str(), incognito)),
        ) {
            Ok(raw) => Ok(raw),
            Err(err) if err.to_string().contains("17100002") => Ok(String::new()),
            Err(err) => Err(err),
        }
    })
    .await;

    match result {
        Ok(raw) => Some(cookies::parse_cookie_header(&raw)),
        Err(err) if err.to_string().contains("17100002") => Some(HashMap::new()),
        Err(err) => {
            log::debug!("OHOS WebCookieManager.fetchCookieSync failed: {err}");
            None
        }
    }
}

pub(crate) async fn clear_session_cookies(incognito: bool) -> bool {
    let result = nsh_platform_harmonyos::run_on_ui_thread(move |ui| {
        let webview_mod = ui.load_module("@ohos.web.webview")?;
        let cookie_mgr: Module = webview_mod.get("WebCookieManager")?;
        cookie_mgr.call::<_, _, ()>("clearAllCookiesSync", FnArgs::from((incognito,)))
    })
    .await;

    match result {
        Ok(()) => true,
        Err(err) => {
            log::debug!("OHOS WebCookieManager.clearAllCookiesSync failed: {err}");
            false
        }
    }
}
