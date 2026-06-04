#![no_std]
#![allow(dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod acpi;
pub mod arch;
pub mod font;
pub mod intr;
pub mod logger;
pub mod serial;

// `task` and `cap` module bodies live in D:\2026Work\LodaxOS\src\* (single
// source of truth shared with the kernel via `#[path]` here). Using
// `#[path]` on the module declaration keeps `crate::task` and `crate::cap`
// as the canonical paths, so `use crate::task;` / `use crate::cap;` from
// inside the source files resolve correctly (unlike a nested
// `mod _impl; pub use _impl::*;` shim which would force `super::super::`).
#[path = "../../src/task.rs"]
pub mod task;
#[path = "../../src/cap.rs"]
pub mod cap;

pub mod mm;

// Stub `percpu` so `crate::percpu::tick()` resolves in this crate
// (the real implementation lives in the kernel crate; see file
// comment for details).
pub mod percpu;
