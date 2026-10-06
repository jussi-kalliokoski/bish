//! How a POSIX system does what bish needs.
//!
//! Written once for every Unix: what differs between them is the
//! numbers and layouts, which live in `sys_linux.rs`/`sys_darwin.rs`
//! and reach this file as `sys::*`. A target that is not a Unix at all
//! replaces this file rather than editing it -- see `mod.rs`.

use super::sys;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// `guard + usable` bytes of fresh address space, the lowest `guard`
/// bytes of it unreadable.
///
/// For a stack of bish's own making: stacks grow downward, so the
/// unreadable end is the one an overrun reaches, and it faults there
/// rather than silently corrupting whatever was mapped next. Address
/// space rather than memory -- pages are faulted in as they are
/// touched, so an untouched megabyte costs a page-table entry and
/// nothing else.
pub(crate) fn map_guarded_stack(guard: usize, usable: usize) -> std::io::Result<*mut u8> {
    // POSIX says `(void *) -1`, which every Unix agrees on, so this one
    // is not a per-OS fact.
    const MAP_FAILED: isize = -1;

    let total = guard + usable;
    let base = unsafe { mmap(std::ptr::null_mut(), total, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_PRIVATE | sys::MAP_ANON, -1, 0) };
    if base as isize == MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { mprotect(base, guard, sys::PROT_NONE) } != 0 {
        let failure = std::io::Error::last_os_error();
        unsafe { unmap(base, total) };
        return Err(failure);
    }
    Ok(base)
}

/// Hands `len` bytes mapped by `map_guarded_stack` back to the OS.
///
/// # Safety
///
/// `base` and `len` must be a mapping this module made, and nothing may
/// still be running on it.
pub(crate) unsafe fn unmap(base: *mut u8, len: usize) {
    unsafe { munmap(base, len) };
}
