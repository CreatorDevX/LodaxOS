extern crate alloc;
use alloc::boxed::Box;

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::phys;
use crate::sync::IrqSaveSpinLock;

const PAGE_SHIFT: u64 = 12;

/// Radix tree: 4 levels × 10 bits (bits 12–51) = 1 TB addressable per tree.
const RADIX_BITS: u64 = 10;
const RADIX_SIZE: usize = 1 << RADIX_BITS;
const RADIX_MASK: u64 = (RADIX_SIZE - 1) as u64;
const RADIX_LEVELS: usize = 4;

#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmaPerm {
    None = 0,
    Read = 1,
    Write = 2,
    ReadWrite = 3,
    Execute = 4,
    ReadExecute = 5,
    WriteExecute = 6,
    ReadWriteExecute = 7,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub perm: VmaPerm,
    pub flags: u64,
}

#[repr(C)]
union RadixEntry {
    child: *mut RadixNode,
    vma: *mut Vma,
}

#[repr(C)]
struct RadixNode {
    entries: [RadixEntry; RADIX_SIZE],
}

impl RadixNode {
    fn new_zeroed() -> *mut Self {
        let page = phys::alloc_order(1).expect("radix tree: OOM");
        unsafe {
            ptr::write_bytes(page as *mut u8, 0, 2 * phys::PAGE_SIZE as usize);
        }
        page as *mut RadixNode
    }

    unsafe fn free_node(node: *mut Self) {
        phys::free_order(node as u64, 1);
    }
}

pub struct VmaTree {
    root: *mut RadixNode,
}

// Required for `IrqSaveSpinLock<VmaTree>` to be `Sync`.
unsafe impl Send for VmaTree {}

fn radix_shift(level: usize) -> u64 {
    PAGE_SHIFT + (level as u64) * RADIX_BITS
}

fn radix_index(addr: u64, level: usize) -> usize {
    ((addr >> radix_shift(level)) & RADIX_MASK) as usize
}

impl VmaTree {
    pub const fn new_const() -> Self {
        Self {
            root: ptr::null_mut(),
        }
    }

    pub fn new() -> Self {
        Self {
            root: ptr::null_mut(),
        }
    }

    /// Insert a VMA into the tree keyed by its start address.
    pub fn insert(&mut self, vma: &mut Vma) {
        let root = if self.root.is_null() {
            self.root = RadixNode::new_zeroed();
            self.root
        } else {
            self.root
        };

        let mut node = root;
        for level in (1..RADIX_LEVELS).rev() {
            let idx = radix_index(vma.start, level);
            unsafe {
                let entry = &mut (*node).entries[idx];
                if entry.child.is_null() {
                    entry.child = RadixNode::new_zeroed();
                }
                node = entry.child;
            }
        }

        let leaf_idx = radix_index(vma.start, 0);
        unsafe {
            (*node).entries[leaf_idx].vma = vma as *mut Vma;
        }
    }

    /// Look up a VMA by virtual address. Returns the VMA if found at the exact address.
    /// Takes `&mut self` to enforce exclusive access (interior mutability is used via the
    /// `IrqSaveSpinLock` that guards the tree).
    pub fn lookup(&mut self, addr: u64) -> Option<&mut Vma> {
        if self.root.is_null() {
            return None;
        }

        let mut node = self.root;
        for level in (1..RADIX_LEVELS).rev() {
            let idx = radix_index(addr, level);
            unsafe {
                let entry = &mut (*node).entries[idx];
                if entry.child.is_null() {
                    return None;
                }
                node = entry.child;
            }
        }

        let leaf_idx = radix_index(addr, 0);
        unsafe {
            let vma_ptr = (*node).entries[leaf_idx].vma;
            if vma_ptr.is_null() {
                None
            } else {
                Some(&mut *vma_ptr)
            }
        }
    }

    /// Find the VMA that covers `addr` (addr ∈ [vma.start, vma.end)).
    /// Uses linear search since VMAs are typically few.
    /// Takes `&mut self` to enforce exclusive access.
    pub fn find_covering(&mut self, addr: u64) -> Option<&mut Vma> {
        if self.root.is_null() {
            return None;
        }
        let result: Option<*mut Vma> = self.visit_all(|vma| {
            if addr >= vma.start && addr < vma.end {
                Some(vma as *mut Vma)
            } else {
                None
            }
        });
        result.map(|p| unsafe { &mut *p })
    }

