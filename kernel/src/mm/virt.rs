use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lodaxos_system::MAX_CPUS;

use super::phys;

const PAGE_SIZE: u64 = 0x1000;

// Page table entry flags
pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const CACHE_DISABLE: u64 = 1 << 4; // PCD — force uncacheable for MMIO
pub const NO_EXECUTE: u64 = 1 << 63;

pub const DATA: u64 = PRESENT | WRITABLE | NO_EXECUTE;

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

/// Return the kernel PML4 physical address.  Set during page-table init
/// and never changes.  All CPUs share the same kernel PML4 for the
/// higher-half mappings; per-task PML4s are forks of this one.
pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

/// Per-CPU pending flag for TLB shootdown — the only synchronisation
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

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

impl PageTable {
    fn new_zeroed() -> Self {
        Self([0u64; 512])
    }
}

fn phys_to_virtual(phys: u64) -> *mut PageTable {
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
/// - level 2: 1GB huge page → split into 512 × 2MB entries (PS set).
/// - level 1: 2MB huge page → split into 512 × 4KB entries (PS clear).
fn ensure_table(entry: &mut u64, flags: u64, level: usize) -> u64 {
    // Loop until the entry is populated.  Use a CAS to atomically
    // write the new page-table page; if another thread beat us, we
    // free our redundant allocation (Bug 9 fix).
    if *entry & PRESENT == 0 {
        let page = phys::alloc_page().expect("out of memory for page tables");
        let entry_atomic = unsafe {
            &*(entry as *const u64 as *const core::sync::atomic::AtomicU64)
        };
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
                // We won — zero out the page before using it.
                let pt_virt = phys_to_virtual(page);
                unsafe { (*pt_virt) = PageTable::new_zeroed(); }
                break;
            }
        }
    }

    // Split huge pages so the caller can map at a finer granularity.
    if *entry & (1 << 7) != 0 {
        // Only preserve flag bits from the original entry — NX (bit 63) and
        // the lower 12 flag bits (excluding PRESENT and PS).  Do NOT carry
        // physical-address bits (51:12) into the sub-entries: those come
        // from `entry_phys` computed below.
        let orig_flags = (*entry & (1 << 63)) | ((*entry & 0xFFF) & !(1 | (1 << 7)));

        let new_page = phys::alloc_page().expect("out of memory for page table split");
        let new_table = phys_to_virtual(new_page);
        unsafe { (*new_table) = PageTable::new_zeroed(); }

        // Propagate caller's flags (especially USER) to child entries.
        // The CPU checks USER at every level of the page-table walk, so
        // child entries must inherit the caller's permission flags.
        let child_flags = flags & USER;

        match level {
            2 => {
                // 1 GB → 512 × 2 MB entries (each with PS bit set).
                // In practice, callers pass level=2 for PML4 entries which
                // can never be huge, so this arm is dead code.
                let base = *entry & 0x000F_FFC0_0000_0000; // 1 GB aligned (bits 51:30)
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x20_0000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | child_flags | PRESENT | (1 << 7);
                    }
                }
            }
            1 | 0 => {
                // 2 MB → 512 × 4 KB entries (PS clear, normal PT entries).
                // Level 1 is for PDP entries (1 GB huge pages reached in
                // a 1 GB→2 MB split, though currently unused).  Level 0 is
                // for PD entries (2 MB huge pages encountered by map_page/
                // map_contiguous during heap allocation).
                let base = *entry & 0x000F_FFFF_FE00_0000; // 2 MB aligned (bits 51:21)
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x1000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | child_flags | PRESENT;
                    }
                }
            }
            _ => panic!(
                "ensure_table: huge page at unexpected level {} (entry={:#x})",
                level, *entry
            ),
        }

        // Point the parent entry to the new table (preserve caller's flags).
        *entry = new_page | flags | PRESENT;
    }

    *entry & !0xFFF
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
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, DATA);
            k4kb += 1;
            addr += PAGE_SIZE;
        }
        // 2MB huge pages
        while addr + 0x20_0000 <= region_end {
            id_map_huge_2mb(pml4_page, HIGHER_HALF + addr, addr, DATA);
            k2mb += 1;
            addr += 0x20_0000;
        }
        // Trailing 4KB pages
        while addr < region_end {
            id_map_page(pml4_page, HIGHER_HALF + addr, addr, DATA);
            k4kb += 1;
            addr += PAGE_SIZE;
        }
        log::debug!("Page tables: kernel image {} 4KB + {} 2MB pages mapped in higher-half", k4kb, k2mb);
    }

    // Identity-map all 4 GB (matching the bitmap range).  Build the page-table
    // hierarchy directly rather than calling id_map_huge_2mb per 2 MB page
    // (which would walk id_ensure_table O(N) — ~4000 checks for 2048 pages).
    //   PML4[0] → PDP[0..3] → PD tables (512 entries each, 2 MB huge pages).
    let pdp_page = phys::alloc_page().expect("out of memory for identity map PDP");
    unsafe { *(pdp_page as *mut PageTable) = PageTable::new_zeroed(); }
    for pdp_idx in 0..4usize {
        let pd_page = phys::alloc_page().expect("out of memory for identity map PD");
        let pd = pd_page as *mut PageTable;
        unsafe { (*pd) = PageTable::new_zeroed(); }
        for entry in 0..512usize {
            let phys = (pdp_idx as u64) * 0x4000_0000 + (entry as u64) * 0x20_0000;
            // LAPIC (0xFEE00000) falls in the 0xFEC00000–0xFEE00000 2MB page.
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

    *entry & !0xFFF
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

    // Level 2: PDP→PD — may hit 1 GB huge pages (from identity map).
    let pdp_phys = ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, table_flags, 2);
    // Propagate USER to existing intermediate entry (identity map entries lack it)
    unsafe { (*pml4).0[pml4_idx] |= flags & USER; }
    let pdp = phys_to_virtual(pdp_phys);

    // Level 1: PD→PT — may hit 2 MB huge pages (from identity map).
    let pd_phys = ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, table_flags, 1);
    unsafe { (*pdp).0[pdp_idx] |= flags & USER; }
    let pd = phys_to_virtual(pd_phys);

    let pt_phys = ensure_table(unsafe { &mut (*pd).0[pd_idx] }, table_flags, 0);
    // Propagate USER to PD entry — existing entries from fork_pml4
    // may have U/S=0 (supervisor-only), causing #PF on user-mode access
    // even when leaf PTEs have USER set. (Bug fix)
    unsafe { (*pd).0[pd_idx] |= flags & USER; }
    let pt = phys_to_virtual(pt_phys);

    unsafe {
        (*pt).0[pt_idx] = phys | flags;
    }
}

