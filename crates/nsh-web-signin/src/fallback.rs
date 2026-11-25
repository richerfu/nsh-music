//! Fallback WebView sign-in backend for platforms without a dedicated native
//! popup or cookie-store bridge.

use std::collections::HashMap;

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome};

pub(crate) fn mode() -> SignInMode {
    SignInMode::ManualFallback
}

pub(crate) fn available() -> bool {
    false
}

pub(crate) fn open_blocking(_options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    Err(SignInError::Unavailable(
        "native popup WebView sign-in is unavailable on this platform".into(),
    ))
}

pub(crate) async fn fetch_session_cookies(
    _target_url: &str,
    _incognito: bool,
) -> Option<HashMap<String, String>> {
    None
}

pub(crate) async fn clear_session_cookies(_incognito: bool) -> bool {
    false
}
