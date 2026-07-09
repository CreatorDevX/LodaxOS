use core::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use crate::sync::SyncUnsafeCell;
use core::ptr;

use crate::sync::IrqSaveSpinLock;

/// Global lock for multi-page alloc/free operations.
/// Prevents TOCTOU races between `alloc_pages`/`free_pages`
/// and concurrent higher-order operations.
static MULTI_PAGE_LOCK: IrqSaveSpinLock<()> = IrqSaveSpinLock::new(());

pub const PAGE_SHIFT: u64 = 12;
pub const PAGE_SIZE: u64 = 0x1000;

const MAX_ORDER: usize = 10;
const BOOTINFO_HANDOFF_PAGE: u64 = 0x5000;

/// Maximum physical address supported by the refcount array (128 MB).
/// Pages beyond this have an implicit refcount of 1 (no tracking).
const REFCOUNT_MAX_PAGES: usize = 128 * 1024 * 1024 / PAGE_SIZE as usize; // 32768

/// Per-page reference count for COW tracking. Index = page frame number.
/// Initialized to 1 for all pages (each free page has refcount 1).
/// AtomicU16 supports up to 65535 references per page, which is ample.
static REFCOUNTS: [AtomicU16; REFCOUNT_MAX_PAGES] = {
    // AtomicU16::new(1) is const, but we need an array constructor.
    // This const fn builds the array at compile time.
    const ONE: AtomicU16 = AtomicU16::new(1);
    [ONE; REFCOUNT_MAX_PAGES]
};
static REFCOUNT_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct FreeBlock {
    next: *mut FreeBlock,
    order: usize,
}

struct Zone {
    base: u64,
    top: u64,
    free_lists: [*mut FreeBlock; MAX_ORDER + 1],
    total_pages: AtomicUsize,
    free_pages: AtomicUsize,
}

unsafe impl Send for Zone {}
unsafe impl Sync for Zone {}

const NULL: *mut FreeBlock = ptr::null_mut();

static ZONE: SyncUnsafeCell<Zone> = SyncUnsafeCell::new(Zone {
    base: 0,
    top: 0,
    free_lists: [NULL; MAX_ORDER + 1],
    total_pages: AtomicUsize::new(0),
    free_pages: AtomicUsize::new(0),
});

static LOCKS: [IrqSaveSpinLock<()>; MAX_ORDER + 1] = [
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()), IrqSaveSpinLock::new(()),
    IrqSaveSpinLock::new(()),
];
static INITIALIZED: AtomicBool = AtomicBool::new(false);

static BOOTINFO_RESERVED_PN: AtomicUsize = AtomicUsize::new(usize::MAX);
static BOOTINFO_RESERVED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Kernel image physical range (set once by `init_from_regions`).
/// Protects the kernel's own code/data pages from being freed and
/// re-allocated -- the kernel image is excluded from the initial free
/// list, but a spurious `free_page` call would otherwise add it back.
static KERNEL_RESERVED_PN: AtomicUsize = AtomicUsize::new(usize::MAX);
static KERNEL_RESERVED_PAGES: AtomicUsize = AtomicUsize::new(0);

fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

fn align_up(x: u64, align: u64) -> u64 {
    x.saturating_add(align - 1) & !(align - 1)
}

fn block_size(order: usize) -> u64 {
    (1u64 << order) * PAGE_SIZE
}

fn is_reserved_page(pn: u64) -> bool {
    if pn == 0 || pn == BOOTINFO_HANDOFF_PAGE / PAGE_SIZE {
        return true;
    }
    // Check BootInfo struct pages
    let base = BOOTINFO_RESERVED_PN.load(Ordering::Acquire);
    let pages = BOOTINFO_RESERVED_PAGES.load(Ordering::Acquire);
    if base != usize::MAX && pages != 0 {
        let pn_usize = pn as usize;
        if pn_usize >= base && pn_usize < base + pages {
            return true;
        }
    }
    // Check kernel image pages (never free these)
    let kbase = KERNEL_RESERVED_PN.load(Ordering::Acquire);
    let kpages = KERNEL_RESERVED_PAGES.load(Ordering::Acquire);
    if kbase != usize::MAX && kpages != 0 {
        let pn_usize = pn as usize;
        if pn_usize >= kbase && pn_usize < kbase + kpages {
            return true;
        }
    }
    false
}