    /// Remove the VMA at the given start address and free its memory.
    /// Also frees intermediate radix tree nodes that become empty.
    pub fn remove(&mut self, start: u64) -> bool {
        if self.root.is_null() {
            return false;
        }

        // Track path for cleanup
        let mut path_nodes: [*mut RadixNode; RADIX_LEVELS] = [ptr::null_mut(); RADIX_LEVELS];
        let mut path_indices: [usize; RADIX_LEVELS] = [0; RADIX_LEVELS];

        let mut node = self.root;
        for level in (1..RADIX_LEVELS).rev() {
            let idx = radix_index(start, level);
            path_nodes[level] = node;
            path_indices[level] = idx;
            unsafe {
                let entry = &mut (*node).entries[idx];
                if entry.child.is_null() {
                    return false;
                }
                node = entry.child;
            }
        }

        let leaf_idx = radix_index(start, 0);
        unsafe {
            let vma_ptr = (*node).entries[leaf_idx].vma;
            (*node).entries[leaf_idx].vma = ptr::null_mut();
            if vma_ptr.is_null() {
                return false;
            }
            drop(Box::from_raw(vma_ptr));

            // Check if leaf node is now empty and free it upward
            let mut cur_node = node;
            for level in 0..RADIX_LEVELS {
                let mut has_entries = false;
                for i in 0..RADIX_SIZE {
                    if level == 0 {
                        if !(*cur_node).entries[i].vma.is_null() {
                            has_entries = true;
                            break;
                        }
                    } else {
                        if !(*cur_node).entries[i].child.is_null() {
                            has_entries = true;
                            break;
                        }
                    }
                }
                if !has_entries {
                    RadixNode::free_node(cur_node);
                    if level + 1 < RADIX_LEVELS {
                        let parent = path_nodes[level + 1];
                        let pidx = path_indices[level + 1];
                        (*parent).entries[pidx].child = ptr::null_mut();
                        cur_node = parent;
                    } else {
                        self.root = ptr::null_mut();
                    }
                } else {
                    break;
                }
            }
            true
        }
    }

    pub fn visit_all<F, R>(&mut self, mut f: F) -> Option<R>
    where
        F: FnMut(&mut Vma) -> Option<R>,
    {
        if self.root.is_null() {
            return None;
        }
        unsafe { self.visit_node(self.root, 4, &mut f) }
    }

    unsafe fn visit_node<F, R>(
        &mut self,
        node: *mut RadixNode,
        level: usize,
        f: &mut F,
    ) -> Option<R>
    where
        F: FnMut(&mut Vma) -> Option<R>,
    {
        if level == 0 {
            for i in 0..RADIX_SIZE {
                let vma_ptr = (*node).entries[i].vma;
                if !vma_ptr.is_null() {
                    if let Some(result) = f(&mut *vma_ptr) {
                        return Some(result);
                    }
                }
            }
            None
        } else {
            for i in 0..RADIX_SIZE {
                let child = (*node).entries[i].child;
                if !child.is_null() {
                    if let Some(result) = self.visit_node(child, level - 1, f) {
                        return Some(result);
                    }
                }
            }
            None
        }
    }
}

/// Process memory descriptor — holds the VMA tree for one process.
pub struct ProcessMemory {
    pub vma_tree: VmaTree,
    pub pml4_phys: u64,
}

impl ProcessMemory {
    pub fn new(pml4_phys: u64) -> Self {
        Self {
            vma_tree: VmaTree::new(),
            pml4_phys,
        }
    }

    /// Add a VMA covering [start, end) with given permissions.
    pub fn add_vma(&mut self, start: u64, end: u64, perm: VmaPerm) -> &mut Vma {
        let vma = Vma {
            start,
            end,
            perm,
            flags: 0,
        };
        let boxed = Box::into_raw(Box::new(vma));
        unsafe {
            self.vma_tree.insert(&mut *boxed);
            &mut *boxed
        }
    }

    /// Handle a page fault. Returns true if the fault was handled (page mapped).
    pub fn handle_page_fault(&mut self, fault_addr: u64, write: bool) -> bool {
        if write {
        if let Some(pte) = super::virt::read_pte(self.pml4_phys, fault_addr) {
            if pte & super::virt::COW != 0 {
                return self.handle_cow_fault(fault_addr);
            }
        }
    }

    let vma = match self.vma_tree.find_covering(fault_addr) {
            Some(v) => v,
            None => return false,
        };

        if write && vma.perm as u64 & VmaPerm::Write as u64 == 0 {
            return false;
        }

        let page_addr = fault_addr & !0xFFF;
        let phys_page = match phys::alloc_page() {
            Some(p) => p,
            None => return false,
        };

        super::virt::map_page_explicit(
            self.pml4_phys,
            page_addr,
            phys_page,
            super::virt::DATA,
        );
        x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(page_addr));

