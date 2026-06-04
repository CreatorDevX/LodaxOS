use core::sync::atomic::{AtomicBool, Ordering};
use lodaxos_system::{CapOp, Caps};

use super::phys;
use crate::cap;

const PAGE_SIZE: u64 = 0x1000;

// Page table entry flags
pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const CACHE_DISABLE: u64 = 1 << 4; // PCD — force uncacheable for MMIO
pub const NO_EXECUTE: u64 = 1 << 63;

pub const DATA: u64 = PRESENT | WRITABLE | NO_EXECUTE;

/// Higher-half base: 0xFFFF_8000_0000_0000
pub const HIGHER_HALF: u64 = 0xFFFF_8000_0000_0000;

static PT_INITIALIZED: AtomicBool = AtomicBool::new(false);

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

/// Ensure `entry` points to a page table. Returns the physical address of
/// the table. If the entry is a huge page (PS bit set), splits it into
/// sub-entries so the caller can create finer-grained mappings.
///
/// `level` is the PML4 walk depth: 3=PML4, 2=PDP, 1=PD, 0=PT.
/// - level 2: 1GB huge page → split into 512 × 2MB entries (PS set).
/// - level 1: 2MB huge page → split into 512 × 4KB entries (PS clear).
fn ensure_table(entry: &mut u64, flags: u64, level: usize) -> u64 {
    if *entry & PRESENT == 0 {
        let page = phys::alloc_page().expect("out of memory for page tables");
        let virt = phys_to_virtual(page);
        unsafe {
            (*virt) = PageTable::new_zeroed();
        }
        *entry = page | flags | PRESENT;
    }

    // Split huge pages so the caller can map at a finer granularity.
    if *entry & (1 << 7) != 0 {
        let orig_flags = *entry & !(PRESENT | (1 << 7)); // strip present + PS

        let new_page = phys::alloc_page().expect("out of memory for page table split");
        let new_table = phys_to_virtual(new_page);
        unsafe { (*new_table) = PageTable::new_zeroed(); }

        match level {
            2 => {
                // 1 GB → 512 × 2 MB entries (each with PS bit set).
                let base = *entry & 0x000F_FFFF_FFC0_0000; // 1 GB aligned
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x20_0000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags | (1 << 7);
                    }
                }
            }
            1 => {
                // 2 MB → 512 × 4 KB entries (PS clear, normal PT entries).
                let base = *entry & 0x000F_FFFF_FFFF_0000; // 2 MB aligned
                for i in 0..512usize {
                    let entry_phys = base + (i as u64) * 0x1000;
                    unsafe {
                        (*new_table).0[i] = entry_phys | orig_flags;
                    }
                }
            }
            _ => {
                panic!(
                    "ensure_table: huge page at unexpected level {} (entry={:#x})",
                    level, *entry
                );
            }
        }

        // Point the parent entry to the new table (preserve caller's flags).
        *entry = new_page | flags | PRESENT;
    }

    *entry & !0xFFF
}

fn index_for_addr(virt: u64, level: usize) -> usize {
    ((virt >> (12 + level * 9)) & 0x1FF) as usize
}

pub unsafe fn init(regions: &[(u64, u64)], fb_phys: Option<(u64, u64)>) {
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
    core::arch::asm!("mov cr3, {}", in(reg) pml4_page);
    log::info!("Page tables: CR3 loaded with phys={:#x}", pml4_page);
    log::info!("Page tables: post-CR3-switch check");

    PT_INITIALIZED.store(true, Ordering::SeqCst);
}

/// Identity-mapped helpers for use during init (before CR3 switch).
/// Uses physical addresses directly since UEFI page tables identity-map all memory.
fn id_ensure_table(entry: &mut u64, flags: u64) -> u64 {
    if *entry & PRESENT == 0 {
        let page = phys::alloc_page().expect("out of memory for page tables");
        let target = page as *mut PageTable;
        unsafe {
            (*target) = PageTable::new_zeroed();
        }
        *entry = page | flags | PRESENT;
    }
    *entry & !0xFFF
}