unsafe fn phys_to_block(phys: u64) -> &'static mut FreeBlock {
    &mut *(phys as *mut FreeBlock)
}

fn block_phys(block: *mut FreeBlock) -> u64 {
    block as u64
}

unsafe fn add_block(zone: &mut Zone, phys: u64, order: usize) {
    let block = phys_to_block(phys);
    block.next = zone.free_lists[order];
    block.order = order;
    zone.free_lists[order] = block;
    zone.free_pages.fetch_add(1usize << order, Ordering::Relaxed);
}

unsafe fn pop_block(zone: &mut Zone, order: usize) -> Option<u64> {
    let head = zone.free_lists[order];
    if head.is_null() {
        return None;
    }
    zone.free_lists[order] = (*head).next;
    (*head).next = ptr::null_mut();
    zone.free_pages.fetch_sub(1usize << order, Ordering::Relaxed);
    Some(block_phys(head))
}

/// Remove a specific block from the free list for a given order.
unsafe fn remove_block(zone: &mut Zone, target_phys: u64, order: usize) -> bool {
    let target = target_phys as *mut FreeBlock;
    let mut prev: *mut FreeBlock = ptr::null_mut();
    let mut curr = zone.free_lists[order];

    while !curr.is_null() {
        if curr == target {
            if prev.is_null() {
                zone.free_lists[order] = (*curr).next;
            } else {
                (*prev).next = (*curr).next;
            }
            (*curr).next = ptr::null_mut();
            zone.free_pages.fetch_sub(1usize << order, Ordering::Relaxed);
            return true;
        }
        prev = curr;
        curr = (*curr).next;
    }
    false
}

/// Carve a contiguous free range into power-of-2 buddy blocks.
unsafe fn carve_range(zone: &mut Zone, start: u64, end: u64) {
    let mut addr = start;
    while addr < end {
        let remaining = end - addr;
        let pn = addr / PAGE_SIZE;
        let align_zeros = pn.trailing_zeros() as usize;
        let max_pages = remaining / PAGE_SIZE;
        let max_fit = if max_pages <= 1 {
            0
        } else {
            63 - (max_pages.wrapping_sub(1)).leading_zeros() as usize
        };

        let order = MAX_ORDER.min(align_zeros.min(max_fit));
        let _g = LOCKS[order].lock();
        add_block(zone, addr, order);
        drop(_g);
        addr += block_size(order);
    }
}

/// Try to coalesce a freed block with its buddy, recursing upward.
unsafe fn coalesce(zone: &mut Zone, addr: u64, order: usize) {
    if order >= MAX_ORDER {
        let _g = LOCKS[order].lock();
        add_block(zone, addr, order);
        return;
    }

    let buddy = addr ^ block_size(order);

    // Hold the per-order lock across the remove+add sequence
    // to prevent a concurrent alloc/free from interleaving:
    //   remove buddy  ->  (window closed)  ->  add our block
    let _g = LOCKS[order].lock();
    if remove_block(zone, buddy, order) {
        // Buddy was found -- recurse to the next order.
        // Release current lock before recursing (lock order: low->high).
        drop(_g);
        coalesce(zone, addr.min(buddy), order + 1);
    } else {
        add_block(zone, addr, order);
        // Lock released here when _g drops
    }
}

fn range_overlaps(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && a_end > b_start
}

