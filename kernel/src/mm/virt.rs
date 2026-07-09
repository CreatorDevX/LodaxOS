use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lodaxos_system::MAX_CPUS;

use super::phys;
use crate::sync::IrqSaveSpinLock;

const PAGE_SIZE: u64 = 0x1000;
const PHYS_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const ENTRY_FLAG_MASK: u64 = NO_EXECUTE | 0xFFF;
const DIRECT_MAP_SIZE: u64 = 0x0000_8000_0000_0000;
const USER_IMAGE_BASE: u64 = 0x2000_0000;

// Page table entry flags
pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const CACHE_DISABLE: u64 = 1 << 4; // PCD -- force uncacheable for MMIO
pub const NO_EXECUTE: u64 = 1 << 63;

pub const DATA: u64 = PRESENT | WRITABLE | NO_EXECUTE;

/// Page-table flags for executable pages (code).
/// NOTE: NO_EXECUTE deliberately omitted so that instruction fetches succeed.
pub const CODE: u64 = PRESENT | WRITABLE;

/// Software-defined flag: Copy-On-Write. Stored in PTE bit 11 (available for
/// software use on x86-64). When set, a write fault on this page triggers a
/// copy instead of a permission violation.
pub const COW: u64 = 1 << 11;

/// Higher-half base: 0xFFFF_8000_0000_0000
pub const HIGHER_HALF: u64 = 0xFFFF_8000_0000_0000;

static PT_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Physical address of the kernel's PML4 (set once by `init`).
/// Accessed lock-free because it is written once during boot and
/// read-only thereafter.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);
static PAGE_SPLIT_LOCK: IrqSaveSpinLock<()> = IrqSaveSpinLock::new(());

/// Physical address of the katerm rescue PML4.
/// Created by `katerm_pml4_init()` after the kernel PML4 is ready.
pub static KATERM_PML4: AtomicU64 = AtomicU64::new(0);

/// Top of the katerm rescue stack (virtual address).
/// Set by `katerm_pml4_init()`. Used by `enter_rescue_mode()`.
pub static KATERM_STACK_TOP: AtomicU64 = AtomicU64::new(0);

/// Virtual address of the katerm rescue stack base.
const KATERM_STACK_VIRT: u64 = 0xFFFF_F000_0000_0000;

/// Return the kernel PML4 physical address.  Set during page-table init
/// and never changes.  All CPUs share the same kernel PML4 for the
/// higher-half mappings; per-task PML4s are forks of this one.
pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

/// Per-CPU pending flag for TLB shootdown -- the only synchronisation
/// mechanism now.  No lock is held while IPIs are in flight, so the
/// classic deadlock (CPU A holds lock and waits for CPU B's ACK while
/// CPU B tries to acquire the same lock) cannot occur (Bug 2 fix).
pub(crate) static TLB_FLUSH_ADDR: AtomicU64 = AtomicU64::new(0);
pub static TLB_ACK: [AtomicU64; MAX_CPUS] =
    [const { AtomicU64::new(0) }; MAX_CPUS];

/// Set a per-CPU pending-tlb-flush flag with overwrite detection (Bug 12).
fn set_pending_flush(cpu: usize, addr: u64) {
    let prev = crate::percpu::PERCPU[cpu].pending_tlb_flush
        .swap(addr, Ordering::SeqCst);
    if prev != 0 && prev != addr && prev != u64::MAX {
        crate::percpu::PERCPU[cpu].pending_tlb_flush
            .store(u64::MAX, Ordering::SeqCst);
    }
}

/// Broadcast a TLB flush to all other CPUs using per-CPU pending flags.
/// Does NOT hold any lock while IPIs are in flight (Bug 2 fix).
fn tlb_shootdown(flush_addr: u64) {
    let cpu = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    for i in 0..MAX_CPUS {
        if i != cpu && crate::percpu::is_online(i) {
            set_pending_flush(i, flush_addr);
        }
    }
    // Local flush.
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(flush_addr));
    // IPI tells remote CPUs to process their pending_tlb_flush in the
    // main interrupt entry path (see idt.rs timer/int handler).
    crate::arch::apic::send_ipi_others(crate::arch::idt::IPI_VECTOR);
}

pub fn flush_page_all(virt: u64) {
    tlb_shootdown(virt);
}

/// Broadcast a TLB shootdown for every 4 KB page in [start, end).
/// Used when a range of kernel PML4 entries are cleared (e.g., freeing
/// a driver's kernel stack) so that all CPUs invalidate their TLB caches.
pub fn tlb_shootdown_range(start: u64, end: u64) {
    let page_start = start & !(PAGE_SIZE - 1);
    let page_end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let cpu = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    // Set pending flush for the last page in the range on each remote CPU.
    // The IPI handler processes one pending flush per interrupt, so we set
    // the highest address; the lower addresses will naturally be invalidated
    // because the TLB is tagged by page.
    let last_page = if page_end > page_start { page_end - PAGE_SIZE } else { page_start };
    for i in 0..MAX_CPUS {
        if i != cpu && crate::percpu::is_online(i) {
            set_pending_flush(i, last_page);
        }
    }
    // Local flush for each page in the range.
    let mut addr = page_start;
    while addr < page_end {
        x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(addr));
        addr = addr.saturating_add(PAGE_SIZE);
    }
    // IPI tells remote CPUs to process their pending_tlb_flush.
    crate::arch::apic::send_ipi_others(crate::arch::idt::IPI_VECTOR);
}

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

impl PageTable {
    fn new_zeroed() -> Self {
        Self([0u64; 512])
    }
}

fn phys_to_virtual(addr: u64) -> *mut PageTable {
    if addr >= HIGHER_HALF {
        return addr as *mut PageTable;
    }

    let phys = addr & PHYS_ADDR_MASK;
    if phys >= DIRECT_MAP_SIZE {
        panic!(
            "phys_to_virtual: physical address {:#x} is outside higher-half direct map",
            addr
        );
    }
    (HIGHER_HALF + phys) as *mut PageTable
}

/// Check whether all 512 entries in a page table are zero (empty).
fn is_table_empty(table: *const PageTable) -> bool {
    unsafe {
        for i in 0..512 {
            if (*table).0[i] != 0 {
                return false;
            }
        }
        true
    }
}

