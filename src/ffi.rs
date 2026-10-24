//! C ABI surface — everything declared in include/processkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `pk_free_string`.
//! - Failures set a thread-local last-error (code + message)
//!   and return the documented sentinel (a PK_* code, or null
//!   handle).
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - A process handle IS thread-safe: supervision state is
//!   serialized internally, so any thread may poll/kill/wait
//!   through it.

// The exported functions ARE the raw-pointer C ABI declared in
// include/processkit.h; pointer validity is the documented
// contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;
use std::sync::Mutex;

use crate::{parse_config, PkResult, ProcResult, Running, ERR_INTERNAL, ERR_INVALID_ARG, PK_OK};

/// The opaque process handle (PkProcess in the header).
pub struct PkProcess {
    inner: Mutex<Running>,
}

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(PK_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> PkResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

/// Write a caller-owned string result. Returns PK_OK, or an
/// error code when `out` is null or the string cannot be
/// represented (interior NUL — impossible for our own JSON,
/// which escapes control characters, but the guard is cheap).
fn out_string(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    match CString::new(text) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            clear_error();
            PK_OK as c_int
        }
        Err(_) => fail(ERR_INTERNAL, "result string contained a NUL byte"),
    }
}

fn handle<'a>(ptr: *mut PkProcess) -> PkResult<&'a PkProcess> {
    if ptr.is_null() {
        return Err((
            ERR_INVALID_ARG,
            "process handle must not be null".to_string(),
        ));
    }
    Ok(unsafe { &*ptr })
}

fn with_running<T, F: FnOnce(&mut Running) -> PkResult<T>>(
    ptr: *mut PkProcess,
    f: F,
) -> PkResult<T> {
    let proc = handle(ptr)?;
    let mut running = proc
        .inner
        .lock()
        .map_err(|_| (ERR_INTERNAL, "process handle mutex poisoned".to_string()))?;
    f(&mut running)
}

#[no_mangle]
pub extern "C" fn pk_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn pk_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn pk_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet. Documented
/// in the header.
#[no_mangle]
pub extern "C" fn pk_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