/// Remove all blocks overlapping [rstart, rend) from the given order's free list.
/// Non-overlapping portions of partially-covered blocks are re-inserted at
/// lower orders so they remain available.
///
/// BUG 8 fix: collect all blocks to remove first, then release the lock
/// before calling `carve_range` (which acquires lower-order locks).
/// Holding `LOCKS[order]` while acquiring `LOCKS[lower]` is safe today,
/// but fragile against future callers that might hold a lower-order lock.
unsafe fn remove_range(zone: &mut Zone, rstart: u64, rend: u64, order: usize) {
    // Phase 1: walk the free list under the lock, collect blocks to split.
    let mut to_carve: [(u64, u64); 64] = [(0, 0); 64]; // (start, end) pairs
    let mut carve_count = 0usize;
    {
        let _g = LOCKS[order].lock();
        let bsize = block_size(order);
        let mut prev: *mut FreeBlock = ptr::null_mut();
        let mut curr = zone.free_lists[order];

        while !curr.is_null() {
            let cphys = block_phys(curr);
            let cend = cphys + bsize;
            let next = (*curr).next;

            if range_overlaps(cphys, cend, rstart, rend) {
                // Remove the block
                if prev.is_null() {
                    zone.free_lists[order] = next;
                } else {
                    (*prev).next = next;
                }
                zone.free_pages.fetch_sub(1usize << order, Ordering::Relaxed);

                // Record non-overlapping portions for re-insertion below
                if cphys < rstart && carve_count < to_carve.len() {
                    to_carve[carve_count] = (cphys, rstart);
                    carve_count += 1;
                }
                if cend > rend && carve_count < to_carve.len() {
                    to_carve[carve_count] = (rend, cend);
                    carve_count += 1;
                }
            } else {
                prev = curr;
            }

            curr = next;
        }
    } // LOCKS[order] released here

    // Phase 2: re-add non-overlapping portions outside the lock.
    // `carve_range` acquires lower-order locks, which is safe without
    // holding the higher-order lock.
    for i in 0..carve_count {
        let (cs, ce) = to_carve[i];
        carve_range(zone, cs, ce);
    }
}

/// Check whether a physical page overlaps any of the exclude ranges.
fn is_excluded(phys: u64, exclude_ranges: &[(u64, u64)]) -> bool {
    let page_end = phys.saturating_add(PAGE_SIZE);
    for &(start, size) in exclude_ranges {
        let end = start.saturating_add(size);
        if phys < end && page_end > start {
            return true;
        }
    }
    false
}

