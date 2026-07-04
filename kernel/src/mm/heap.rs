use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{phys, virt};
use crate::sync::{IrqSaveSpinLock, SyncUnsafeCell};

#[repr(C)]
struct Slab {
    free_head: *mut u8,
    slab_base: *mut u8,
    next: *mut Slab,
    prev: *mut Slab,
    order: u8,
    total_objs: u16,
    free_objs: u16,
}

impl Slab {
    unsafe fn init(base: *mut u8, order: u8, obj_size: usize) -> *mut Slab {
        let slab = base as *mut Slab;
        let objects_start = base.add(core::mem::size_of::<Slab>());
        let page_count = 1usize << order;
        let slab_bytes = page_count * phys::PAGE_SIZE as usize;
        let header = core::mem::size_of::<Slab>();
        if slab_bytes <= header || obj_size < core::mem::size_of::<*mut u8>() {
            return ptr::null_mut();
        }
        let avail = slab_bytes - header;
        let total = (avail / obj_size) as u16;
        let mut cur = objects_start;
        for i in 0..total as usize {
            let next_obj = if i + 1 < total as usize {
                cur.add(obj_size)
            } else {
                ptr::null_mut()
            };
            *(cur as *mut *mut u8) = next_obj;
            cur = next_obj;
        }
        (*slab) = Slab {
            free_head: objects_start,
            slab_base: base,
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            order,
            total_objs: total,
            free_objs: total,
        };
        slab
    }

    unsafe fn alloc_obj(&mut self) -> *mut u8 {
        if self.free_objs == 0 {
            return ptr::null_mut();
        }
        let obj = self.free_head;
        self.free_head = *(obj as *mut *mut u8);
        self.free_objs -= 1;
        obj
    }

    unsafe fn free_obj(&mut self, obj: *mut u8) {
        *(obj as *mut *mut u8) = self.free_head;
        self.free_head = obj;
        self.free_objs += 1;
    }

    fn is_full(&self) -> bool {
        self.free_objs == 0
    }

    fn is_empty(&self) -> bool {
        self.free_objs == self.total_objs
    }

    fn contains(&self, obj: *mut u8) -> bool {
        let base = self.slab_base as usize;
        let end = base + ((1usize << self.order) * phys::PAGE_SIZE as usize);
        let ptr = obj as usize;
        ptr >= base && ptr < end
    }
}

struct KmemCache {
    obj_size: usize,
    slab_order: u8,
    objs_per_slab: u16,
    partial: *mut Slab,
    free: *mut Slab,
    full: *mut Slab,
}

static CACHE_LOCKS: [IrqSaveSpinLock<()>; 9] = [
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()),
];

impl KmemCache {
    const fn new_const(obj_size: usize, slab_order: u8, objs_per_slab: u16) -> Self {
        Self {
            obj_size,
            slab_order,
            objs_per_slab,
            partial: ptr::null_mut(),
            free: ptr::null_mut(),
            full: ptr::null_mut(),
        }
    }

