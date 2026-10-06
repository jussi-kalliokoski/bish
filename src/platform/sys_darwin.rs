//! What macOS's own numbers and structs are. Tables, not logic -- see
//! this directory's `mod.rs` for why they are collected here.
//!
//! Values are from the macOS SDK headers of the same name as the system
//! call they belong to, and the `sys_tables` test next door keeps this
//! file and `sys_linux.rs` exposing the same names, so a port cannot
//! half-happen.

// mmap(2)/mprotect(2), from <sys/mman.h>. `MAP_ANON` is the name Darwin
// spells it; `MAP_ANONYMOUS` is a Linux alias for the same idea, with a
// different value.
pub(crate) const PROT_NONE: i32 = 0x00;
pub(crate) const PROT_READ: i32 = 0x01;
pub(crate) const PROT_WRITE: i32 = 0x02;
pub(crate) const MAP_PRIVATE: i32 = 0x0002;
pub(crate) const MAP_ANON: i32 = 0x1000;
