//! What Linux's own numbers and structs are. Tables, not logic -- see
//! this directory's `mod.rs` for why they are collected here.

// mmap(2)/mprotect(2). The protection bits agree across every Unix bish
// targets; the mapping flags do not -- an anonymous mapping is 0x20
// here and 0x1000 on Darwin, and nothing but the wrong pages would say
// so.
pub(crate) const PROT_NONE: i32 = 0;
pub(crate) const PROT_READ: i32 = 1;
pub(crate) const PROT_WRITE: i32 = 2;
pub(crate) const MAP_PRIVATE: i32 = 2;
pub(crate) const MAP_ANON: i32 = 0x20;