fn id_map_page(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    let pml4 = pml4_phys as *mut PageTable;
    let pml4_idx = index_for_addr(virt, 3);
    let pdp_idx = index_for_addr(virt, 2);
    let pd_idx = index_for_addr(virt, 1);
    let pt_idx = index_for_addr(virt, 0);

    let pdp_phys = id_ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, WRITABLE);
    let pdp = pdp_phys as *mut PageTable;

    let pd_phys = id_ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, WRITABLE);
    let pd = pd_phys as *mut PageTable;

    let pt_phys = id_ensure_table(unsafe { &mut (*pd).0[pd_idx] }, WRITABLE);
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

    let pdp_phys = id_ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, WRITABLE);
    let pdp = pdp_phys as *mut PageTable;

    let pd_phys = id_ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, WRITABLE);
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

    // Level 2: PDP→PD — may hit 1 GB huge pages (from identity map).
    let pdp_phys = ensure_table(unsafe { &mut (*pml4).0[pml4_idx] }, WRITABLE, 2);
    let pdp = phys_to_virtual(pdp_phys);

    // Level 1: PD→PT — may hit 2 MB huge pages (from identity map).
    let pd_phys = ensure_table(unsafe { &mut (*pdp).0[pdp_idx] }, WRITABLE, 1);
    let pd = phys_to_virtual(pd_phys);

    let pt_phys = ensure_table(unsafe { &mut (*pd).0[pd_idx] }, WRITABLE, 0);
    let pt = phys_to_virtual(pt_phys);

    unsafe {
        (*pt).0[pt_idx] = phys | flags;
    }
}

pub fn translate(virt: u64) -> Option<u64> {
    if !PT_INITIALIZED.load(Ordering::SeqCst) {
        return None;
    }

    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
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
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        if virt >= HIGHER_HALF { Caps::CAP_MM_MAP_KERNEL } else { Caps::CAP_MM_MAP },
        CapOp::MmUnmap { vaddr: virt },
    ) {
        log::warn!("virt::unmap: cap denied: {:?}", e);
        return;
    }

    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
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

    // Flush TLB
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt);
    }
}

pub fn pml4_address() -> u64 {
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
    cr3 & !0xFFF
}

/// Switch the active PML4 (write CR3). The new PML4 must already be
/// populated — this only updates the hardware register. The caller
/// is responsible for ensuring the new PML4 maps everything the new
/// context will need (kernel code, IDT handler code, stack, etc.).
#[inline]
pub fn switch_pml4(pml4_phys: u64) {
    // Memory fence: ensure all prior stores to the new PML4's page
    // tables are visible before CR3 is loaded.
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
        core::arch::asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags));
    }
}

// ---- PML4 deep-copy (for per-task address spaces) ----

/// Recursive helper: deep-copy a 4-level page table subtree.
/// `src_phys` is the physical address of the source table (PML4, PDP, PD, or PT).
/// `level` is 3 (PML4), 2 (PDP), 1 (PD), or 0 (PT).
/// Returns the physical address of the new copy, or `None` on OOM.
fn copy_table_recursive(src_phys: u64, level: usize) -> Option<u64> {
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
            // PT level — entries are leaves, just copy.
            unsafe { (*new).0[i] = entry };
        } else if entry & (1 << 7) != 0 {
            // 1GB (level 2) or 2MB (level 1) huge page — leaf, just copy.
            unsafe { (*new).0[i] = entry };
        } else {
            // Points to a sub-table — recurse.
            let child_src = entry & !0xFFF;
            let child_dst = copy_table_recursive(child_src, level - 1)?;
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
/// The caller can then modify the new PML4 (e.g. add ELF segments)
/// without affecting the source. When the new PML4 is no longer
/// needed, call `free_pml4`.
pub fn fork_pml4(src_phys: u64) -> Option<u64> {
    copy_table_recursive(src_phys, 3)
}

/// Free a PML4 and all its sub-tables. Does NOT free the physical
/// pages the PML4 points to (those are owned by whoever mapped them).
/// Only frees the page-table structure pages themselves.
pub fn free_pml4(pml4_phys: u64) {
    free_table_recursive(pml4_phys, 3);
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

        let pdp_phys = ensure_table(&mut (*pml4).0[pml4_idx], WRITABLE, 2);
        let pdp = phys_to_virtual(pdp_phys);
        let pd_phys = ensure_table(&mut (*pdp).0[pdp_idx], WRITABLE, 1);
        let pd = phys_to_virtual(pd_phys);
        let pt_phys = ensure_table(&mut (*pd).0[pd_idx], WRITABLE, 0);
        let pt = phys_to_virtual(pt_phys);

        for i in 0..batch {
            (*pt).0[pt_idx as usize + i as usize] = (phys + i * PAGE_SIZE) | flags;
        }

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

/// Explicitly map a single 4KB page (public wrapper).
pub fn map_page_explicit(pml4_phys: u64, virt: u64, phys: u64, flags: u64) {
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        if virt >= HIGHER_HALF { Caps::CAP_MM_MAP_KERNEL } else { Caps::CAP_MM_MAP },
        CapOp::MmMap { vaddr: virt, paddr: phys, flags: flags as u32, kernel_half: virt >= HIGHER_HALF },
    ) {
        log::warn!("virt::map_page_explicit: cap denied: {:?}", e);
        return;
    }
    let pml4 = phys_to_virtual(pml4_phys);
    map_page(pml4, virt, phys, flags);
}
