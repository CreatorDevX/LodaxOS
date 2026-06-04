use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lodaxos_system::{CapOp, Caps};
use crate::cap;

pub const PAGE_SHIFT: u64 = 12;
pub const PAGE_SIZE: u64 = 0x1000;

const MAX_ORDER: usize = 10;
const ORDER_COUNT: usize = MAX_ORDER + 1;

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
struct FreeBlock {
    next: *mut FreeBlock,
}

struct Zone {
    base: u64,
    top: u64,
    free_lists: [*mut FreeBlock; ORDER_COUNT],
    total_pages: AtomicUsize,
    free_pages: AtomicUsize,
}

const NULL_FB: *mut FreeBlock = ptr::null_mut();

fn zone_ptr() -> *mut Zone {
    &raw mut ZONE
}

static mut ZONE: Zone = Zone {
    base: 0,
    top: 0,
    free_lists: [NULL_FB; ORDER_COUNT],
    total_pages: AtomicUsize::new(0),
    free_pages: AtomicUsize::new(0),
};

static LOCK: SpinLock = SpinLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

const BOOTINFO_HANDOFF_PAGE: u64 = 0x1000;

/// Physical base of the dynamically-allocated BootInfo struct.  Set by
/// `init_from_regions`; checked by `is_reserved_page` to prevent double-free
/// of the BootInfo page(s) the caller did not include in its free-memory list.
static mut BOOTINFO_RESERVED_BASE: u64 = 0;
static mut BOOTINFO_RESERVED_PAGES: u64 = 0;

fn block_size(order: usize) -> u64 {
    (1u64 << order) * PAGE_SIZE
}

fn max_order_for_pages(page_count: u64) -> usize {
    if page_count == 0 {
        return 0;
    }
    page_count.ilog2().min(MAX_ORDER as u32) as usize
}

fn max_order_at(phys_addr: u64, remaining_bytes: u64) -> usize {
    let pages = phys_addr / PAGE_SIZE;
    let align_bits = pages.trailing_zeros() as usize;
    let size_bits = if remaining_bytes < PAGE_SIZE {
        0
    } else {
        let p = remaining_bytes / PAGE_SIZE;
        p.ilog2() as usize
    };
    align_bits.min(size_bits).min(MAX_ORDER)
}

unsafe fn add_to_free_list(zone: &mut Zone, addr: u64, order: usize) {
    let block = addr as *mut FreeBlock;
    (*block).next = zone.free_lists[order];
    zone.free_lists[order] = block;
}

unsafe fn pop_from_free_list(zone: &mut Zone, order: usize) -> Option<u64> {
    let head = zone.free_lists[order];
    if head.is_null() {
        return None;
    }
    zone.free_lists[order] = (*head).next;
    Some(head as u64)
}

unsafe fn split_and_enqueue(zone: &mut Zone, addr: u64, high_order: usize, target_order: usize) {
    let mut order = high_order;
    while order > target_order {
        order -= 1;
        let buddy_addr = addr ^ (1u64 << (order + 12));
        add_to_free_list(zone, buddy_addr, order);
    }
}

fn is_reserved_page(page: u64) -> bool {
    if page == 0 || page == BOOTINFO_HANDOFF_PAGE / PAGE_SIZE {
        return true;
    }
    let base = unsafe { BOOTINFO_RESERVED_BASE };
    let pages = unsafe { BOOTINFO_RESERVED_PAGES };
    if pages == 0 {
        return false;
    }
    page >= base / PAGE_SIZE && page < (base / PAGE_SIZE) + pages
}

/// Reserve a single 4 KB page so the buddy allocator will never hand it out.
/// Removes the page from whatever free-list block contains it by splitting
/// the encompassing block around the target address.
unsafe fn reserve_one_page(zone: &mut Zone, target: u64) {
    for order in 0..ORDER_COUNT {
        let mut curr = zone.free_lists[order];
        let mut prev: *mut FreeBlock = ptr::null_mut();
        while !curr.is_null() {
            let block_addr = curr as u64;
            let block_end = block_addr + block_size(order);
            let next = (*curr).next;

            if target >= block_addr && target < block_end {
                // Unlink this block
                if prev.is_null() {
                    zone.free_lists[order] = next;
                } else {
                    (*prev).next = next;
                }
                zone.free_pages.fetch_sub(1 << order, Ordering::Relaxed);

                // Re-add the portion before the target page
                let before = target - block_addr;
                if before > 0 {
                    let before_order = max_order_at(block_addr, before);
                    add_to_free_list(zone, block_addr, before_order);
                    zone.free_pages.fetch_add(1 << before_order, Ordering::Relaxed);
                }

                // Re-add the portion after the target page
                let after_start = target + PAGE_SIZE;
                let after = block_end - after_start;
                if after > 0 {
                    let after_order = max_order_at(after_start, after);
                    add_to_free_list(zone, after_start, after_order);
                    zone.free_pages.fetch_add(1 << after_order, Ordering::Relaxed);
                }
                return;
            }
            prev = curr;
            curr = next;
        }
    }
}