/// Initialise the physical page allocator from the final bootloader memory map.
///
/// Carves each usable memory region into the largest possible power-of-2 blocks
/// and inserts them into the buddy free lists. Reserved pages (page 0, the
/// BootInfo handoff page, BootInfo struct pages, and explicit exclude ranges)
/// are never added to the free lists.
pub unsafe fn init_from_regions(
    regions: &[(u64, u64)],
    boot_info_phys: u64,
    kernel_start: u64,
    kernel_size: u64,
    exclude_ranges: &[(u64, u64)],
) {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    // Record kernel image range to protect from spurious free_page calls.
    if kernel_size > 0 {
        let kstart = align_down(kernel_start, PAGE_SIZE);
        let kend = align_up(kernel_start.saturating_add(kernel_size), PAGE_SIZE);
        KERNEL_RESERVED_PN.store((kstart / PAGE_SIZE) as usize, Ordering::Release);
        KERNEL_RESERVED_PAGES.store(((kend - kstart) / PAGE_SIZE) as usize, Ordering::Release);
    }

    log::debug!("Physical allocator (buddy): {} regions", regions.len());

    let zone = unsafe { &mut *ZONE.get() };

    zone.base = 0;
    zone.top = 0;
    zone.free_lists = [ptr::null_mut(); MAX_ORDER + 1];
    zone.total_pages.store(0, Ordering::Relaxed);
    zone.free_pages.store(0, Ordering::Relaxed);

    // Record BootInfo pages to reserve
    let bootinfo_pages = {
        let sz = core::mem::size_of::<lodaxos_system::BootInfo>();
        (sz + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize
    };
    let bootinfo_base = align_down(boot_info_phys, PAGE_SIZE);
    BOOTINFO_RESERVED_PN.store((bootinfo_base / PAGE_SIZE) as usize, Ordering::Release);
    BOOTINFO_RESERVED_PAGES.store(bootinfo_pages, Ordering::Release);

    // Determine zone base and top from regions
    let mut min_addr = u64::MAX;
    let mut max_addr = 0u64;
    for &(start, size) in regions {
        let a = align_up(start, PAGE_SIZE);
        let b = align_down(start.saturating_add(size), PAGE_SIZE);
        if a < b {
            min_addr = min_addr.min(a);
            max_addr = max_addr.max(b);
        }
    }
    if min_addr == u64::MAX {
        min_addr = 0;
    }
    zone.base = min_addr;
    zone.top = max_addr;

    // Walk each region, carving contiguous non-reserved ranges into buddy blocks
    for &(start, size) in regions {
        let reg_start = align_up(start, PAGE_SIZE);
        let reg_end = align_down(start.saturating_add(size), PAGE_SIZE);
        if reg_start >= reg_end {
            continue;
        }

        let mut range_start = 0u64;
        let mut in_range = false;
        let mut addr = reg_start;

        while addr < reg_end {
            let pn = addr / PAGE_SIZE;
            let free = !is_reserved_page(pn)
                && !is_excluded(addr, exclude_ranges);

            if free && !in_range {
                range_start = addr;
                in_range = true;
            } else if !free && in_range {
                carve_range(zone, range_start, addr);
                in_range = false;
            }

            addr += PAGE_SIZE;
        }

        if in_range {
            carve_range(zone, range_start, reg_end);
        }
    }

    // Reserve BootInfo struct pages
    let bootinfo_end = bootinfo_base + (bootinfo_pages as u64 * PAGE_SIZE);
    for order in 0..=MAX_ORDER {
        remove_range(zone, bootinfo_base, bootinfo_end, order);
    }

    zone.total_pages
        .store(zone.free_pages.load(Ordering::Relaxed), Ordering::Relaxed);

    log::info!(
        "Physical allocator (buddy): {} pages free ({} MB), max order {}",
        zone.free_pages.load(Ordering::Relaxed),
        zone.free_pages.load(Ordering::Relaxed) as u64 * PAGE_SIZE / (1024 * 1024),
        MAX_ORDER
    );

    INITIALIZED.store(true, Ordering::Release);
}

pub unsafe fn reserve_range(start: u64, size_in_pages: usize) {
    // Hold the global lock so that a concurrent `alloc_order` / `free_order`
    // cannot insert a higher-order block into a range whose order has already
    // been processed.
    let _mpl = MULTI_PAGE_LOCK.lock();
    let zone = unsafe { &mut *ZONE.get() };
    let rstart = align_down(start, PAGE_SIZE);
    let rend = align_up(
        start.saturating_add((size_in_pages as u64) * PAGE_SIZE),
        PAGE_SIZE,
    );

    for order in 0..=MAX_ORDER {
        remove_range(zone, rstart, rend, order);
    }
}

pub fn alloc_order(order: usize) -> Option<u64> {
    if order > MAX_ORDER {
        return None;
    }

    let zone = unsafe { &mut *ZONE.get() };

    // Fast path: try target order
    {
        let _g = LOCKS[order].lock();
        if let Some(phys) = unsafe { pop_block(zone, order) } {
            // Reset refcount to 1 for freshly allocated pages.
            // Pages that were freed had their refcount decremented to 0.
            if order == 0 {
                let pn = phys / PAGE_SIZE;
                if (pn as usize) < REFCOUNT_MAX_PAGES {
                    REFCOUNTS[pn as usize].store(1, Ordering::Relaxed);
                }
            }
            return Some(phys);
        }
    }

    // Search upward for a larger block, then split
    for higher in (order + 1)..=MAX_ORDER {
        let phys = {
            let _g = LOCKS[higher].lock();
            unsafe { pop_block(zone, higher) }
        };
        if let Some(phys) = phys {
            // Split: add buddies at each intermediate order
            let mut current = phys;
            for cur_order in (order..higher).rev() {
                let half = block_size(cur_order);
                let _g = LOCKS[cur_order].lock();
                unsafe { add_block(zone, current, cur_order) };
                drop(_g);
                current += half;
            }
            // Reset refcount for the returned page
            if order == 0 {
                let pn = current / PAGE_SIZE;
                if (pn as usize) < REFCOUNT_MAX_PAGES {
                    REFCOUNTS[pn as usize].store(1, Ordering::Relaxed);
                }
            }
            return Some(current);
        }
    }

    log::error!("alloc_order({}): out of memory!", order);
    None
}

/// Increment the reference count for a physical page.
/// Used when a page is COW-shared between parent and child.
pub fn ref_page(pn: u64) {
    let idx = pn as usize;
    if idx < REFCOUNT_MAX_PAGES {
        // Saturating increment to prevent overflow
        let old = REFCOUNTS[idx].fetch_add(1, Ordering::Relaxed);
        if old == u16::MAX {
            REFCOUNTS[idx].store(u16::MAX, Ordering::Relaxed);
        }
    }
}

/// Decrement the reference count for a physical page.
/// Returns true if the refcount reached zero (caller should free the page).
pub fn unref_page(pn: u64) -> bool {
    let idx = pn as usize;
    if idx >= REFCOUNT_MAX_PAGES {
        return true; // Beyond tracked range, safe to free
    }
    let prev = REFCOUNTS[idx].fetch_sub(1, Ordering::AcqRel);
    prev == 1
}

/// Get the current reference count for a physical page.
pub fn refcount(pn: u64) -> u16 {
    let idx = pn as usize;
    if idx >= REFCOUNT_MAX_PAGES {
        return 1;
    }
    REFCOUNTS[idx].load(Ordering::Relaxed)
}

pub fn free_order(addr: u64, order: usize) {
    if order > MAX_ORDER || addr % PAGE_SIZE != 0 {
        return;
    }

    if is_reserved_page(addr / PAGE_SIZE) {
        return;
    }

    let zone = unsafe { &mut *ZONE.get() };

    if addr < zone.base || addr + block_size(order) > zone.top {
        return;
    }

    // For single pages (order 0), check the reference count.
    // A page with refcount > 1 is still COW-shared and must not be freed.
    if order == 0 {
        let pn = addr / PAGE_SIZE;
        if !unref_page(pn) {
            return; // refcount > 0, page is still referenced
        }
    }

    unsafe { coalesce(zone, addr, order) };
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
            if count > (1u64 << MAX_ORDER) {
                return None;
            }
            let order = if count <= 1 {
                0
            } else {
                let o = 64 - (count.wrapping_sub(1)).leading_zeros();
                (o as usize).min(MAX_ORDER)
            };
            let alloc_count = 1u64 << order;
            // Hold the multi-page lock across the alloc + excess-free
            // sequence to prevent a concurrent alloc_order from grabbing
            // pages that are being freed (Bug 1).
            let _mpl = MULTI_PAGE_LOCK.lock();
            alloc_order(order).map(|addr| {
                if alloc_count > count {
                    let excess = alloc_count - count;
                    // Free excess pages using the chunked helper so that
                    // each sub-range is freed atomically (minimises races).
                    unsafe { free_pages_chunked(addr + count * PAGE_SIZE, excess); }
                }
                addr
            })
        }
    }
}

