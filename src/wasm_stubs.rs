//! Minimal C/C++ runtime shims for linking Emscripten-built ymfm into
//! `wasm32-unknown-unknown` (wasm-bindgen / eframe), without full wasi/libc++.
//!
//! The chip objects are compiled with `emcc -fno-exceptions -fno-rtti`. They
//! still need `operator new/delete`, a few stdio helpers, and `getenv`. Z80 IRQ
//! hooks (`neo_z80_*`) live in `neogeo/z80.rs`.

use std::alloc::{alloc, dealloc, Layout};
use std::os::raw::{c_char, c_int, c_void};

#[repr(C)]
struct AllocHdr {
    size: usize,
}

const HDR_SIZE: usize = std::mem::size_of::<AllocHdr>();

// ── C++ operator new / delete (Itanium ABI) ──────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn _Znwm(size: usize) -> *mut c_void {
    cpp_new(size)
}

#[no_mangle]
pub unsafe extern "C" fn _Znam(size: usize) -> *mut c_void {
    cpp_new(size)
}

#[no_mangle]
pub unsafe extern "C" fn _ZdlPv(ptr: *mut c_void) {
    cpp_delete(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn _ZdlPvm(ptr: *mut c_void, _size: usize) {
    cpp_delete(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn _ZdaPv(ptr: *mut c_void) {
    cpp_delete(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn _ZdaPvm(ptr: *mut c_void, _size: usize) {
    cpp_delete(ptr);
}

unsafe fn cpp_new(size: usize) -> *mut c_void {
    let size = size.max(1);
    let total = size.saturating_add(HDR_SIZE);
    let layout = Layout::from_size_align_unchecked(total, 16);
    let base = alloc(layout);
    if base.is_null() {
        abort();
    }
    let hdr = base as *mut AllocHdr;
    (*hdr).size = size;
    base.add(HDR_SIZE) as *mut c_void
}

unsafe fn cpp_delete(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let base = (ptr as *mut u8).sub(HDR_SIZE);
    let hdr = base as *mut AllocHdr;
    let size = (*hdr).size.max(1);
    let total = size.saturating_add(HDR_SIZE);
    let layout = Layout::from_size_align_unchecked(total, 16);
    dealloc(base, layout);
}

/// C `abort`. Must be a real exported symbol: older emcc (e.g. CI's 3.1.74)
/// emits direct calls to `abort` from assert/error paths, and Rust's stricter
/// wasm `rust-lld` treats a leftover undefined `env.abort` import as a fatal
/// link error (newer emcc inlines it, which is why local builds slipped by).
#[no_mangle]
pub extern "C" fn abort() -> ! {
    core::arch::wasm32::unreachable();
}

// ── libc helpers referenced by emcc/ymfm objects ─────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // Same headered allocator as operator new so free() can recover size.
    cpp_new(size)
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    cpp_delete(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn calloc(n: usize, size: usize) -> *mut c_void {
    let total = n.saturating_mul(size);
    let p = cpp_new(total);
    if !p.is_null() && total > 0 {
        std::ptr::write_bytes(p as *mut u8, 0, total);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: usize) -> *mut c_void {
    if ptr.is_null() {
        return cpp_new(new_size);
    }
    if new_size == 0 {
        cpp_delete(ptr);
        return std::ptr::null_mut();
    }
    let base = (ptr as *mut u8).sub(HDR_SIZE);
    let hdr = base as *mut AllocHdr;
    let old_size = (*hdr).size;
    let new_ptr = cpp_new(new_size);
    if new_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let copy = old_size.min(new_size.max(1));
    std::ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, copy);
    cpp_delete(ptr);
    new_ptr
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: c_int, n: usize) -> *mut u8 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    let needle = c as u8;
    for i in 0..n {
        if *s.add(i) == needle {
            return s.add(i) as *mut u8;
        }
    }
    std::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> c_int {
    if a.is_null() || b.is_null() {
        return 0;
    }
    for i in 0..n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as c_int - y as c_int;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n > 0 && !dest.is_null() && !src.is_null() {
        std::ptr::copy_nonoverlapping(src, dest, n);
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n > 0 && !dest.is_null() && !src.is_null() {
        std::ptr::copy(src, dest, n);
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: c_int, n: usize) -> *mut u8 {
    if n > 0 && !s.is_null() {
        std::ptr::write_bytes(s, c as u8, n);
    }
    s
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// C `snprintf`. ymfm references it only in cold register/name formatting that
/// never runs on web. Emscripten lowers variadics to their fixed wasm params
/// (varargs travel via a side buffer), so `(buf, size, fmt)` is link-compatible.
/// Write an empty string and report 0 written.
#[no_mangle]
pub unsafe extern "C" fn snprintf(buf: *mut c_char, size: usize, _fmt: *const c_char) -> c_int {
    if !buf.is_null() && size > 0 {
        *buf = 0;
    }
    0
}

/// POSIX clock_gettime — used by some emcc support code. We only need a
/// non-trapping stub; ymfm timers use their own master-clock countdowns.
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(_clk_id: c_int, tp: *mut Timespec) -> c_int {
    if !tp.is_null() {
        (*tp).tv_sec = 0;
        (*tp).tv_nsec = 0;
    }
    0
}

#[repr(C)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[no_mangle]
pub unsafe extern "C" fn getenv(_name: *const c_char) -> *mut c_char {
    // NEO_DEBUG / NEO_MUTE unused on web.
    std::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn __assert_fail(
    _expr: *const c_char,
    _file: *const c_char,
    _line: c_int,
    _func: *const c_char,
) -> ! {
    abort();
}

#[no_mangle]
pub unsafe extern "C" fn __libcpp_verbose_abort(_fmt: *const c_char) -> ! {
    abort();
}

// Itanium mangled name used by some libc++ builds.
#[no_mangle]
pub unsafe extern "C" fn _ZNSt3__222__libcpp_verbose_abortEPKcz(_fmt: *const c_char) -> ! {
    abort();
}

/// Emscripten integer printf used by assert/debug paths — drop output.
#[no_mangle]
pub unsafe extern "C" fn fiprintf(_stream: *mut c_void, _fmt: *const c_char) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn __small_fprintf(_stream: *mut c_void, _fmt: *const c_char) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn fflush(_stream: *mut c_void) -> c_int {
    0
}

// Dummy stderr so C++ can take its address; glue should not write raw bytes.
#[no_mangle]
pub static mut stderr: *mut c_void = std::ptr::null_mut();