fn carve_blocks(zone: &mut Zone, mut start: u64, size: u64) -> usize {
    if size == 0 {
        return 0;
    }
    let end = start + size;
    let mut total = 0usize;

    // Handle page 0: if the region starts at or contains page 0, skip it
    if start == 0 {
        start = PAGE_SIZE;
    }
    if start < PAGE_SIZE && end > PAGE_SIZE {
        start = PAGE_SIZE;
    }

    // Handle BootInfo handoff page at 0x1000 (the chainloader leaves the
    // 8-byte pointer to the dynamically-allocated BootInfo struct at this
    // fixed address; the actual struct lives elsewhere).
    if start == BOOTINFO_HANDOFF_PAGE {
        start = BOOTINFO_HANDOFF_PAGE + PAGE_SIZE;
    }
    if start < BOOTINFO_HANDOFF_PAGE && end > BOOTINFO_HANDOFF_PAGE {
        carve_blocks(zone, start, BOOTINFO_HANDOFF_PAGE - start);
        start = BOOTINFO_HANDOFF_PAGE + PAGE_SIZE;
    }

    // Carve remaining range into maximal buddy blocks.
    // UEFI memory regions are always page-aligned, but guard against
    // sub-page leftovers so we never carve past the region boundary.
    let mut addr = start;
    while addr < end {
        let remaining = end - addr;
        if remaining < PAGE_SIZE {
            break;
        }
        let order = max_order_at(addr, remaining);
        let block = block_size(order);
        unsafe {
            add_to_free_list(zone, addr, order);
        }
        total += 1 << order;
        addr += block;
    }

    total
}

/// Initialise the buddy allocator from the given free memory regions.
/// `boot_info_phys` is the physical address of the dynamically-allocated
/// BootInfo struct — its page(s) are removed from the free lists so the
/// buddy allocator does not re-issue them (the UEFI memory map includes
/// the BootInfo's region as EfiLoaderData / free).
pub unsafe fn init_from_regions(regions: &[(u64, u64)], boot_info_phys: u64) {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    log::debug!("Physical allocator (buddy): {} regions", regions.len());

    let mut min_base = u64::MAX;
    let mut max_top = 0u64;
    for &(start, size) in regions {
        if start < min_base { min_base = start; }
        let end = start + size;
        if end > max_top { max_top = end; }
    }

    if min_base == u64::MAX || max_top == 0 {
        log::error!("Physical allocator: no usable memory regions");
        return;
    }

    let zp = zone_ptr();
    unsafe {
        (*zp).base = min_base;
        (*zp).top = max_top;
    }

    let mut total_pages = 0usize;
    for &(start, size) in regions {
        total_pages += carve_blocks(unsafe { &mut *zone_ptr() }, start, size);
    }

    // Reserve the BootInfo page(s) so the buddy never hands them out.
    // The BootInfo struct is ~2 KB, so we reserve up to one 4 KB page.
    let bootinfo_pages = {
        let sz = core::mem::size_of::<lodaxos_system::BootInfo>();
        (sz + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize
    };
    let bootinfo_base = boot_info_phys & !0xFFF;
    for i in 0..bootinfo_pages {
        let addr = bootinfo_base + (i as u64) * PAGE_SIZE;
        unsafe { reserve_one_page(&mut *zone_ptr(), addr) };
    }
    total_pages = total_pages.saturating_sub(bootinfo_pages);
    unsafe {
        BOOTINFO_RESERVED_BASE = bootinfo_base;
        BOOTINFO_RESERVED_PAGES = bootinfo_pages as u64;
    }

    unsafe {
        (*zp).total_pages.store(total_pages, Ordering::Relaxed);
        (*zp).free_pages.store(total_pages, Ordering::Relaxed);
    }

    log::info!("Physical allocator (buddy): {} pages free ({} MB), orders 0-{}",
        total_pages,
        total_pages as u64 * PAGE_SIZE / (1024 * 1024),
        MAX_ORDER);

    INITIALIZED.store(true, Ordering::SeqCst);
}

pub fn alloc_order(order: usize) -> Option<u64> {
    if order > MAX_ORDER {
        return None;
    }
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        Caps::CAP_MM_ALLOC,
        CapOp::MmAlloc { frames: 1 << order },
    ) {
        log::warn!("phys::alloc_order: cap denied: {:?}", e);
        return None;
    }

    LOCK.lock();
    let result = {
        let zp = zone_ptr();
        let zone = unsafe { &mut *zp };
        let mut o = order;
        while o <= MAX_ORDER && zone.free_lists[o].is_null() {
            o += 1;
        }
        let addr = if o > MAX_ORDER {
            None
        } else {
            let addr = unsafe { pop_from_free_list(zone, o).unwrap() };
            if o > order {
                unsafe { split_and_enqueue(zone, addr, o, order) };
            }
            zone.free_pages.fetch_sub(1 << order, Ordering::Relaxed);
            Some(addr)
        };
        addr
    };
    LOCK.unlock();

    if result.is_none() {
        log::error!("alloc_order({}): out of memory!", order);
    }
    result
}