    unsafe fn alloc(&mut self, cache_idx: usize) -> *mut u8 {
        // Fast path: try partial first.
        {
            let _g = CACHE_LOCKS[cache_idx].lock();
            if !self.partial.is_null() {
                let slab = self.partial;
                let obj = (*slab).alloc_obj();
                let is_full = (*slab).is_full();
                if is_full {
                    Self::remove_partial_ptr(self, slab);
                    Self::push_full_ptr(self, slab);
                }
                return obj;
            }
            // Try free list
            if !self.free.is_null() {
                let slab = self.free;
                self.free = (*slab).next;
                if !self.free.is_null() {
                    (*self.free).prev = ptr::null_mut();
                }
                (*slab).next = ptr::null_mut();
                (*slab).prev = ptr::null_mut();
                let obj = (*slab).alloc_obj();
                let is_full = (*slab).is_full();
                if is_full {
                    Self::push_full_ptr(self, slab);
                } else {
                    Self::push_partial_ptr(self, slab);
                }
                return obj;
            }
        }
        // Slow path: need a new slab. Release the per-cache lock before
        // calling into `phys::` to avoid the wrong-direction lock order
        // (`heap → phys`). Re-acquire the lock to install the slab.
        let phys_addr = match phys::alloc_order(self.slab_order as usize) {
            Some(p) => p,
            None => return ptr::null_mut(),
        };
        // Map the slab's phys pages into the higher-half at
        // HIGHER_HALF + phys_addr. Without this explicit mapping, the
        // first dereference of the returned pointer would #PF, the
        // demand pager (vma::handle_page_fault) would allocate a
        // *different* phys page, and the slab's free-list pointer
        // (which lives in the original phys page) would be silently
        // corrupted on the first cross-page allocation. This bug
        // made the heap unsafe to use; see audit S3.
        let num_pages = 1usize << self.slab_order;
        let virt_base = virt::HIGHER_HALF + phys_addr;
        virt::map_contiguous(
            virt::kernel_pml4(),
            virt_base,
            phys_addr,
            num_pages as u64,
            virt::DATA,
        );
        let slab = Slab::init(virt_base as *mut u8, self.slab_order, self.obj_size);
        let obj = (*slab).alloc_obj();
        let is_full = (*slab).is_full();
        let _g = CACHE_LOCKS[cache_idx].lock();
        // Double-check: another thread may have added a slab while we
        // were allocating. So if used, and free our
        // redundant pages after dropping the cache lock (avoid heap→phys lock order).
        if !self.partial.is_null() {
            let existing = self.partial;
            let real_obj = (*existing).alloc_obj();
            let was_full = (*existing).is_full();
            if was_full {
                Self::remove_partial_ptr(self, existing);
                Self::push_full_ptr(self, existing);
            }
            drop(_g);
            for p in 0..num_pages {
                virt::unmap(virt_base + (p as u64) * phys::PAGE_SIZE);
            }
            phys::free_pages(phys_addr, num_pages as u64);
            return real_obj;
        }
        if !self.free.is_null() {
            let existing = self.free;
            self.free = (*existing).next;
            if !self.free.is_null() {
                (*self.free).prev = ptr::null_mut();
            }
            (*existing).next = ptr::null_mut();
            (*existing).prev = ptr::null_mut();
            let real_obj = (*existing).alloc_obj();
            let was_full = (*existing).is_full();
            if was_full {
                Self::push_full_ptr(self, existing);
            } else {
                Self::push_partial_ptr(self, existing);
            }
            drop(_g);
            for p in 0..num_pages {
                virt::unmap(virt_base + (p as u64) * phys::PAGE_SIZE);
            }
            phys::free_pages(phys_addr, num_pages as u64);
            return real_obj;
        }
        if is_full {
            Self::push_full_ptr(self, slab);
        } else {
            Self::push_partial_ptr(self, slab);
        }
        obj
    }

    unsafe fn free(&mut self, obj: *mut u8, cache_idx: usize) {
        let _g = CACHE_LOCKS[cache_idx].lock();

        // Find slab via raw pointer to avoid borrow conflicts
        let slab_ptr = self.find_slab_ptr(obj);
        if slab_ptr.is_null() {
            return;
        }

        let was_full = (*slab_ptr).is_full();
        (*slab_ptr).free_obj(obj);
        let now_empty = (*slab_ptr).is_empty();

        if was_full && !now_empty {
            // Full -> Partial: move from full list to partial list
            Self::remove_full_ptr(self, slab_ptr);
            Self::push_partial_ptr(self, slab_ptr);
        }
        if now_empty {
            Self::remove_from_any_list(self, slab_ptr);
            // Reclaim: return empty slab pages to the physical allocator
            let slab_base = (*slab_ptr).slab_base as u64;
            let order = (*slab_ptr).order;
            let num_pages = 1usize << order;
            let phys_addr = slab_base - virt::HIGHER_HALF;
            // Pass the virtual address to virt::unmap (Bug 5 fix:
            // previously passed phys_addr = slab_base - HIGHER_HALF,
            // which is a physical address — virt::unmap expects a
            // virtual address).
            for p in 0..num_pages {
                virt::unmap(slab_base + (p as u64) * phys::PAGE_SIZE);
            }
            phys::free_pages(phys_addr, num_pages as u64);
        }
    }

    unsafe fn find_slab_ptr(&mut self, obj: *mut u8) -> *mut Slab {
        let mut cur = self.partial;
        while !cur.is_null() {
            if (*cur).contains(obj) {
                return cur;
            }
            cur = (*cur).next;
        }
        let mut cur = self.full;
        while !cur.is_null() {
            if (*cur).contains(obj) {
                return cur;
            }
            cur = (*cur).next;
        }
        ptr::null_mut()
    }