pub fn translate(virt: u64) -> Option<u64> {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return None;
    }

    let cr3 = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
    let pml4 = phys_to_virtual(cr3 & !0xFFF);

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

    let pdp = phys_to_virtual(pdp_entry & !0xFFF);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 {
        return None;
    }
    if pd_entry & (1 << 7) != 0 {
        // 2MB huge page
        let base = pd_entry & 0x000F_FFFF_FE00_0000;
        return Some(base | (virt & 0x1F_FFFF));
    }

    let pd = phys_to_virtual(pd_entry & !0xFFF);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 {
        return None;
    }

    let pt = phys_to_virtual(pt_phys & !0xFFF);
    let page_entry = unsafe { (*pt).0[pt_idx] };
    if page_entry & PRESENT == 0 {
        return None;
    }

    Some((page_entry & 0x000F_FFFF_FFFF_F000) | (virt & 0xFFF))
}

pub fn unmap(virt: u64) {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    let cr3 = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
    let pml4 = phys_to_virtual(cr3 & !0xFFF);

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
        return;
    }

    let pdp = phys_to_virtual(pdp_entry & !0xFFF);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 {
        return;
    }
    if pd_entry & (1 << 7) != 0 {
        unsafe { (*pdp).0[pdp_idx] = 0; }
        return;
    }

    let pd = phys_to_virtual(pd_entry & !0xFFF);
    let pt_entry = unsafe { (*pd).0[pd_idx] };
    if pt_entry & PRESENT == 0 {
        return;
    }

    let pt = phys_to_virtual(pt_entry & !0xFFF);
    unsafe { (*pt).0[pt_idx] = 0; }

    // Free empty intermediate page tables.  Walk back up the tree:
    // if a table becomes all-zeros after removing this entry, the
    // table itself is freed and the parent entry is zeroed.
    if is_table_empty(pt) {
        let pt_phys = pt_entry & !0xFFF;
        phys::free_page(pt_phys);
        unsafe { (*pd).0[pd_idx] = 0; }

        let pd = phys_to_virtual(pd_entry & !0xFFF);
        if is_table_empty(pd) {
            let pd_phys = pd_entry & !0xFFF;
            phys::free_page(pd_phys);
            unsafe { (*pdp).0[pdp_idx] = 0; }

            let pdp = phys_to_virtual(pdp_entry & !0xFFF);
            if is_table_empty(pdp) {
                let pdp_phys = pdp_entry & !0xFFF;
                phys::free_page(pdp_phys);
                unsafe { (*pml4).0[pml4_idx] = 0; }
            }
        }
    }

    // Flush TLB locally.
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt));

    // Broadcast TLB shootdown IPI — no lock held (Bug 2 fix).
    tlb_shootdown(virt);
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
/// populated — this only updates the hardware register. The caller
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
    let src = src_phys as *const PageTable;
    let new_phys = phys::alloc_page()?;
    let new = new_phys as *mut PageTable;
    unsafe {
        (*new) = PageTable::new_zeroed();
    }
    for i in 0..512usize {
        let entry = unsafe { (*src).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if level == 0 {
            // PT level — entries are leaves.
            let mut new_entry = entry;
            if cow && (entry & WRITABLE != 0) && (entry & USER != 0) {
                // Mark read-only with COW flag so a write faults and we can
                // copy the page before making it writable again.
                new_entry = (entry & !WRITABLE) | COW;
            }
            unsafe { (*new).0[i] = new_entry };
        } else if entry & (1 << 7) != 0 {
            // 1GB (level 2) or 2MB (level 1) huge page — leaf.
            let mut new_entry = entry;
            if cow && (entry & WRITABLE != 0) && (entry & USER != 0) {
                // COW huge pages: split on first write, handled by
                // ensure_table in the page fault path.
                new_entry = (entry & !WRITABLE) | COW;
            }
            unsafe { (*new).0[i] = new_entry };
        } else {
            // Points to a sub-table — recurse.
            let child_src = entry & !0xFFF;
            let child_dst = copy_table_recursive(child_src, level - 1, cow)?;
            unsafe {
                (*new).0[i] = child_dst | (entry & 0xFFF);
            }
        }
    }
    Some(new_phys)
}