pub fn free_order(addr: u64, order: usize) {
    if order > MAX_ORDER || addr % PAGE_SIZE != 0 {
        return;
    }
    if is_reserved_page(addr / PAGE_SIZE) {
        return;
    }
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        Caps::CAP_MM_ALLOC,
        CapOp::MmAlloc { frames: 1 << order },
    ) {
        log::warn!("phys::free_order: cap denied: {:?}", e);
        return;
    }

    LOCK.lock();
    let zone = unsafe { &mut *zone_ptr() };
    let mut merge_addr = addr;
    let mut merge_order = order;

    while merge_order < MAX_ORDER {
        let buddy_addr = merge_addr ^ (1u64 << (merge_order + 12));
        let bsize = block_size(merge_order);

        if buddy_addr < zone.base || buddy_addr + bsize > zone.top {
            break;
        }

        // Search for buddy in the free list
        let mut found = false;

        // Check if buddy is the head
        if zone.free_lists[merge_order] as u64 == buddy_addr {
            zone.free_lists[merge_order] = unsafe { (*zone.free_lists[merge_order]).next };
            found = true;
        } else {
            let mut curr = zone.free_lists[merge_order];
            while !curr.is_null() {
                let next = unsafe { (*curr).next };
                if next as u64 == buddy_addr {
                    unsafe { (*curr).next = (*next).next };
                    found = true;
                    break;
                }
                curr = next;
            }
        }

        if !found {
            break;
        }

        if buddy_addr < merge_addr {
            merge_addr = buddy_addr;
        }
        merge_order += 1;
    }

    unsafe { add_to_free_list(zone, merge_addr, merge_order) };
    zone.free_pages.fetch_add(1 << merge_order, Ordering::Relaxed);
    LOCK.unlock();
}

pub fn alloc_page() -> Option<u64> {
    alloc_order(0)
}

pub fn free_page(addr: u64) {
    free_order(addr, 0);
}

pub fn alloc_pages(count: u64) -> Option<u64> {
    match count {
        0 => None,
        1 => alloc_page(),
        _ => {
            let order = max_order_for_pages(count);
            let alloc_count = 1u64 << order;
            alloc_order(order).map(|addr| {
                if alloc_count > count {
                    let excess = alloc_count - count;
                    for i in 0..excess {
                        free_page(addr + (count + i) * PAGE_SIZE);
                    }
                }
                addr
            })
        }
    }
}

pub fn free_pages(addr: u64, count: u64) {
    if count == 0 || is_reserved_page(addr / PAGE_SIZE) {
        return;
    }
    for i in 0..count {
        free_page(addr + i * PAGE_SIZE);
    }
}

pub fn free_pages_count() -> usize {
    unsafe { (*zone_ptr()).free_pages.load(Ordering::Relaxed) }
}

pub fn total_pages() -> usize {
    unsafe { (*zone_ptr()).total_pages.load(Ordering::Relaxed) }
}