/// Ensure `entry` points to a page table. Returns the physical address of
/// the table. If the entry is a huge page (PS bit set), splits it into
/// sub-entries so the caller can create finer-grained mappings.
///
/// `level` is the PML4 walk depth: 3=PML4, 2=PDP, 1=PD, 0=PT.
/// - level 2: 1GB huge page -> split into 512 × 2MB entries (PS set).
/// - level 1: 2MB huge page -> split into 512 × 4KB entries (PS clear).
fn ensure_table(entry: &mut u64, flags: u64, level: usize, addr: u64) -> u64 {
    // Loop until the entry is populated.  Use a CAS to atomically
    // write the new page-table page; if another thread beat us, we
    // free our redundant allocation (Bug 9 fix).
    //
    // BUG 1 fix: read via the atomic *first* so there is no TOCTOU
    // window between a non-atomic PRESENT check and the CAS loop.
    let entry_atomic = unsafe {
        &*(entry as *const u64 as *const core::sync::atomic::AtomicU64)
    };
    let cur = entry_atomic.load(Ordering::Acquire);
    if cur & PRESENT == 0 {
        let page = phys::alloc_page().expect("out of memory for page tables");
        let pt_virt = phys_to_virtual(page);
        unsafe { (*pt_virt) = PageTable::new_zeroed(); }
        loop {
            let cur = entry_atomic.load(Ordering::Relaxed);
            if cur & PRESENT != 0 {
                // Already set by another thread
                phys::free_page(page);
                break;
            }
            let new_val = page | flags | PRESENT;
            if entry_atomic
                .compare_exchange_weak(cur, new_val, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
    }

    // Split huge pages so the caller can map at a finer granularity.
    if *entry & (1 << 7) != 0 {
        let new_page = phys::alloc_page().expect("out of memory for page table split");
        let new_table = phys_to_virtual(new_page);
        unsafe { (*new_table) = PageTable::new_zeroed(); }

        // Only preserve flag bits from the original entry -- NX (bit 63) and
        // the lower 12 flag bits (excluding PRESENT and PS).  Do NOT carry
        // physical-address bits (51:12) into the sub-entries: those come
        // from `entry_phys` computed below.
        let orig_flags = (*entry & (1 << 63)) | ((*entry & 0xFFF) & !(1 | (1 << 7)));

        // Only propagate USER to child entries if the original huge page
        // already had USER set.  Blindly adding USER here causes a
        // privilege escalation: splitting a supervisor-only identity-map
        // huge page (e.g. at 0x20000000) would make all 512 resulting
        // 4KB pages user-accessible, even pages the ELF loader never
        // explicitly maps.  The caller (map_contiguous) writes explicit
        // PTEs with the correct flags for the pages it actually needs.
        let child_flags = if *entry & USER != 0 { flags & USER } else { 0 };

        match level {
            2 => {
                // 1 GB -> 512 × 2 MB entries (each with PS bit set).
                // In practice, callers pass level=2 for PML4 entries which
                // can never be huge, so this arm is dead code.
                //
                // BUG 5 fix: do NOT set PRESENT on sibling entries.  Only
                // the entries the caller explicitly maps should be accessible.
                // This prevents kernel-mode access to 511 unrelated pages.
                let base = *entry & 0x000F_FFC0_0000_0000; // 1 GB aligned (bits 51:30)
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x20_0000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | child_flags | (1 << 7);
                    }
                }
            }
            1 | 0 => {
                // 2 MB -> 512 × 4 KB entries (PS clear, normal PT entries).
                // Level 1 is for PDP entries (1 GB huge pages reached in
                // a 1 GB->2 MB split, though currently unused).  Level 0 is
                // for PD entries (2 MB huge pages encountered by map_page/
                // map_contiguous during heap allocation).
                //
                // BUG 5 fix: do NOT set PRESENT on sibling entries.  Only
                // the entries the caller explicitly maps should be accessible.
                // This prevents kernel-mode access to 511 unrelated pages.
                let base = *entry & 0x000F_FFFF_FE00_0000; // 2 MB aligned (bits 51:21)
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x1000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | child_flags;
                    }
                }
            }
            _ => panic!(
                "ensure_table: huge page at unexpected level {} (entry={:#x})",
                level, *entry
            ),
        }

        // Ensure the child-table writes are globally visible before
        // pointing the parent entry at the new table.  Without this,
        // the CPU's page-table walk could observe the new parent
        // pointer but stale zeroes in the child table, leading to
        // non-present page faults or protection violations.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }

        let lost_split_race = {
            let _guard = PAGE_SPLIT_LOCK.lock();
            if *entry & (1 << 7) == 0 {
                true
            } else {
                // Point the parent entry to the new table only after the table is
                // fully initialized, and serialize this publication against other
                // CPUs splitting the same shared higher-half mapping.
                *entry = new_page | flags | PRESENT;
                false
            }
        };

        if lost_split_race {
            phys::free_page(new_page);
            return *entry & PHYS_ADDR_MASK;
        }

        // Flush the stale huge-page entry from the local TLB; other CPUs
        // are handled by the caller (Bug 24 fix).
        let huge_shift = 12 + level * 9;
        let huge_base = addr & !((1u64 << huge_shift) - 1);
        x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(huge_base));
    }

    *entry & PHYS_ADDR_MASK
}

fn index_for_addr(virt: u64, level: usize) -> usize {
    ((virt >> (12 + level * 9)) & 0x1FF) as usize
}