/// Deep-copy a PML4 (the entire 4-level page-table hierarchy) and
/// return the physical address of the new PML4. All page-table
/// pages are freshly allocated from the physical allocator. The
/// physical pages they point to (kernel code/data, MMIO, etc.) are
/// **shared** with the source PML4 — only the table structure is
/// copied. To map a new physical page in the new PML4, call
/// `map_page_explicit` with the new PML4's physical address.
///
/// User-space (lower-half) writable pages are marked read-only with
/// the COW bit so that a write in the child (or parent) triggers a
/// copy-on-write fault. Kernel higher-half pages stay writable.
///
/// The caller can then modify the new PML4 (e.g. add ELF segments)
/// without affecting the source. When the new PML4 is no longer
/// needed, call `free_pml4`.
pub fn fork_pml4(src_phys: u64) -> Option<u64> {
    let src = src_phys as *const PageTable;
    let new_phys = phys::alloc_page()?;
    let new = new_phys as *mut PageTable;
    unsafe { (*new) = PageTable::new_zeroed(); }

    for i in 0..512usize {
        let entry = unsafe { (*src).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if entry & (1 << 7) != 0 {
            // Huge page at PML4 level (1GB) — copy as-is (kernel maps only).
            unsafe { (*new).0[i] = entry };
            continue;
        }
        // PML4 entries 0..255 = user half, 256..511 = kernel half.
        let cow = i < 256;
        let child_src = entry & !0xFFF;
        let child_dst = copy_table_recursive(child_src, 2, cow)?;
        unsafe {
            (*new).0[i] = child_dst | (entry & 0xFFF);
        }
    }
    Some(new_phys)
}

/// Free a PML4 and all its sub-tables. Does NOT free the physical
/// pages the PML4 points to (those are owned by whoever mapped them).
/// Only frees the page-table structure pages themselves.
pub fn free_pml4(pml4_phys: u64) {
    free_table_recursive(pml4_phys, 3);
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
    let table = table_phys as *const PageTable;
    for i in 0..512usize {
        let entry = unsafe { (*table).0[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        if level > 0 && entry & (1 << 7) == 0 {
            // Points to a sub-table — recurse.
            let child_phys = entry & !0xFFF;
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
/// once per 4 KB page — O(N/512) instead of O(N).
pub unsafe fn map_contiguous(
    pml4_phys: u64,
    virt_start: u64,
    phys_start: u64,
    num_pages: u64,
    flags: u64,
) {
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
        let pdp_phys = ensure_table(&mut (*pml4).0[pml4_idx], table_flags, 2);
        // Propagate USER to PML4 entry — the CPU checks USER at every level
        // of the page-table walk, so PML4 must also have USER for user pages.
        (*pml4).0[pml4_idx] |= flags & USER;
        log::trace!("map_contiguous: PML4[{}] |= USER → flags={:#x}", pml4_idx, (*pml4).0[pml4_idx] & 0xFFF);
        let pdp = phys_to_virtual(pdp_phys);
        let pd_phys = ensure_table(&mut (*pdp).0[pdp_idx], table_flags, 1);
        (*pdp).0[pdp_idx] |= flags & USER;
        log::trace!("map_contiguous: PDP[{}] |= USER → flags={:#x}", pdp_idx, (*pdp).0[pdp_idx] & 0xFFF);
        let pd = phys_to_virtual(pd_phys);
        let pt_phys = ensure_table(&mut (*pd).0[pd_idx], table_flags, 0);
        // Propagate USER to PD entry — same rationale as map_page.
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
        virt += batch * PAGE_SIZE;
        phys += batch * PAGE_SIZE;
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
    let pml4 = phys_to_virtual(pml4_phys & !0xFFF);
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 { return None; }
    if pdp_entry & (1 << 7) != 0 { return None; } // 1GB huge page

    let pdp = phys_to_virtual(pdp_entry & !0xFFF);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 { return None; }
    if pd_entry & (1 << 7) != 0 { return None; } // 2MB huge page

    let pd = phys_to_virtual(pd_entry & !0xFFF);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 { return None; }

    let pt = phys_to_virtual(pt_phys & !0xFFF);
    Some(unsafe { (*pt).0[pt_idx] })
}

/// Update a PTE for a given virtual address in a given PML4.
/// Returns `None` if the page is not mapped at 4KB level.
pub fn write_pte(pml4_phys: u64, virt: u64, new_pte: u64) -> Option<()> {
    let pml4 = phys_to_virtual(pml4_phys & !0xFFF);
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_entry = unsafe { (*pml4).0[pml4_idx] };
    if pdp_entry & PRESENT == 0 { return None; }
    if pdp_entry & (1 << 7) != 0 { return None; }

    let pdp = phys_to_virtual(pdp_entry & !0xFFF);
    let pd_entry = unsafe { (*pdp).0[pdp_idx] };
    if pd_entry & PRESENT == 0 { return None; }
    if pd_entry & (1 << 7) != 0 { return None; }

    let pd = phys_to_virtual(pd_entry & !0xFFF);
    let pt_phys = unsafe { (*pd).0[pd_idx] };
    if pt_phys & PRESENT == 0 { return None; }

    let pt = phys_to_virtual(pt_phys & !0xFFF);
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
        // TLB shootdown — no lock held, safe from any context (Bug 2).
        tlb_shootdown(virt);
    }
}