    unsafe fn remove_partial_ptr(&mut self, slab: *mut Slab) {
        let prev = (*slab).prev;
        let next = (*slab).next;
        if !prev.is_null() {
            (*prev).next = next;
        } else {
            self.partial = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*slab).next = ptr::null_mut();
        (*slab).prev = ptr::null_mut();
    }

    unsafe fn push_full_ptr(&mut self, slab: *mut Slab) {
        (*slab).next = self.full;
        (*slab).prev = ptr::null_mut();
        if !self.full.is_null() {
            (*self.full).prev = slab;
        }
        self.full = slab;
    }

    unsafe fn remove_full_ptr(&mut self, slab: *mut Slab) {
        let prev = (*slab).prev;
        let next = (*slab).next;
        if !prev.is_null() {
            (*prev).next = next;
        } else {
            self.full = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
        (*slab).next = ptr::null_mut();
        (*slab).prev = ptr::null_mut();
    }

    unsafe fn push_partial_ptr(&mut self, slab: *mut Slab) {
        (*slab).next = self.partial;
        (*slab).prev = ptr::null_mut();
        if !self.partial.is_null() {
            (*self.partial).prev = slab;
        }
        self.partial = slab;
    }

    unsafe fn push_free_ptr(&mut self, slab: *mut Slab) {
        (*slab).next = self.free;
        (*slab).prev = ptr::null_mut();
        if !self.free.is_null() {
            (*self.free).prev = slab;
        }
        self.free = slab;
    }

    unsafe fn remove_from_any_list(&mut self, slab: *mut Slab) {
        // Check each list for presence
        let lists = [
            &mut self.partial as *mut *mut Slab,
            &mut self.full as *mut *mut Slab,
        ];
        for list_head in lists {
            let mut cur = *list_head;
            while !cur.is_null() {
                if cur == slab {
                    // Found it, remove
                    let prev = (*cur).prev;
                    let next = (*cur).next;
                    if !prev.is_null() {
                        (*prev).next = next;
                    } else {
                        *list_head = next;
                    }
                    if !next.is_null() {
                        (*next).prev = prev;
                    }
                    (*cur).next = ptr::null_mut();
                    (*cur).prev = ptr::null_mut();
                    return;
                }
                cur = (*cur).next;
            }
        }
    }

}

struct CacheAllocator {
    caches: [KmemCache; 9],
    initialized: AtomicBool,
}

/// Compute slab order and objects-per-slab for a given object size.
fn cache_params(obj_size: usize) -> (u8, u16) {
    let header = core::mem::size_of::<Slab>();
    // Start with order 0 (one 4KB page)
    // If obj_size is large, we may need more pages
    for order in 0..=4u8 {
        let page_bytes = (1usize << order) * phys::PAGE_SIZE as usize;
        let avail = page_bytes - header;
        if avail >= obj_size {
            let objs = (avail / obj_size) as u16;
            if objs >= 1 {
                return (order, objs);
            }
        }
    }
    // Fallback: order 4 (64KB) should fit anything
    (4, 1)
}

impl CacheAllocator {
    const fn new() -> Self {
        Self {
            caches: [
                KmemCache::new_const(32, 0, 127),
                KmemCache::new_const(64, 0, 63),
                KmemCache::new_const(128, 0, 31),
                KmemCache::new_const(256, 0, 15),
                KmemCache::new_const(512, 0, 7),
                KmemCache::new_const(1024, 0, 3),
                KmemCache::new_const(2048, 0, 1),
                KmemCache::new_const(4096, 1, 1),
                KmemCache::new_const(8192, 2, 1),
            ],
            initialized: AtomicBool::new(false),
        }
    }

    unsafe fn init(&mut self) {
        if self.initialized.load(Ordering::Acquire) {
            return;
        }
        let sizes = [32usize, 64, 128, 256, 512, 1024, 2048, 4096, 8192];
        for (i, &size) in sizes.iter().enumerate() {
            let (order, objs) = cache_params(size);
            self.caches[i] = KmemCache::new_const(size, order, objs);
        }
        self.initialized.store(true, Ordering::Release);
    }

    fn cache_index(size: usize) -> Option<usize> {
        match size {
            0..=32 => Some(0),
            33..=64 => Some(1),
            65..=128 => Some(2),
            129..=256 => Some(3),
            257..=512 => Some(4),
            513..=1024 => Some(5),
            1025..=2048 => Some(6),
            2049..=4096 => Some(7),
            4097..=8192 => Some(8),
            _ => None,
        }
    }

