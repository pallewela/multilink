//! C interface for embedding multilink (used by the macOS menu bar app and its privileged
//! daemon; header in macos/multilink.h). Strings are UTF-8 and NUL-terminated. A Rust panic
//! never crosses into the caller: it becomes an error message or exit code 101.
use crate::Running;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn to_c(s: &str) -> *mut c_char {
    CString::new(s.replace('\0', " ")).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Write `s` to `*out` when `out` is not NULL.
unsafe fn put(out: *mut *mut c_char, s: &str) {
    if !out.is_null() {
        unsafe { *out = to_c(s) };
    }
}

/// Check a config file. Returns NULL when it is valid, otherwise a message that the caller
/// must release with `multilink_free_string`.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_check_config(path: *const c_char) -> *mut c_char {
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned();
    match catch_unwind(|| crate::check_config(&path)).unwrap_or_else(|_| Err("internal error".into())) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => to_c(&e),
    }
}

/// Start balancing on a thread of this process (needs root). `config_toml` is the config's
/// text; `status` (may be NULL) overrides its status address. Returns NULL on failure, with
/// the reason in `*error` (free it with `multilink_free_string`).
///
/// # Safety
/// `config_toml` must be a valid string, `status` NULL or a valid string, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_start(config_toml: *const c_char, status: *const c_char, error: *mut *mut c_char) -> *mut Running {
    let toml = unsafe { CStr::from_ptr(config_toml) }.to_string_lossy().into_owned();
    let status = (!status.is_null()).then(|| unsafe { CStr::from_ptr(status) }.to_string_lossy().into_owned());
    match catch_unwind(|| crate::start(&toml, status.as_deref())).unwrap_or_else(|_| Err("internal error".into())) {
        Ok(r) => Box::into_raw(Box::new(r)),
        Err(e) => {
            unsafe { put(error, &e) };
            std::ptr::null_mut()
        }
    }
}

/// Apply a new config to the running balancer. Returns 0 when everything applied, 1 when
/// policy, weights and pins applied but `*message` lists settings that need a restart
/// (comma-separated), -1 when the config is invalid (`*message` says why; nothing changed).
///
/// # Safety
/// `r` must come from `multilink_start` and not be released; `message` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_reload(r: *const Running, config_toml: *const c_char, message: *mut *mut c_char) -> c_int {
    let (r, toml) = (unsafe { &*r }, unsafe { CStr::from_ptr(config_toml) }.to_string_lossy().into_owned());
    match catch_unwind(AssertUnwindSafe(|| r.reload(&toml))).unwrap_or_else(|_| Err("internal error".into())) {
        Ok(restart) if restart.is_empty() => 0,
        Ok(restart) => {
            unsafe { put(message, &restart.join(", ")) };
            1
        }
        Err(e) => {
            unsafe { put(message, &e) };
            -1
        }
    }
}

/// Ask the balancer to stop; returns at once.
///
/// # Safety
/// `r` must come from `multilink_start` and not be released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_stop(r: *const Running) {
    unsafe { &*r }.stop();
}

/// Block until the balancer has stopped and removed its routes; returns its exit code
/// (0 ok, 1 runtime failure, 101 panic). May be called more than once, from any thread.
///
/// # Safety
/// `r` must come from `multilink_start` and not be released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_wait(r: *const Running) -> c_int {
    let r = unsafe { &*r };
    catch_unwind(AssertUnwindSafe(|| r.wait())).unwrap_or(101)
}

/// Free a balancer after `multilink_wait` has returned. NULL is ignored.
///
/// # Safety
/// `r` must be NULL or come from `multilink_start`, released once, with no call still using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_release(r: *mut Running) {
    if !r.is_null() {
        drop(unsafe { Box::from_raw(r) });
    }
}

/// Release a string returned by multilink. NULL is ignored.
///
/// # Safety
/// `s` must be NULL or a pointer from this library, released only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn multilink_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_config_over_ffi() {
        let dir = std::env::temp_dir().join(format!("multilink-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good.toml"), dir.join("bad.toml"));
        std::fs::write(&good, "policy = \"round-robin\"\n").unwrap();
        std::fs::write(&bad, "polcy = \"x\"\n").unwrap();
        let c = |p: &std::path::Path| CString::new(p.to_str().unwrap()).unwrap();
        unsafe {
            assert!(multilink_check_config(c(&good).as_ptr()).is_null());
            let msg = multilink_check_config(c(&bad).as_ptr());
            assert!(CStr::from_ptr(msg).to_str().unwrap().contains("unknown field `polcy`"));
            multilink_free_string(msg);
            multilink_free_string(std::ptr::null_mut());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn start_reports_config_errors_without_starting() {
        let mut err = std::ptr::null_mut();
        let r = unsafe { multilink_start(c"polcy = 1".as_ptr(), std::ptr::null(), &mut err) };
        assert!(r.is_null());
        assert!(unsafe { CStr::from_ptr(err) }.to_str().unwrap().contains("unknown field `polcy`"));
        unsafe { multilink_free_string(err) };
        let r = unsafe { multilink_start(c"".as_ptr(), c"nowhere".as_ptr(), &mut err) };
        assert!(r.is_null());
        assert!(unsafe { CStr::from_ptr(err) }.to_str().unwrap().contains("bad status address"));
        unsafe { multilink_free_string(err) };
    }
}