pub unsafe fn init(regions: &[(u64, u64)], fb_phys: Option<(u64, u64)>, kernel_phys: Option<(u64, u64)>) {
    if PT_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    log::info!("Page tables: starting init, {} regions", regions.len());
    let pml4_page = phys::alloc_page().expect("out of memory for PML4");
    let pml4 = pml4_page as *mut PageTable;
    (*pml4) = PageTable::new_zeroed();
    log::trace!("Page tables: PML4 at phys={:#x}", pml4_page);

    let mut total_4kb: u64 = 0;
    let mut total_2mb: u64 = 0;

    // Map each free physical region into the higher-half (data, NX)
    // Use 2MB huge pages for aligned contiguous ranges to reduce boot time.
    for &(start, size) in regions {
        let mut addr = start;
        let region_end = start + size;

        // Leading 4KB pages (up to next 2MB boundary)
        let aligned = (addr + 0x1F_FFFF) & !0x1F_FFFF;
        while addr < aligned.min(region_end) {
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, DATA);
            total_4kb += 1;
            addr += PAGE_SIZE;
        }

        // 2MB huge pages
        while addr + 0x20_0000 <= region_end {
            id_map_huge_2mb(pml4_page, HIGHER_HALF + addr, addr, DATA);
            total_2mb += 1;
            addr += 0x20_0000;
        }

        // Trailing 4KB pages
        while addr < region_end {
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, DATA);
            total_4kb += 1;
            addr += PAGE_SIZE;
        }
    }
    log::debug!("Page tables: {} 4KB + {} 2MB pages mapped in higher-half", total_4kb, total_2mb);

    // Map the kernel image range in the higher-half so BSS statics
    // (IST stacks, per-CPU data, etc.) are accessible via HIGHER_HALF +
    // phys.  Without this, IST stack addresses in the TSS point to
    // unmapped pages and any interrupt causes an instant #PF.
    if let Some((kstart, ksize)) = kernel_phys {
        let mut addr = kstart;
        let region_end = kstart + ksize;
        let mut k4kb: u64 = 0;
        let mut k2mb: u64 = 0;

        // Leading 4KB pages (up to next 2MB boundary)
        let aligned = (addr + 0x1F_FFFF) & !0x1F_FFFF;
        while addr < aligned.min(region_end) {
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, CODE);
            k4kb += 1;
            addr += PAGE_SIZE;
        }
        // 2MB huge pages
        while addr + 0x20_0000 <= region_end {
            id_map_huge_2mb(pml4_page, HIGHER_HALF + addr, addr, CODE);
            k2mb += 1;
            addr += 0x20_0000;
        }
        // Trailing 4KB pages
        while addr < region_end {
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, CODE);
            k4kb += 1;
            addr += PAGE_SIZE;
        }
        log::debug!("Page tables: kernel image {} 4KB + {} 2MB pages mapped in higher-half", k4kb, k2mb);
    }

    // Identity-map all 4 GB (matching the bitmap range).  Build the page-table
    // hierarchy directly rather than calling id_map_huge_2mb per 2 MB page
    // (which would walk id_ensure_table O(N) -- ~4000 checks for 2048 pages).
    //   PML4[0] -> PDP[0..3] -> PD tables (512 entries each, 2 MB huge pages).
    let pdp_page = phys::alloc_page().expect("out of memory for identity map PDP");
    unsafe { *(pdp_page as *mut PageTable) = PageTable::new_zeroed(); }
    for pdp_idx in 0..4usize {
        let pd_page = phys::alloc_page().expect("out of memory for identity map PD");
        let pd = pd_page as *mut PageTable;
        unsafe { (*pd) = PageTable::new_zeroed(); }
        for entry in 0..512usize {
            let phys = (pdp_idx as u64) * 0x4000_0000 + (entry as u64) * 0x20_0000;
            // LAPIC (0xFEE00000) falls in the 0xFEC00000--0xFEE00000 2MB page.
            // Mark it cache-disable (PCD) to prevent CPU caching of MMIO registers.
            let flags = if phys <= 0xFEE0_0000 && 0xFEE0_0000 < phys + 0x20_0000 {
                WRITABLE | PRESENT | (1 << 7) | (1 << 4) // PCD
            } else {
                WRITABLE | PRESENT | (1 << 7)
            };
            unsafe { (*pd).0[entry] = phys | flags; }
        }
        unsafe { (*(pdp_page as *mut PageTable)).0[pdp_idx] = pd_page | WRITABLE | PRESENT; }
    }
    unsafe { (*pml4).0[0] = pdp_page | WRITABLE | PRESENT; }
    log::trace!("Page tables: identity-mapped 0..4 GB (2 MB huge pages, direct)");
    log::info!("Page tables: stage 1 complete (identity map done)");

    // Map the framebuffer in the higher-half (4 KB pages).
    // The identity map above also covers the framebuffer (2 MB pages), but
    // that is harmless: the CPU never executes from framebuffer addresses.
    if let Some((fb_base, fb_size)) = fb_phys {
        let num_pages = (fb_size + PAGE_SIZE - 1) / PAGE_SIZE;
        for p in 0..num_pages {
            let pa = fb_base + p * PAGE_SIZE;
            id_map_page(pml4_page, HIGHER_HALF + pa, pa, DATA);
        }
        log::trace!("Page tables: framebuffer mapped in higher-half ({} pages at {:#x})", num_pages, fb_base);
    }

    // Load new PML4
    log::info!("Page tables: about to load CR3 with phys={:#x}", pml4_page);
    unsafe {
        x86_64::registers::control::Cr3::write(
            x86_64::structures::paging::PhysFrame::containing_address(x86_64::PhysAddr::new(pml4_page)),
            x86_64::registers::control::Cr3Flags::empty(),
        );
    }
    log::info!("Page tables: CR3 loaded with phys={:#x}", pml4_page);
    log::info!("Page tables: post-CR3-switch check");

    KERNEL_PML4.store(pml4_page, Ordering::Release);
    PT_INITIALIZED.store(true, Ordering::Release);
}

/// Identity-mapped helpers for use during init (before CR3 switch).
/// Uses physical addresses directly since UEFI page tables identity-map all memory.
fn id_ensure_table(entry: &mut u64, flags: u64, level: usize) -> u64 {
    if *entry & PRESENT == 0 {
        let page = phys::alloc_page().expect("out of memory for page tables");
        let target = page as *mut PageTable;
        unsafe {
            (*target) = PageTable::new_zeroed();
        }
        *entry = page | flags | PRESENT;
    }

    if *entry & (1 << 7) != 0 {
        let orig_flags = (*entry & (1 << 63)) | ((*entry & 0xFFF) & !(1 | (1 << 7)));
        let new_page = phys::alloc_page().expect("out of memory for page table split");
        let new_table = new_page as *mut PageTable;
        unsafe { (*new_table) = PageTable::new_zeroed(); }

        match level {
            2 => {
                let base = *entry & 0x000F_FFC0_0000_0000;
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x20_0000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | PRESENT | (1 << 7);
                    }
                }
            }
            1 | 0 => {
                let base = *entry & 0x000F_FFFF_FE00_0000;
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x1000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | PRESENT;
                    }
                }
            }
            _ => panic!(
                "id_ensure_table: huge page at unexpected level {} (entry={:#x})",
                level, *entry
            ),
        }

        *entry = new_page | flags | PRESENT;
    }

    *entry & PHYS_ADDR_MASK
}

fn id_map_page(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    let pml4 = pml4_phys as *mut PageTable;
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_phys = id_ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, WRITABLE, 2);
    let pdp = pdp_phys as *mut PageTable;

    let pd_phys = id_ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, WRITABLE, 1);
    let pd = pd_phys as *mut PageTable;

    let pt_phys = id_ensure_table(unsafe { &mut (*pd).0[pd_idx] }, WRITABLE, 0);
    let pt = pt_phys as *mut PageTable;

    unsafe {
        (*pt).0[pt_idx] = phys | flags;
    }
}

