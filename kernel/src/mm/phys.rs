use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    let base = BOOTINFO_RESERVED_PN.load(Ordering::Acquire);
    let pages = BOOTINFO_RESERVED_PAGES.load(Ordering::Acquire);
    if base == usize::MAX || pages == 0 {
        return false;
    }
    let pn_usize = pn as usize;
    pn_usize >= base && pn_usize < base + pages
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
    //   remove buddy  →  (window closed)  →  add our block
    let _g = LOCKS[order].lock();
    if remove_block(zone, buddy, order) {
        // Buddy was found — recurse to the next order.
        // Release current lock before recursing (lock order: low→high).
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
unsafe fn remove_range(zone: &mut Zone, rstart: u64, rend: u64, order: usize) {
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

            // Re-add non-overlapping portions as smaller blocks
            if cphys < rstart {
                carve_range(zone, cphys, rstart);
            }
            if cend > rend {
                carve_range(zone, rend, cend);
            }
        } else {
            prev = curr;
        }

        curr = next;
    }
}

/// Check whether a physical page falls within any of the exclude ranges.
fn is_excluded(phys: u64, exclude_ranges: &[(u64, u64)]) -> bool {
    for &(start, size) in exclude_ranges {
        let end = start.saturating_add(size);
        if phys >= start && phys < end {
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
    exclude_ranges: &[(u64, u64)],
) {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
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
            return Some(current);
        }
    }

    log::error!("alloc_order({}): out of memory!", order);
    None
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