        true
    }

    fn handle_cow_fault(&mut self, fault_addr: u64) -> bool {
        let page_addr = fault_addr & !0xFFF;
        let old_pte = match super::virt::read_pte(self.pml4_phys, page_addr) {
            Some(p) => p,
            None => return false,
        };
        resolve_cow(self.pml4_phys, page_addr, old_pte)
    }
}

/// Shared COW resolution: allocates a new page, copies content from the
/// old physical page, and maps it writable in the given PML4.
/// Returns true on success.
fn resolve_cow(pml4_phys: u64, page_addr: u64, old_pte: u64) -> bool {
    let old_phys = old_pte & 0x000F_FFFF_FFFF_F000;
    let new_phys = match phys::alloc_page() {
        Some(p) => p,
        None => return false,
    };
    unsafe {
        core::ptr::copy_nonoverlapping(
            old_phys as *const u8,
            new_phys as *mut u8,
            phys::PAGE_SIZE as usize,
        );
    }
    let new_pte = new_phys | ((old_pte & 0xFFF) & !super::virt::COW) | super::virt::WRITABLE;
    super::virt::write_pte(pml4_phys, page_addr, new_pte);
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(page_addr));
    // Note: the old physical page is NOT freed here — it may still be
    // mapped in the parent process's PML4 (with COW+RO).
    true
}

// ---- Global kernel VMA tree for demand paging ----

/// Synchronized kernel VMA tree.  All mutations go through
/// `with_kernel_vma_tree()` which acquires the IRQ-safe lock.
static KERNEL_VMA_TREE: IrqSaveSpinLock<VmaTree> = IrqSaveSpinLock::new(VmaTree::new_const());

/// Single-shot gate for `init_kernel_vmas`.  Prevents re-entrancy.
static KERNEL_VMA_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Run `f` with the kernel VMA tree locked.  The closure receives
/// a mutable reference to the tree.
pub fn with_kernel_vma_tree<R>(f: impl FnOnce(&mut VmaTree) -> R) -> R {
    let mut guard = KERNEL_VMA_TREE.lock();
    f(&mut *guard)
}

pub fn init_kernel_vmas() {
    if KERNEL_VMA_INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    // The kernel heap is mapped at HIGHER_HALF.
    // Register it as a demand-paged VMA (covers entire 64MB heap range).
    let heap_virt_base: u64 = 0xFFFF_8080_0000_0000;
    let heap_size: u64 = 64 * 1024 * 1024;
    let vma = Vma {
        start: heap_virt_base,
        end: heap_virt_base + heap_size,
        perm: VmaPerm::ReadWrite,
        flags: 0,
    };
    let ptr = Box::into_raw(Box::new(vma));
    with_kernel_vma_tree(|tree| {
        tree.insert(unsafe { &mut *ptr });
    });
    KERNEL_VMA_INITIALIZED.store(true, Ordering::Release);
    log::info!("Kernel VMA tree initialized: heap {:#x}-{:#x}",
        heap_virt_base, heap_virt_base + heap_size);
}

pub fn handle_page_fault(fault_addr: u64, error_code: u64) -> bool {
    let is_write = error_code & (1 << 1) != 0;
    let is_user = error_code & (1 << 2) != 0;
    let is_present = error_code & 1 != 0;

    if is_present && is_write {
        let cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
        if let Some(pte) = super::virt::read_pte(cr3 & !0xFFF, fault_addr) {
            if pte & super::virt::COW != 0 {
                let page_addr = fault_addr & !0xFFF;
                return resolve_cow(cr3 & !0xFFF, page_addr, pte);
            }
        }
        return false;
    }

    if is_present {
        return false;
    }

    if is_user {
        return false;
    }

    // Kernel-mode fault — check kernel VMA tree.
    let found = with_kernel_vma_tree(|tree| {
        tree.find_covering(fault_addr).is_some()
    });

    if found {
        let page_addr = fault_addr & !0xFFF;
        let phys_page = match phys::alloc_page() {
            Some(p) => p,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes((super::virt::HIGHER_HALF + phys_page) as *mut u8, 0, phys::PAGE_SIZE as usize);
            let cr3: u64;
            core::arch::asm!("mov {}, cr3", out(reg) cr3);
            super::virt::map_page_explicit(
                cr3 & !0xFFF,
                page_addr,
                phys_page,
                super::virt::DATA,
            );
        }
        log::trace!("Demand-paged: {:#x} -> {:#x}", page_addr, phys_page);
        true
    } else {
        false
    }
}