    unsafe fn kmalloc(&mut self, size: usize) -> *mut u8 {
        self.kmalloc_aligned(size, 1)
    }

    /// Allocate with a minimum alignment.  For alignments that exceed the
    /// slab allocator's natural alignment (≤16), fall through to direct
    /// page allocation which provides 4 KB alignment.
    unsafe fn kmalloc_aligned(&mut self, size: usize, align: usize) -> *mut u8 {
        // Slab caches only guarantee object-size alignment (≤ 16 B for 32 B objs).
        // Larger alignments require direct page allocation (4 KB aligned).
        if align > 16 {
            let pages = (size + phys::PAGE_SIZE as usize - 1) / phys::PAGE_SIZE as usize;
            let order = {
                let mut o = 0usize;
                while (1usize << o) < pages { o += 1; }
                if o > 10 { return ptr::null_mut(); }
                o
            };
            let Some(phys_addr) = phys::alloc_order(order) else {
                return ptr::null_mut();
            };
            let virt_base = virt::HIGHER_HALF + phys_addr;
            // Use kernel_pml4() for kernel heap mappings (Bug 7 fix:
            // pml4_address() reads CR3, which may be a per-process PML4
            // when called from a non-scheduler context, making the mapping
            // invisible under the kernel PML4).
            virt::map_contiguous(
                virt::kernel_pml4(),
                virt_base,
                phys_addr,
                pages as u64,
                virt::DATA,
            );
            return virt_base as *mut u8;
        }
        let idx = Self::cache_index(size);
        match idx {
            Some(i) => self.caches[i].alloc(i),
            None => {
                // For very large allocations, allocate pages directly
                let pages = (size + phys::PAGE_SIZE as usize - 1) / phys::PAGE_SIZE as usize;
                let order = {
                    let mut o = 0usize;
                    while (1usize << o) < pages { o += 1; }
                    if o > 10 { return ptr::null_mut(); }
                    o
                };
                let Some(phys_addr) = phys::alloc_order(order) else {
                    return ptr::null_mut();
                };
                // Map into higher-half (see comment in KmemCache::alloc).
                let virt_base = virt::HIGHER_HALF + phys_addr;
                virt::map_contiguous(
                    virt::kernel_pml4(),
                    virt_base,
                    phys_addr,
                    pages as u64,
                    virt::DATA,
                );
                virt_base as *mut u8
            }
        }
    }

    unsafe fn kfree(&mut self, ptr: *mut u8, size: usize) {
        let idx = Self::cache_index(size);
        match idx {
            Some(i) => self.caches[i].free(ptr, i),
            None => {
                let pages = (size + phys::PAGE_SIZE as usize - 1) / phys::PAGE_SIZE as usize;
                let virt_base = ptr as u64;
                for i in 0..pages {
                    let addr = virt_base + (i as u64) * phys::PAGE_SIZE;
                    let phys_addr = addr - virt::HIGHER_HALF;
                    // Unmap the virtual mapping before freeing the physical
                    // page (Bug 6 fix: previously only freed physical pages,
                    // leaving stale PTEs pointing to now-free memory).
                    virt::unmap(addr);
                    phys::free_page(phys_addr);
                }
            }
        }
    }
}

unsafe impl Send for CacheAllocator {}
unsafe impl Sync for CacheAllocator {}

fn allocator_ptr() -> *mut CacheAllocator {
    ALLOCATOR.get()
}

static ALLOCATOR: SyncUnsafeCell<CacheAllocator> = SyncUnsafeCell::new(CacheAllocator::new());

pub fn init() {
    unsafe {
        (*allocator_ptr()).init();
    }
    log::info!("Slab allocator initialized (32B..8KB caches)");
}

pub struct GlobalAllocator;

unsafe impl GlobalAlloc for GlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let a = allocator_ptr();
        if !(*a).initialized.load(Ordering::Acquire) {
            return ptr::null_mut();
        }
        // Pass alignment alongside size so the slab allocator can
        // guarantee the returned pointer satisfies the requested alignment.
        unsafe { (*a).kmalloc_aligned(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let a = allocator_ptr();
        if !(*a).initialized.load(Ordering::Acquire) {
            return;
        }
        unsafe { (*a).kfree(ptr, layout.size()) };
    }
}
