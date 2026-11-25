//! Thin re-exports of `nsh-web-signin` cookie helpers and session cookie bridge.

pub use nsh_web_signin::{
    SignInMode, clear_session_cookies, fetch_session_cookies, format_cookie_header, mode,
    parse_cookie_header, satisfies_until_cookies, url_matches_cookies_of,
};
