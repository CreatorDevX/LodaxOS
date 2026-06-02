pub use lodaxos_core::mm::{heap, phys, virt, vma};

#[global_allocator]
static ALLOCATOR: heap::GlobalAllocator = heap::GlobalAllocator;
