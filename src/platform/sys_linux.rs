//! What Linux's own numbers and structs are. Tables, not logic -- see
//! this directory's `mod.rs` for why they are collected here.

// A table entry is a fact about this OS, and some facts are only ever
// consumed by the *other* OS's implementation -- `F_SETFD` is how Darwin
// sets close-on-exec where Linux passes `O_CLOEXEC` to `pipe2`, and each
// is dead on the side that does not need it. They are both here because
// the pairing test next door says a name missing from one table is a
// port that half-happened, which is the error worth catching; an unused
// constant is not.
#![allow(dead_code)]

// mmap(2)/mprotect(2). The protection bits agree across every Unix bish
// targets; the mapping flags do not -- an anonymous mapping is 0x20
// here and 0x1000 on Darwin, and nothing but the wrong pages would say
// so.
pub(crate) const PROT_NONE: i32 = 0;
pub(crate) const PROT_READ: i32 = 1;
pub(crate) const PROT_WRITE: i32 = 2;
pub(crate) const MAP_PRIVATE: i32 = 2;
pub(crate) const MAP_ANON: i32 = 0x20;

// open(2)/fcntl(2). `O_CLOEXEC` is the one that differs; `F_SETFD` and
// `FD_CLOEXEC` are the same two numbers everywhere and are here so that
// a reader finds all three in one place.
pub(crate) const O_CLOEXEC: i32 = 0o2000000;
pub(crate) const F_SETFD: i32 = 2;
pub(crate) const FD_CLOEXEC: i32 = 1;
