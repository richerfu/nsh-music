//! Android JNI cookie-store backend using `android.webkit.CookieManager`.
//!
//! Pairs with NSH's embedded Android `web_view()` sign-in sheet to read and
//! clear ALL cookies (including `HttpOnly` cookies) without modifying
//! `nsh-platform-android`. Discovers the process `JavaVM` automatically via
//! `JNI_GetCreatedJavaVMs`, or accepts explicit registration via
//! [`register_java_vm`].

use std::collections::HashMap;
use std::sync::OnceLock;

use jni::JavaVM;
use jni::objects::{JObject, JString, JValue};
use jni::sys::{JNI_OK, jint, jsize};

use crate::{SignInError, SignInMode, WebSignInOptions, WebSignInOutcome, cookies};

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

/// Explicitly registers the host Android `JavaVM` (for example from an app's `JNI_OnLoad`).
pub fn register_java_vm(vm: JavaVM) {
    let _ = JAVA_VM.set(vm);
}

/// Explicitly registers a raw `JavaVM` pointer from C/JNI glue.
///
/// # Safety
/// `vm` must be a valid non-null `JavaVM*` that remains alive for the process lifetime.
pub unsafe fn register_java_vm_raw(vm: *mut jni::sys::JavaVM) -> bool {
    if vm.is_null() {
        return false;
    }
    match unsafe { JavaVM::from_raw(vm) } {
        Ok(wrapped) => {
            let _ = JAVA_VM.set(wrapped);
            true
        }
        Err(_) => false,
    }
}

fn discover_java_vm() -> Option<&'static JavaVM> {
    if let Some(vm) = JAVA_VM.get() {
        return Some(vm);
    }

    type GetCreatedVmsFn =
        unsafe extern "C" fn(*mut *mut jni::sys::JavaVM, jsize, *mut jsize) -> jint;

    let mut sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"JNI_GetCreatedJavaVMs".as_ptr()) };
    if sym.is_null() {
        // Fallback: ART exports `JNI_GetCreatedJavaVMs` from `libart.so`.
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

pub(crate) fn mode() -> SignInMode {
    SignInMode::EmbeddedWithNativeCookies
}

pub(crate) fn available() -> bool {
    false
}

pub(crate) fn open_blocking(_options: WebSignInOptions) -> Result<WebSignInOutcome, SignInError> {
    Err(SignInError::Unavailable(
        "Android uses embedded WebView sheet with android.webkit.CookieManager".into(),
    ))
}

pub(crate) async fn fetch_session_cookies(
    target_url: &str,
    _incognito: bool,
) -> Option<HashMap<String, String>> {
    let target = target_url.trim().to_owned();
    if target.is_empty() {
        return None;
    }
    crate::spawn_blocking_thread(move || fetch_cookies_jni(&target)).await
}

pub(crate) async fn clear_session_cookies(_incognito: bool) -> bool {
    crate::spawn_blocking_thread(clear_cookies_jni).await
}

fn fetch_cookies_jni(target_url: &str) -> Option<HashMap<String, String>> {
    let vm = discover_java_vm()?;
    let mut env = vm.attach_current_thread().ok()?;

    let mgr_val = env
        .call_static_method(
            "android/webkit/CookieManager",
            "getInstance",
            "()Landroid/webkit/CookieManager;",
            &[],
        )
        .ok()?;
    let mgr_obj = mgr_val.l().ok()?;
    if mgr_obj.is_null() {
        clear_pending_exception(&mut env);
        return None;
    }

    let url_jstr = env.new_string(target_url).ok()?;
    let res = env.call_method(
        &mgr_obj,
        "getCookie",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[JValue::Object(&url_jstr)],
    );
    if clear_pending_exception(&mut env) {
        return None;
    }
    let cookie_obj = res.ok()?.l().ok()?;
    if cookie_obj.is_null() {
        return Some(HashMap::new());
    }
    let cookie_jstr = JString::from(cookie_obj);
    let raw: String = env.get_string(&cookie_jstr).ok()?.into();
    Some(cookies::parse_cookie_header(&raw))
}

fn clear_cookies_jni() -> bool {
    let Some(vm) = discover_java_vm() else {
        return false;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return false;
    };

    let Ok(mgr_val) = env.call_static_method(
        "android/webkit/CookieManager",
        "getInstance",
        "()Landroid/webkit/CookieManager;",
        &[],
    ) else {
        clear_pending_exception(&mut env);
        return false;
    };
    let Ok(mgr_obj) = mgr_val.l() else {
        clear_pending_exception(&mut env);
        return false;
    };
    if mgr_obj.is_null() {
        clear_pending_exception(&mut env);
        return false;
    }

    let null_cb = JObject::null();
    let rm_res = env.call_method(
        &mgr_obj,
        "removeAllCookies",
        "(Landroid/webkit/ValueCallback;)V",
        &[JValue::Object(&null_cb)],
    );
    if clear_pending_exception(&mut env) || rm_res.is_err() {
        return false;
    }

    let flush_res = env.call_method(&mgr_obj, "flush", "()V", &[]);
    !(clear_pending_exception(&mut env) || flush_res.is_err())
}

fn clear_pending_exception(env: &mut jni::JNIEnv<'_>) -> bool {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
        true
    } else {
        false
    }
}
