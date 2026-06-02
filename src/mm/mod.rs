pub mod phys;
pub mod virt;
pub mod heap;
pub mod vma;

#[global_allocator]
static ALLOCATOR: heap::GlobalAllocator = heap::GlobalAllocator;