/// Identity-mapped 2MB huge page (before CR3 switch).
fn id_map_huge_2mb(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    let pml4 = pml4_phys as *mut PageTable;
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);

    let pdp_phys = id_ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, WRITABLE, 2);
    let pdp = pdp_phys as *mut PageTable;

    let pd_phys = id_ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, WRITABLE, 1);
    let pd = pd_phys as *mut PageTable;

    unsafe {
        (*pd).0[pd_idx] = phys | flags | (1 << 7);
    }
}

fn map_page(pml4: *mut PageTable, virt: u64, phys: u64, flags: u64) {
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let table_flags = WRITABLE | (flags & USER);

    // Level 2: PDP->PD -- may hit 1 GB huge pages (from identity map).
    let pdp_phys = ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, table_flags, 2, virt);
    // Propagate USER to existing intermediate entry (identity map entries lack it)
    unsafe { (*pml4).0[pml4_idx] |= flags & USER; }
    let pdp = phys_to_virtual(pdp_phys);

    // Level 1: PD->PT -- may hit 2 MB huge pages (from identity map).
    let pd_phys = ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, table_flags, 1, virt);
    unsafe { (*pdp).0[pdp_idx] |= flags & USER; }
    let pd = phys_to_virtual(pd_phys);

    let pt_phys = ensure_table(unsafe { &mut (*pd).0[pd_idx] }, table_flags, 0, virt);
    // Propagate USER to PD entry -- existing entries from fork_pml4
    // may have U/S=0 (supervisor-only), causing #PF on user-mode access
    // even when leaf PTEs have USER set. (Bug fix)
    unsafe { (*pd).0[pd_idx] |= flags & USER; }
    let pt = phys_to_virtual(pt_phys);

    unsafe {
        (*pt).0[pt_idx] = phys | flags;
    }
}

/// Check whether the leaf PTE for `virt` in the given PML4 has the NX bit set.
/// Used by the ELF loader to assert that executable segments are mapped without NX.
pub fn is_nx(pml4: u64, virt: u64) -> bool {
    let pml4_ptr = phys_to_virtual(pml4 & PHYS_ADDR_MASK);
    let pml4_idx = index_for_addr(virt, 3);
    let e4 = unsafe { (*pml4_ptr).0[pml4_idx] };
    if e4 & PRESENT == 0 { return true; }
    if e4 & (1 << 7) != 0 { return e4 & NO_EXECUTE != 0; }

    let pdp = phys_to_virtual(e4 & PHYS_ADDR_MASK);
    let pdp_idx = index_for_addr(virt, 2);
    let e3 = unsafe { (*pdp).0[pdp_idx] };
    if e3 & PRESENT == 0 { return true; }
    if e3 & (1 << 7) != 0 { return e3 & NO_EXECUTE != 0; }

    let pd = phys_to_virtual(e3 & PHYS_ADDR_MASK);
    let pd_idx = index_for_addr(virt, 1);
    let e2 = unsafe { (*pd).0[pd_idx] };
    if e2 & PRESENT == 0 { return true; }
    if e2 & (1 << 7) != 0 { return e2 & NO_EXECUTE != 0; }

    let pt = phys_to_virtual(e2 & PHYS_ADDR_MASK);
    let pt_idx = index_for_addr(virt, 0);
    let e1 = unsafe { (*pt).0[pt_idx] };
    if e1 & PRESENT == 0 { return true; }
    e1 & NO_EXECUTE != 0
}

pub fn translate(virt: u64) -> Option<u64> {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return None;
    }

    let cr3 = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
    let pml4 = phys_to_virtual(cr3 & PHYS_ADDR_MASK);

    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 {
        return None;
    }
    if pdp_entry & (1 << 7) != 0 {
        // 1GB huge page
        let base = pdp_entry & 0x000F_FFC0_0000_0000;
        return Some(base | (virt & 0x3FFF_FFFF));
    }

    let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 {
        return None;
    }
    if pd_entry & (1 << 7) != 0 {
        // 2MB huge page
        let base = pd_entry & 0x000F_FFFF_FE00_0000;
        return Some(base | (virt & 0x1F_FFFF));
    }

    let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 {
        return None;
    }

    let pt = phys_to_virtual(pt_phys & PHYS_ADDR_MASK);
    let page_entry = unsafe { (*pt).0[pt_idx] };
    if page_entry & PRESENT == 0 {
        return None;
    }

    Some((page_entry & PHYS_ADDR_MASK) | (virt & 0xFFF))
}

pub fn unmap(virt: u64) {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    let cr3 = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
    let pml4 = phys_to_virtual(cr3 & PHYS_ADDR_MASK);

    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 {
        return;
    }
    if pdp_entry & (1 << 7) != 0 {
        unsafe { (*pml4).0[pml4_idx] = 0; }
        x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt));
        tlb_shootdown(virt);
        return;
    }

    let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 {
        return;
    }
    if pd_entry & (1 << 7) != 0 {
        unsafe { (*pdp).0[pdp_idx] = 0; }
        x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt));
        tlb_shootdown(virt);
        return;
    }

    let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
    let pt_entry = unsafe { (*pd).0[pd_idx] };
    if pt_entry & PRESENT == 0 {
        return;
    }

    let pt = phys_to_virtual(pt_entry & PHYS_ADDR_MASK);
    unsafe { (*pt).0[pt_idx] = 0; }

    // Collect empty intermediate page tables to free AFTER TLB shootdown.
    // Freeing them before shootdown risks a remote CPU seeing stale data
    // if it is mid-page-table-walk when the page is reused.
    let mut to_free: [u64; 3] = [0; 3];
    let mut free_count = 0usize;

    if is_table_empty(pt) {
        to_free[free_count] = pt_entry & PHYS_ADDR_MASK;
        free_count += 1;
        unsafe { (*pd).0[pd_idx] = 0; }

        let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
        if is_table_empty(pd) {
            to_free[free_count] = pd_entry & PHYS_ADDR_MASK;
            free_count += 1;
            unsafe { (*pdp).0[pdp_idx] = 0; }

            let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
            if is_table_empty(pdp) {
                to_free[free_count] = pdp_entry & PHYS_ADDR_MASK;
                free_count += 1;
                unsafe { (*pml4).0[pml4_idx] = 0; }
            }
        }
    }

    // Flush TLB locally.
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt));

    // Broadcast TLB shootdown IPI -- no lock held (Bug 2 fix).
    tlb_shootdown(virt);

    // NOW free the intermediate page tables after remote TLBs are flushed.
    for i in 0..free_count {
        if to_free[i] != 0 {
            phys::free_page(to_free[i]);
        }
    }
}

