// All module bodies live in D:\2026Work\LodaxOS\src\mm\* (single source
// of truth shared with the kernel via `#[path]` here). Using
// `#[path]` on the module declaration keeps the file content at
// `crate::mm::<name>` (so `use super::phys` inside resolves correctly),
// unlike a nested `mod _impl; pub use _impl::*;` shim.
#[path = "../../../src/mm/heap.rs"]
pub mod heap;
#[path = "../../../src/mm/phys.rs"]
pub mod phys;
#[path = "../../../src/mm/virt.rs"]
pub mod virt;
#[path = "../../../src/mm/vma.rs"]
pub mod vma;