/// Free a contiguous range `[addr, addr + count * PAGE_SIZE)` by processing
/// it in the largest possible power-of-2 chunks.  Each chunk is freed via
/// `free_order`, which is internally consistent (per-order lock held for
/// the duration of the coalesce at that order).  This is far less racy than
/// freeing individual pages and hoping the coalesce cross-order interleaving
/// works out, and it eliminates the caller-level TOCTOU window (Bug 3).
///
/// Must be called with `MULTI_PAGE_LOCK` held if count > 1.
unsafe fn free_pages_chunked(addr: u64, count: u64) {
    let end = addr + count * PAGE_SIZE;
    let mut current = addr;
    while current < end {
        let remaining = end - current;
        let pn = current / PAGE_SIZE;
        let align_zeros = pn.trailing_zeros() as usize;
        let max_pages = remaining / PAGE_SIZE;
        let max_order = if max_pages <= 1 {
            0
        } else {
            63 - (max_pages.wrapping_sub(1)).leading_zeros() as usize
        };
        let order = MAX_ORDER.min(align_zeros.min(max_order));
        free_order(current, order);
        current += block_size(order);
    }
}

pub fn free_pages(addr: u64, count: u64) {
    if count == 0 || is_reserved_page(addr / PAGE_SIZE) {
        return;
    }
    let _mpl = MULTI_PAGE_LOCK.lock();
    unsafe { free_pages_chunked(addr, count); }
}

pub fn free_pages_count() -> usize {
    unsafe { (*ZONE.get()).free_pages.load(Ordering::Relaxed) }
}

pub fn total_pages() -> usize {
    unsafe { (*ZONE.get()).total_pages.load(Ordering::Relaxed) }
}

/// Count free blocks at each allocator order. Returns [count; MAX_ORDER+1].
/// Each lock is held only briefly to walk the linked list.
pub fn free_counts_per_order() -> [usize; MAX_ORDER + 1] {
    let zone = unsafe { &*ZONE.get() };
    let mut counts = [0usize; MAX_ORDER + 1];
    for order in 0..=MAX_ORDER {
        let _g = LOCKS[order].lock();
        let mut cur = zone.free_lists[order];
        while !cur.is_null() {
            counts[order] += 1;
            cur = unsafe { (*cur).next };
        }
    }
    counts
}