/// Split the 2 MB identity-map huge page covering `virt_addr` into 4 KB
/// pages, then clear the PTE for `virt_addr` alone.  This is used to
/// unmap the SIPI trampoline page (physical 0x8000) after all APs have
/// booted, preventing accidental execution of 16-bit trampoline code in
/// 64-bit long mode (which causes #UD).
///
/// The function walks PML4 → PDP → PD for the given virtual address.
/// If the PD entry is a 2 MB huge page, it allocates a new PT, copies
/// all 512 entries, replaces the PD entry, clears the target PTE, and
/// performs a TLB shootdown.
pub fn unmap_from_huge_page(virt_addr: u64) {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    let cr3 = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
    let pml4 = phys_to_virtual(cr3 & PHYS_ADDR_MASK);

    let pml4_idx = index_for_addr(virt_addr, 3);
    let pdp_idx = index_for_addr(virt_addr, 2);
    let pd_idx = index_for_addr(virt_addr, 1);
    let pt_idx = index_for_addr(virt_addr, 0);

    // PML4 → PDP
    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 {
        return;
    }
    if pdp_entry & (1 << 7) != 0 {
        // 1 GB huge page -- can't split, just clear it entirely.
        unsafe { (*pml4).0[pml4_idx] = 0; }
        tlb_flush_and_shootdown(virt_addr);
        return;
    }

    // PDP → PD
    let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 {
        return;
    }
    if pd_entry & (1 << 7) != 0 {
        // 2 MB huge page -- split into 512 × 4 KB pages.
        let huge_phys_base = pd_entry & PHYS_ADDR_MASK;
        let huge_flags = pd_entry & ENTRY_FLAG_MASK & !(1 << 7); // clear PS

        let pt_page = match phys::alloc_page() {
            Some(p) => p,
            None => {
                log::error!("virt: cannot split huge page at {:#x} -- OOM", virt_addr);
                return;
            }
        };
        unsafe {
            let pt = pt_page as *mut PageTable;
            *pt = PageTable::new_zeroed();
            for i in 0..512u64 {
                (*pt).0[i as usize] = (huge_phys_base + i * PAGE_SIZE) | huge_flags;
            }
            // Install the new PT in the PD, replacing the 2 MB huge page.
            (*pdp).0[pdp_idx] = pt_page | (pd_entry & ENTRY_FLAG_MASK) & !(1 << 7);
        }

        // Now clear just the target PTE.
        let pt = phys_to_virtual(pt_page);
        unsafe { (*pt).0[pt_idx as usize] = 0; }

        tlb_flush_and_shootdown(virt_addr);
        log::trace!("virt: split 2 MB page at {:#x} and unmapped PTE[{}]", virt_addr, pt_idx);
        return;
    }

    // Already a 4 KB page -- just clear it.
    let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
    let pt_entry = unsafe { (*pd).0[pd_idx] };
    if pt_entry & PRESENT == 0 {
        return;
    }
    let pt = phys_to_virtual(pt_entry & PHYS_ADDR_MASK);
    unsafe { (*pt).0[pt_idx as usize] = 0; }
    tlb_flush_and_shootdown(virt_addr);
}

/// Flush TLB locally and broadcast a shootdown IPI to all other CPUs.
fn tlb_flush_and_shootdown(virt_addr: u64) {
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt_addr));
    tlb_shootdown(virt_addr);
}

/// Physical address of the currently-loaded PML4 (reads CR3).
/// Prefer `kernel_pml4()` for the kernel's shared page table; use
/// this to detect which PML4 is currently loaded on this CPU.
#[inline]
pub fn current_pml4() -> u64 {
    x86_64::registers::control::Cr3::read().0.start_address().as_u64()
}

/// Backward-compat alias for `current_pml4()`.  Prefer `current_pml4()`
/// or `kernel_pml4()` for the kernel's shared page table.
#[inline]
pub fn pml4_address() -> u64 {
    current_pml4()
}

/// Switch the active PML4 (write CR3). The new PML4 must already be
/// populated -- this only updates the hardware register. The caller
/// is responsible for ensuring the new PML4 maps everything the new
/// context will need (kernel code, IDT handler code, stack, etc.).
#[inline]
pub fn switch_pml4(pml4_phys: u64) {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    unsafe {
        x86_64::registers::control::Cr3::write(
            x86_64::structures::paging::PhysFrame::containing_address(x86_64::PhysAddr::new(pml4_phys)),
            x86_64::registers::control::Cr3Flags::empty(),
        );
    }
}

// ---- PML4 deep-copy (for per-task address spaces) ----