fn run_impl(config_json: *const c_char, out: *mut *mut c_char) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_result_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = (|| -> PkResult<(ProcResult, Option<(i32, String)>)> {
        let text = cstr(config_json, "config_json")?;
        let cfg = parse_config(text)?;
        Ok(crate::run_config(cfg))
    })();
    match inner {
        Ok((result, spawn_err)) => {
            let code = out_string(out, result.to_json());
            if code == PK_OK as c_int {
                // A spawn failure is a result (state
                // "spawn_failed"), not a call failure — the
                // call still returns PK_OK — but the reason
                // lands in the last-error (set after out_string,
                // which clears it) so callers who check learn
                // why nothing ran.
                if let Some((ecode, msg)) = spawn_err {
                    set_error(ecode, &msg);
                }
            }
            code
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn pk_run(config_json: *const c_char, out_result_json: *mut *mut c_char) -> c_int {
    match guard(|| run_impl(config_json, out_result_json)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

fn start_impl(config_json: *const c_char, out: *mut *mut PkProcess) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_handle must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = (|| -> PkResult<PkProcess> {
        let text = cstr(config_json, "config_json")?;
        let cfg = parse_config(text)?;
        let running = Running::start(cfg)?;
        Ok(PkProcess {
            inner: Mutex::new(running),
        })
    })();
    match inner {
        Ok(proc) => {
            unsafe { *out = Box::into_raw(Box::new(proc)) };
            clear_error();
            PK_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn pk_start(config_json: *const c_char, out_handle: *mut *mut PkProcess) -> c_int {
    match guard(|| start_impl(config_json, out_handle)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

fn poll_impl(handle_ptr: *mut PkProcess, out: *mut *mut c_char) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = with_running(handle_ptr, |r| r.poll());
    match inner {
        Ok(info) => out_string(out, info.to_json()),
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn pk_poll(handle: *mut PkProcess, out_json: *mut *mut c_char) -> c_int {
    match guard(|| poll_impl(handle, out_json)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

fn wait_impl(handle_ptr: *mut PkProcess, wait_ms: u32, out: *mut *mut c_char) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_result_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = with_running(handle_ptr, |r| r.wait(wait_ms));
    match inner {
        Ok(result) => out_string(out, result.to_json()),
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn pk_wait(
    handle: *mut PkProcess,
    wait_ms: u32,
    out_result_json: *mut *mut c_char,
) -> c_int {
    match guard(|| wait_impl(handle, wait_ms, out_result_json)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn pk_kill(handle: *mut PkProcess, kill_tree: c_int) -> c_int {
    match guard(|| {
        let inner = with_running(handle, |r| r.kill(kill_tree != 0));
        match inner {
            Ok(()) => {
                clear_error();
                PK_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Close the handle and free it. If the process is still
/// running and tree-kill was configured, it is killed first —
/// on Windows, closing the Job Object does this at the OS
/// level (KILL_ON_JOB_CLOSE), so the tree cannot outlive the
/// handle even if this process dies mid-close. Closing a null
/// handle is a no-op. Anything else through the handle after
/// this is a contract violation (use-after-close).
#[no_mangle]
pub extern "C" fn pk_close(handle: *mut PkProcess) {
    if !handle.is_null() {
        let _ = guard(|| unsafe {
            let boxed = Box::from_raw(handle);
            if let Ok(running) = boxed.inner.into_inner() {
                running.close();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take_string(p: *mut c_char) -> String {
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        pk_free_string(p);
        s
    }

    fn echo_config_json() -> CString {
        if cfg!(windows) {
            cs(r#"{"exe":"cmd","args":["/C","echo","hello"]}"#)
        } else {
            cs(r#"{"exe":"/bin/echo","args":["hello"]}"#)
        }
    }

    #[test]
    fn ffi_run_echo() {
        let cfg = echo_config_json();
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(pk_run(cfg.as_ptr(), &mut out), PK_OK as c_int);
        let json = take_string(out);
        assert!(json.contains("\"state\":\"exited\""), "{json}");
        assert!(json.contains("\"exit_code\":0"), "{json}");
        assert!(json.contains("\"stdout\":\"hello"), "{json}");
    }

    #[test]
    fn ffi_run_config_errors() {
        let mut out: *mut c_char = std::ptr::null_mut();
        // Unknown key -> PK_ERR_PARSE, no string.
        let bad = cs(r#"{"exe":"x","bogus":1}"#);
        assert_eq!(pk_run(bad.as_ptr(), &mut out), crate::ERR_PARSE as c_int);
        assert!(out.is_null());
        assert_eq!(pk_last_error_code(), crate::ERR_PARSE as c_int);
        // Missing exe -> PK_ERR_INVALID_ARG.
        let empty = cs(r#"{}"#);
        assert_eq!(pk_run(empty.as_ptr(), &mut out), ERR_INVALID_ARG as c_int);
        // Null config -> PK_ERR_INVALID_ARG.
        assert_eq!(pk_run(std::ptr::null(), &mut out), ERR_INVALID_ARG as c_int);
    }

    #[test]
    fn ffi_run_spawn_failed_is_a_result() {
        let cfg = cs(r#"{"exe":"/nonexistent/processkit-nope-12345"}"#);
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(pk_run(cfg.as_ptr(), &mut out), PK_OK as c_int);
        let json = take_string(out);
        assert!(json.contains("\"state\":\"spawn_failed\""), "{json}");
        // And the reason is in the last-error for callers who look.
        assert_ne!(pk_last_error_code(), PK_OK as c_int);
    }

    #[test]
    fn ffi_start_poll_wait_close() {
        let cfg = echo_config_json();
        let mut handle: *mut PkProcess = std::ptr::null_mut();
        assert_eq!(pk_start(cfg.as_ptr(), &mut handle), PK_OK as c_int);
        assert!(!handle.is_null());

        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            pk_wait(handle, crate::WAIT_INFINITE, &mut out),
            PK_OK as c_int
        );
        let json = take_string(out);
        assert!(json.contains("\"state\":\"exited\""), "{json}");
        assert!(json.contains("hello"), "{json}");

        let mut poll_out: *mut c_char = std::ptr::null_mut();
        assert_eq!(pk_poll(handle, &mut poll_out), PK_OK as c_int);
        let text = take_string(poll_out);
        assert!(text.contains("\"state\":\"exited\""), "{text}");
        assert!(text.contains("\"exit_code\":0"), "{text}");
        pk_close(handle);
        pk_close(std::ptr::null_mut()); // no-op, must not crash
    }

    #[test]
    fn ffi_start_spawn_failure_is_an_error() {
        let cfg = cs(r#"{"exe":"/nonexistent/processkit-nope-12345"}"#);
        let mut handle: *mut PkProcess = std::ptr::null_mut();
        let code = pk_start(cfg.as_ptr(), &mut handle);
        assert_ne!(code, PK_OK as c_int);
        assert!(handle.is_null());
    }

    #[test]
    fn ffi_null_handle_errors() {
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            pk_poll(std::ptr::null_mut(), &mut out),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(pk_kill(std::ptr::null_mut(), 1), ERR_INVALID_ARG as c_int);
    }
}
