use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{phys, virt};

struct SpinLock {
    locked: AtomicBool,
}

impl SpinLock {
    const fn new() -> Self {
        Self { locked: AtomicBool::new(false) }
    }
    fn lock(&self) {
        while self.locked.compare_exchange_weak(
            false, true, Ordering::Acquire, Ordering::Relaxed,
        ).is_err() {
            core::hint::spin_loop();
        }
    }
    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

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
        let avail = slab_bytes - core::mem::size_of::<Slab>();
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
    lock: SpinLock,
}

impl KmemCache {
    const fn new_const(obj_size: usize, slab_order: u8, objs_per_slab: u16) -> Self {
        Self {
            obj_size,
            slab_order,
            objs_per_slab,
            partial: ptr::null_mut(),
            free: ptr::null_mut(),
            full: ptr::null_mut(),
            lock: SpinLock::new(),
        }
    }

    unsafe fn alloc(&mut self) -> *mut u8 {
        self.lock.lock();

        // Try partial list first (raw ptr to avoid borrow conflicts)
        if !self.partial.is_null() {
            let slab = self.partial;
            let obj = (*slab).alloc_obj();
            let is_full = (*slab).is_full();
            if is_full {
                self.remove_partial_ptr(slab);
                self.push_full_ptr(slab);
            }
            self.lock.unlock();
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
                self.push_full_ptr(slab);
            } else {
                self.push_partial_ptr(slab);
            }
            self.lock.unlock();
            return obj;
        }

        // Allocate new slab
        let Some(phys_addr) = phys::alloc_order(self.slab_order as usize) else {
            self.lock.unlock();
            return ptr::null_mut();
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
            virt::pml4_address(),
            virt_base,
            phys_addr,
            num_pages as u64,
            virt::DATA,
        );
        let slab = Slab::init(virt_base as *mut u8, self.slab_order, self.obj_size);
        let obj = (*slab).alloc_obj();
        let is_full = (*slab).is_full();
        if is_full {
            self.push_full_ptr(slab);
        } else {
            self.push_partial_ptr(slab);
        }
        self.lock.unlock();
        obj
    }

    unsafe fn free(&mut self, obj: *mut u8) {
        self.lock.lock();

        // Find slab via raw pointer to avoid borrow conflicts
        let slab_ptr = self.find_slab_ptr(obj);
        if slab_ptr.is_null() {
            self.lock.unlock();
            return;
        }

        let was_full = (*slab_ptr).is_full();
        (*slab_ptr).free_obj(obj);
        let now_empty = (*slab_ptr).is_empty();

        if was_full && !now_empty {
            // Full -> Partial: move from full list to partial list
            self.remove_full_ptr(slab_ptr);
            self.push_partial_ptr(slab_ptr);
        }
        if now_empty {
            // All freed: move to free list
            // Check partial list first, then full
            self.remove_from_any_list(slab_ptr);
            self.push_free_ptr(slab_ptr);
        }
        self.lock.unlock();
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
        if self.initialized.load(Ordering::SeqCst) {
            return;
        }
        // Recalculate params for each cache based on actual obj_size
        let sizes = [32usize, 64, 128, 256, 512, 1024, 2048, 4096, 8192];
        for (i, &size) in sizes.iter().enumerate() {
            let (order, objs) = cache_params(size);
            self.caches[i] = KmemCache::new_const(size, order, objs);
        }
        self.initialized.store(true, Ordering::SeqCst);
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
        let idx = Self::cache_index(size);
        match idx {
            Some(i) => self.caches[i].alloc(),
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
                    virt::pml4_address(),
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
            Some(i) => self.caches[i].free(ptr),
            None => {
                // For direct page allocations, free the pages
                // (We don't track the order, so free as order-0 pages)
                let pages = (size + phys::PAGE_SIZE as usize - 1) / phys::PAGE_SIZE as usize;
                for i in 0..pages {
                    // ptr is the higher-half virt (HIGHER_HALF + phys); convert back.
                    let phys_addr = (ptr as u64) - virt::HIGHER_HALF
                        + (i as u64) * phys::PAGE_SIZE;
                    phys::free_page(phys_addr);
                }
            }
        }
    }
}

fn allocator_ptr() -> *mut CacheAllocator {
    &raw mut ALLOCATOR
}

static mut ALLOCATOR: CacheAllocator = CacheAllocator::new();

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
        if !(*a).initialized.load(Ordering::SeqCst) {
            return ptr::null_mut();
        }
        unsafe { (*a).kmalloc(layout.size()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let a = allocator_ptr();
        if (*a).initialized.load(Ordering::SeqCst) {
            unsafe { (*a).kfree(ptr, layout.size()) };
        }
    }
}