/// Recursive helper: deep-copy a 4-level page table subtree.
/// `src_phys` is the physical address of the source table (PML4, PDP, PD, or PT).
/// `level` is 3 (PML4), 2 (PDP), 1 (PD), or 0 (PT).
/// When `cow` is true, writable leaf entries are copied as read-only with the
/// COW bit set, so a subsequent write triggers a copy-on-write fault.
/// Returns the physical address of the new copy, or `None` on OOM.
fn copy_table_recursive(src_phys: u64, level: usize, cow: bool) -> Option<u64> {
    let src = phys_to_virtual(src_phys) as *const PageTable;
    let new_phys = phys::alloc_page()?;
    let new = phys_to_virtual(new_phys);
    unsafe {
        (*new) = PageTable::new_zeroed();
    }
    for i in 0..512usize {
        let entry = unsafe { (*src).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if level == 0 {
            // PT level -- entries are leaves.
            let mut new_entry = entry;
            if cow && (entry & WRITABLE != 0) && (entry & USER != 0) {
                // Mark read-only with COW flag so a write faults and we can
                // copy the page before making it writable again.
                new_entry = (entry & !WRITABLE) | COW;
                // Increment the physical page's reference count since
                // both parent and child now share it via COW.
                let page_pn = (entry & PHYS_ADDR_MASK) / phys::PAGE_SIZE;
                phys::ref_page(page_pn);
            }
            unsafe { (*new).0[i] = new_entry };
        } else if entry & (1 << 7) != 0 {
            // 1GB (level 2) or 2MB (level 1) huge page -- leaf.
            let mut new_entry = entry;
            if cow && (entry & WRITABLE != 0) && (entry & USER != 0) {
                // COW huge pages: split on first write, handled by
                // ensure_table in the page fault path.
                new_entry = (entry & !WRITABLE) | COW;
                // For huge pages, the "page" is the entire huge page block.
                // Increment refcount for the base PFN.
                let page_pn = (entry & PHYS_ADDR_MASK) / phys::PAGE_SIZE;
                phys::ref_page(page_pn);
            }
            unsafe { (*new).0[i] = new_entry };
        } else {
            // Points to a sub-table -- recurse.
            let child_src = entry & PHYS_ADDR_MASK;
            let child_dst = copy_table_recursive(child_src, level - 1, cow)?;
            unsafe {
                (*new).0[i] = child_dst | (entry & ENTRY_FLAG_MASK);
            }
        }
    }
    Some(new_phys)
}

fn copy_kernel_low_identity(src: *const PageTable, dst: *mut PageTable) -> Option<()> {
    let pml4_entry = unsafe { (*src).0[0] };
    if pml4_entry & PRESENT == 0 || pml4_entry & (1 << 7) != 0 {
        return Some(());
    }

    let src_pdp = phys_to_virtual(pml4_entry & PHYS_ADDR_MASK);
    let pdp_entry = unsafe { (*src_pdp).0[0] };
    if pdp_entry & PRESENT == 0 || pdp_entry & (1 << 7) != 0 {
        return Some(());
    }

    let new_pdp_phys = phys::alloc_page()?;
    let new_pd_phys = match phys::alloc_page() {
        Some(page) => page,
        None => {
            phys::free_page(new_pdp_phys);
            return None;
        }
    };
    let new_pdp = phys_to_virtual(new_pdp_phys);
    let new_pd = phys_to_virtual(new_pd_phys);
    unsafe {
        (*new_pdp) = PageTable::new_zeroed();
        (*new_pd) = PageTable::new_zeroed();
    }

    let src_pd = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entries = (USER_IMAGE_BASE >> 21) as usize;
    for i in 0..pd_entries {
        let entry = unsafe { (*src_pd).0[i] };
        if entry & PRESENT != 0 && entry & (1 << 7) == 0 {
            let child_dst = copy_table_recursive(entry & PHYS_ADDR_MASK, 0, false)?;
            unsafe { (*new_pd).0[i] = child_dst | (entry & ENTRY_FLAG_MASK); }
        } else {
            unsafe { (*new_pd).0[i] = entry; }
        }
    }

    unsafe {
        (*new_pdp).0[0] = new_pd_phys | (pdp_entry & ENTRY_FLAG_MASK);
        (*dst).0[0] = new_pdp_phys | (pml4_entry & ENTRY_FLAG_MASK);
    }
    Some(())
}

/// Deep-copy the user half of a PML4 and return the physical address of the
/// new PML4. Kernel higher-half PML4 entries are shared with the source. The
/// kernel mutates those shared tables for direct-map splits, kernel stacks,
/// and MMIO mappings; recursively copying them gives each address space a
/// stale private kernel map and makes later forks walk a large mutable tree.
///
/// User-space (lower-half) writable pages are marked read-only with
/// the COW bit so that a write in the child (or parent) triggers a
/// copy-on-write fault. Kernel higher-half pages stay writable.
///
/// The caller can then modify the new PML4 (e.g. add ELF segments)
/// without affecting the source. When the new PML4 is no longer
/// needed, call `free_pml4`.
pub fn fork_pml4(src_phys: u64) -> Option<u64> {
    let src = phys_to_virtual(src_phys) as *const PageTable;
    let new_phys = phys::alloc_page()?;
    let new = phys_to_virtual(new_phys);
    unsafe { (*new) = PageTable::new_zeroed(); }

    let fork_from_kernel = src_phys == kernel_pml4();
    if fork_from_kernel {
        copy_kernel_low_identity(src, new)?;
    }

    for i in 0..512usize {
        if fork_from_kernel && i < 256 {
            continue;
        }

        let entry = unsafe { (*src).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if i >= 256 {
            unsafe { (*new).0[i] = entry };
            continue;
        }
        if entry & (1 << 7) != 0 {
            // Huge page below the user half -- copy as-is.
            unsafe { (*new).0[i] = entry };
            continue;
        }
        let child_src = entry & PHYS_ADDR_MASK;
        let child_dst = copy_table_recursive(child_src, 2, true)?;
        unsafe {
            (*new).0[i] = child_dst | (entry & ENTRY_FLAG_MASK);
        }
    }
    Some(new_phys)
}

/// Free a PML4 and all its sub-tables. Does NOT free the physical
/// pages the PML4 points to (those are owned by whoever mapped them).
/// Only frees the page-table structure pages themselves.
pub fn free_pml4(pml4_phys: u64) {
    let table = phys_to_virtual(pml4_phys) as *const PageTable;
    for i in 0..256usize {
        let entry = unsafe { (*table).0[i] };
        if entry & PRESENT != 0 && entry & (1 << 7) == 0 {
            free_table_recursive(entry & PHYS_ADDR_MASK, 2);
        }
    }
    phys::free_page(pml4_phys);
    // Invalidate the TLB to remove any cached translations from this
    // PML4 (Bug 10 fix).  Since the page-table pages are now freed and
    // may be re-allocated, stale TLB entries would allow reading/writing
    // the new data through the old virtual addresses.
    if pml4_phys == current_pml4() {
        let cr3 = x86_64::registers::control::Cr3::read();
        unsafe { x86_64::registers::control::Cr3::write(cr3.0, cr3.1); }
    }
}

fn free_table_recursive(table_phys: u64, level: usize) {
    let table = phys_to_virtual(table_phys) as *const PageTable;
    for i in 0..512usize {
        let entry = unsafe { (*table).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if level > 0 && entry & (1 << 7) == 0 {
            // Points to a sub-table -- recurse.
            let child_phys = entry & PHYS_ADDR_MASK;
            free_table_recursive(child_phys, level - 1);
        }
    }
    phys::free_page(table_phys);
}

/// Map `num_pages` physical pages (allocated individually) to a contiguous
/// virtual range starting at `virt_start`. Returns the number of pages
/// successfully mapped.
pub fn map_pages_from_phys(
    pml4_phys: u64,
    virt_start: u64,
    phys_pages: &[u64],
    flags: u64,
) {
    let pml4 = phys_to_virtual(pml4_phys);
    for (i, &phys_page) in phys_pages.iter().enumerate() {
        let virt = virt_start + (i as u64) * PAGE_SIZE;
        map_page(pml4, virt, phys_page, flags);
    }
}

/// Map a contiguous physical range to a contiguous virtual range.
/// Walks the page-table hierarchy once per PT table (2 MB) rather than
/// once per 4 KB page -- O(N/512) instead of O(N).
pub unsafe fn map_contiguous(
    pml4_phys: u64,
    virt_start: u64,
    phys_start: u64,
    num_pages: u64,
    flags: u64,
) {
    if num_pages == 0 {
        return;
    }
    let map_len = num_pages
        .checked_mul(PAGE_SIZE)
        .expect("map_contiguous: page count overflows byte length");
    virt_start
        .checked_add(map_len - PAGE_SIZE)
        .expect("map_contiguous: virtual range overflows u64");
    phys_start
        .checked_add(map_len - PAGE_SIZE)
        .expect("map_contiguous: physical range overflows u64");

    let pml4 = phys_to_virtual(pml4_phys);
    let mut remaining = num_pages;
    let mut virt = virt_start;
    let mut phys = phys_start;

    while remaining > 0 {
        let pt_idx = index_for_addr(virt, 0);
        let pt_remaining = 512 - pt_idx as u64;
        let batch = pt_remaining.min(remaining);

        let pml4_idx = index_for_addr(virt, 3);
        let pdp_idx = index_for_addr(virt, 2);
        let pd_idx = index_for_addr(virt, 1);

        let table_flags = WRITABLE | (flags & USER);
        let pdp_phys = ensure_table(&mut (*pml4).0[pml4_idx], table_flags, 2, virt);
        // Propagate USER to PML4 entry -- the CPU checks USER at every level
        // of the page-table walk, so PML4 must also have USER for user pages.
        (*pml4).0[pml4_idx] |= flags & USER;
        log::trace!("map_contiguous: PML4[{}] |= USER -> flags={:#x}", pml4_idx, (*pml4).0[pml4_idx] & 0xFFF);
        let pdp = phys_to_virtual(pdp_phys);
        let pd_phys = ensure_table(&mut (*pdp).0[pdp_idx], table_flags, 1, virt);
        (*pdp).0[pdp_idx] |= flags & USER;
        log::trace!("map_contiguous: PDP[{}] |= USER -> flags={:#x}", pdp_idx, (*pdp).0[pdp_idx] & 0xFFF);
        let pd = phys_to_virtual(pd_phys);
        let pt_phys = ensure_table(&mut (*pd).0[pd_idx], table_flags, 0, virt);
        // Propagate USER to PD entry -- same rationale as map_page.
        (*pd).0[pd_idx] |= flags & USER;
        let pt = phys_to_virtual(pt_phys);

        for i in 0..batch {
            (*pt).0[pt_idx as usize + i as usize] = (phys + i * PAGE_SIZE) | flags;
        }

        log::trace!(
            "map_contiguous: PT pml4_idx={} pdp_idx={} pd_idx={} pt_idx={} batch={} flags={:#x} leaf_flags={:#x}",
            pml4_idx, pdp_idx, pd_idx, pt_idx, batch, (*pml4).0[pml4_idx] & 0xFFF, (*pt).0[pt_idx] & 0xFFF
        );

        remaining -= batch;
        let step = batch * PAGE_SIZE;
        virt = virt.checked_add(step).expect("map_contiguous: virtual cursor overflow");
        phys = phys.checked_add(step).expect("map_contiguous: physical cursor overflow");
    }
}

/// Identity-map a physical range AND map it in the higher-half.
pub fn map_region(pml4_phys: u64, phys_start: u64, size: u64, flags: u64) {
    let pml4 = phys_to_virtual(pml4_phys);
    let num_pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    for p in 0..num_pages {
        let pa = phys_start + p * PAGE_SIZE;
        // Identity map
        map_page(pml4, pa, pa, flags);
        // Higher-half map
        map_page(pml4, HIGHER_HALF + pa, pa, flags);
    }
}

/// Map a physical range into the higher-half only (no identity map).
/// Used for MMIO regions where the identity map already exists via 2MB huge
/// pages and creating 4KB mappings at the same PD level would conflict.
pub fn map_region_higher_half(pml4_phys: u64, phys_start: u64, size: u64, flags: u64) {
    let pml4 = phys_to_virtual(pml4_phys);
    let num_pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    for p in 0..num_pages {
        let pa = phys_start + p * PAGE_SIZE;
        map_page(pml4, HIGHER_HALF + pa, pa, flags);
    }
}

/// Read the PTE for a given virtual address in a given PML4.
/// Returns `None` if the page is not mapped or if the walk encounters
/// a huge page (caller must handle huge-page COW separately).
pub fn read_pte(pml4_phys: u64, virt: u64) -> Option<u64> {
    let pml4 = phys_to_virtual(pml4_phys & PHYS_ADDR_MASK);
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 { return None; }
    if pdp_entry & (1 << 7) != 0 { return None; } // 1GB huge page

    let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 { return None; }
    if pd_entry & (1 << 7) != 0 { return None; } // 2MB huge page

    let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 { return None; }

    let pt = phys_to_virtual(pt_phys & PHYS_ADDR_MASK);
    Some(unsafe { (*pt).0[pt_idx] })
}

/// Update a PTE for a given virtual address in a given PML4.
/// Returns `None` if the page is not mapped at 4KB level.
pub fn write_pte(pml4_phys: u64, virt: u64, new_pte: u64) -> Option<()> {
    let pml4 = phys_to_virtual(pml4_phys & PHYS_ADDR_MASK);
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 { return None; }
    if pdp_entry & (1 << 7) != 0 { return None; }

    let pdp = phys_to_virtual(pdp_entry & PHYS_ADDR_MASK);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 { return None; }
    if pd_entry & (1 << 7) != 0 { return None; }

    let pd = phys_to_virtual(pd_entry & PHYS_ADDR_MASK);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 { return None; }

    let pt = phys_to_virtual(pt_phys & PHYS_ADDR_MASK);
    unsafe { (*pt).0[pt_idx] = new_pte; }
    // Flush the local TLB for this page.  Remote TLB invalidation is
    // the caller's responsibility (Bug 11 note: `write_pte` is called
    // from COW resolution which also does a local `invlpg`, but callers
    // operating on shared PML4s must also broadcast a shootdown IPI).
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt));
    Some(())
}

/// Explicitly map a single 4KB page (public wrapper).
pub fn map_page_explicit(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    // Check if this virtual address already has a present mapping (e.g. for
    // COW replacement).  Only do a TLB shootdown if there was a prior entry
    // that other CPUs may have cached.
    let old_pte = read_pte(pml4_phys, virt);
    let was_present = old_pte.is_some_and(|p| p & PRESENT != 0);

    let pml4 = phys_to_virtual(pml4_phys);
    map_page(pml4, virt, phys, flags);

    if was_present {
        // TLB shootdown -- no lock held, safe from any context (Bug 2).
        tlb_shootdown(virt);
    }
}

// ---- katerm rescue PML4 ----

/// Create a rescue PML4 for katerm. Deep-copies the kernel's higher-half
/// page table entries (preserving huge pages) and the identity map.
/// Also allocates guard pages + stack page.
/// Returns the physical address of the new PML4.
pub fn katerm_pml4_init(kernel_pml4_phys: u64) -> u64 {
    let src = phys_to_virtual(kernel_pml4_phys) as *const PageTable;
    let new_pml4_page = phys::alloc_page().expect("out of memory for katerm PML4");
    let new_pml4 = phys_to_virtual(new_pml4_page) as *mut PageTable;
    unsafe { (*new_pml4) = PageTable::new_zeroed(); }

    // Copy higher-half entries (PML4 indices 256-511): copy PML4 entry
    // as-is (pointer to a new PDP), then deep-copy the PDP and below.
    for i in 256..512usize {
        let entry = unsafe { (*src).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        // Allocate a new PDP and copy all entries.
        let src_pdp_phys = entry & PHYS_ADDR_MASK;
        let src_pdp = phys_to_virtual(src_pdp_phys) as *const PageTable;
        let new_pdp_page = phys::alloc_page().expect("out of memory for katerm PDP");
        let new_pdp = phys_to_virtual(new_pdp_page) as *mut PageTable;
        unsafe { (*new_pdp) = PageTable::new_zeroed(); }

        for j in 0..512usize {
            let pdp_entry = unsafe { (*src_pdp).0[j] };
            if pdp_entry & PRESENT == 0 {
                continue;
            }
            if pdp_entry & (1 << 7) != 0 {
                // 1GB huge page -- copy as-is.
                unsafe { (*new_pdp).0[j] = pdp_entry; }
                continue;
            }
            // Points to a PD -- allocate new PD, copy entries.
            let src_pd_phys = pdp_entry & PHYS_ADDR_MASK;
            let src_pd = phys_to_virtual(src_pd_phys) as *const PageTable;
            let new_pd_page = phys::alloc_page().expect("out of memory for katerm PD");
            let new_pd = phys_to_virtual(new_pd_page) as *mut PageTable;
            unsafe { (*new_pd) = PageTable::new_zeroed(); }

            for k in 0..512usize {
                let pd_entry = unsafe { (*src_pd).0[k] };
                if pd_entry & PRESENT == 0 {
                    continue;
                }
                if pd_entry & (1 << 7) != 0 {
                    // 2MB huge page -- copy as-is.
                    unsafe { (*new_pd).0[k] = pd_entry; }
                    continue;
                }
                // Points to a PT -- allocate new PT, copy entries.
                let src_pt_phys = pd_entry & PHYS_ADDR_MASK;
                let src_pt = phys_to_virtual(src_pt_phys) as *const PageTable;
                let new_pt_page = phys::alloc_page().expect("out of memory for katerm PT");
                let new_pt = phys_to_virtual(new_pt_page) as *mut PageTable;
                unsafe {
                    (*new_pt) = PageTable::new_zeroed();
                    for l in 0..512usize {
                        (*new_pt).0[l] = (*src_pt).0[l];
                    }
                    (*new_pd).0[k] = new_pt_page | (pd_entry & ENTRY_FLAG_MASK);
                }
            }

            unsafe { (*new_pdp).0[j] = new_pd_page | (pdp_entry & ENTRY_FLAG_MASK); }
        }

        unsafe { (*new_pml4).0[i] = new_pdp_page | (entry & ENTRY_FLAG_MASK); }
    }

    // Copy identity map (PML4 index 0): 4 PDP entries → 4 PD pages × 512 = 2048
    // 2MB huge page entries. No PT-level copies needed.
    let src_pdp_entry = unsafe { (*src).0[0] };
    if src_pdp_entry & PRESENT != 0 && src_pdp_entry & (1 << 7) == 0 {
        let src_pdp = phys_to_virtual(src_pdp_entry & PHYS_ADDR_MASK) as *const PageTable;
        let new_id_pdp_page = phys::alloc_page().expect("out of memory for katerm identity PDP");
        let new_id_pdp = phys_to_virtual(new_id_pdp_page) as *mut PageTable;
        unsafe { (*new_id_pdp) = PageTable::new_zeroed(); }

        for pdp_idx in 0..4usize {
            let pdp_entry = unsafe { (*src_pdp).0[pdp_idx] };
            if pdp_entry & PRESENT == 0 || pdp_entry & (1 << 7) != 0 {
                continue;
            }
            // Copy PD entries (all 2MB huge pages for identity map).
            let src_pd_phys = pdp_entry & PHYS_ADDR_MASK;
            let src_pd = phys_to_virtual(src_pd_phys) as *const PageTable;
            let new_pd_page = phys::alloc_page().expect("out of memory for katerm identity PD");
            let new_pd = phys_to_virtual(new_pd_page) as *mut PageTable;
            unsafe {
                (*new_pd) = PageTable::new_zeroed();
                for k in 0..512usize {
                    (*new_pd).0[k] = (*src_pd).0[k];
                }
                (*new_id_pdp).0[pdp_idx] = new_pd_page | (pdp_entry & ENTRY_FLAG_MASK);
            }
        }

        unsafe { (*new_pml4).0[0] = new_id_pdp_page | (src_pdp_entry & ENTRY_FLAG_MASK); }
    }

    // Allocate katerm stack: guard (NP) + stack (RW) + guard (NP) = 3 pages.
    // Only the middle page is mapped. Guard pages are left absent.
    let stack_page = phys::alloc_page().expect("out of memory for katerm stack");
    // Map the stack page at KATERM_STACK_VIRT.
    let stack_flags = PRESENT | WRITABLE;
    map_page(new_pml4, KATERM_STACK_VIRT, stack_page, stack_flags);

    let stack_top = KATERM_STACK_VIRT + PAGE_SIZE;
    KATERM_STACK_TOP.store(stack_top, Ordering::Release);

    log::info!("katerm: rescue PML4 at phys={:#x}, stack at {:#x}", new_pml4_page, KATERM_STACK_VIRT);

    new_pml4_page
}
