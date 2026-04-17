//! C-compatible FFI for embedding NeuroPack in C/C++ game engines.
//!
//! Build with `crate-type = ["cdylib"]` to produce a shared library.
//! The generated header (neuropack.h) should declare these symbols.
//!
//! ## Thread safety
//! `NeuropackReader` is NOT thread-safe.  Create one per thread, or protect
//! with an external mutex.  `neuropack_read_asset` takes a `*mut` handle
//! because file seeks mutate state internally.
//!
//! ## Error handling
//! Functions that can fail return a null pointer or -1.
//! Call `neuropack_last_error()` immediately after a failure to retrieve
//! a null-terminated UTF-8 error string.  The string is valid until the
//! next FFI call on any thread (stored in thread-local storage).

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;

use crate::decompression::PackageReader;

// ── Thread-local last-error storage ───────────────────────────────────────

thread_local! {
    static LAST_ERROR: std::cell::RefCell<CString> =
        std::cell::RefCell::new(CString::new("").unwrap());
}

fn set_error(msg: &str) {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(msg).unwrap_or_else(|_| CString::new("(error)").unwrap());
    });
}

/// Returns the last error from a failed NeuroPack FFI call as a
/// null-terminated C string.  Valid until the next FFI call.
#[no_mangle]
pub extern "C" fn neuropack_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

// ── Open / Close ───────────────────────────────────────────────────────────

/// Open a `.neuropack` package file.
///
/// Returns a non-null opaque handle on success, or NULL on failure.
/// The caller must release it with `neuropack_close`.
#[no_mangle]
pub extern "C" fn neuropack_open(path: *const c_char) -> *mut c_void {
    if path.is_null() {
        set_error("path is null");
        return std::ptr::null_mut();
    }
    let path_str = unsafe {
        match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => { set_error("path is not valid UTF-8"); return std::ptr::null_mut(); }
        }
    };
    match PackageReader::open(PathBuf::from(path_str)) {
        Ok(reader) => Box::into_raw(Box::new(reader)) as *mut c_void,
        Err(e) => { set_error(&e.to_string()); std::ptr::null_mut() }
    }
}

/// Close a handle returned by `neuropack_open`.  Passing NULL is a no-op.
#[no_mangle]
pub extern "C" fn neuropack_close(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle as *mut PackageReader)); }
    }
}

// ── Read asset ─────────────────────────────────────────────────────────────

/// Decompress an asset into a freshly allocated buffer.
///
/// # Parameters
/// - `handle`    Non-null reader handle from `neuropack_open`.
/// - `rel_path`  Null-terminated relative path, e.g. `"data/textures/hero.dds"`.
/// - `out_data`  Receives a pointer to the allocated buffer on success.
///               Free with `neuropack_free_asset`.
/// - `out_len`   Receives the byte length of the buffer.
///
/// # Returns
/// 0 on success, -1 on failure (call `neuropack_last_error` for details).
#[no_mangle]
pub extern "C" fn neuropack_read_asset(
    handle: *mut c_void,
    rel_path: *const c_char,
    out_data: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if handle.is_null() || rel_path.is_null() || out_data.is_null() || out_len.is_null() {
        set_error("null argument");
        return -1;
    }

    let reader = unsafe { &*(handle as *const PackageReader) };
    let rel = unsafe {
        match CStr::from_ptr(rel_path).to_str() {
            Ok(s) => s,
            Err(_) => { set_error("rel_path is not valid UTF-8"); return -1; }
        }
    };

    let path = std::path::Path::new(rel);
    let entry = match reader.index.iter().find(|e| e.relative_path == path) {
        Some(e) => e,
        None => {
            set_error(&format!("asset not found: {}", rel));
            return -1;
        }
    };

    // Resolve duplicate → original.
    let entry = if let Some(orig) = &entry.duplicate_of {
        match reader.index.iter().find(|e| &e.relative_path == orig) {
            Some(e) => e,
            None => { set_error("duplicate original missing"); return -1; }
        }
    } else { entry };

    match reader.extract_asset(entry) {
        Ok(data) => {
            let len = data.len();
            // Move data into a Box<[u8]>, then leak the raw pointer.
            let ptr = Box::into_raw(data.into_boxed_slice()) as *mut u8;
            unsafe {
                *out_data = ptr;
                *out_len  = len;
            }
            0
        }
        Err(e) => { set_error(&e.to_string()); -1 }
    }
}

/// Free a buffer allocated by `neuropack_read_asset`.
///
/// # Safety
/// `data` must be a pointer previously returned in `out_data` by
/// `neuropack_read_asset`, and `len` must be the corresponding `out_len`.
#[no_mangle]
pub unsafe extern "C" fn neuropack_free_asset(data: *mut u8, len: usize) {
    if !data.is_null() {
        drop(Box::from_raw(std::slice::from_raw_parts_mut(data, len)));
    }
}

// ── Package metadata ───────────────────────────────────────────────────────

/// Return the number of entries in the package index.
#[no_mangle]
pub extern "C" fn neuropack_entry_count(handle: *const c_void) -> usize {
    if handle.is_null() { return 0; }
    let reader = unsafe { &*(handle as *const PackageReader) };
    reader.index.len()
}

/// Copy the relative path of entry `i` into `buf` (including null terminator).
///
/// Returns the number of bytes written (including `\0`), or 0 if `i` is
/// out of range or `buf_len` is too small.
#[no_mangle]
pub extern "C" fn neuropack_entry_path(
    handle: *const c_void,
    i: usize,
    buf: *mut c_char,
    buf_len: usize,
) -> usize {
    if handle.is_null() || buf.is_null() { return 0; }
    let reader = unsafe { &*(handle as *const PackageReader) };
    let entry = match reader.index.get(i) {
        Some(e) => e,
        None => return 0,
    };
    let s = entry.relative_path.display().to_string();
    let bytes = s.as_bytes();
    let needed = bytes.len() + 1;
    if buf_len < needed { return 0; }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
        *buf.add(bytes.len()) = 0;
    }
    needed
}

/// Return the uncompressed byte length of entry `i`, or 0 for out-of-range.
#[no_mangle]
pub extern "C" fn neuropack_entry_size(handle: *const c_void, i: usize) -> u64 {
    if handle.is_null() { return 0; }
    let reader = unsafe { &*(handle as *const PackageReader) };
    reader.index.get(i).map(|e| e.uncompressed_length).unwrap_or(0)
}
