use core::ptr;

use super::phys;

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
        // RadixNode has 1024 entries × 8 bytes = 8 KB
        let page = phys::alloc_order(1).expect("radix tree: OOM");
        unsafe {
            ptr::write_bytes(page as *mut u8, 0, 2 * phys::PAGE_SIZE as usize);
        }
        page as *mut RadixNode
    }
}

pub struct VmaTree {
    root: *mut RadixNode,
}

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
    pub fn lookup(&self, addr: u64) -> Option<&mut Vma> {
        if self.root.is_null() {
            return None;
        }

        let mut node = self.root;
        for level in (1..RADIX_LEVELS).rev() {
            let idx = radix_index(addr, level);
            unsafe {
                let entry = &(*node).entries[idx];
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
    pub fn find_covering(&self, addr: u64) -> Option<&mut Vma> {
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

    /// Remove the VMA at the given start address.
    pub fn remove(&mut self, start: u64) -> Option<*mut Vma> {
        if self.root.is_null() {
            return None;
        }

        let mut node = self.root;
        for level in (1..RADIX_LEVELS).rev() {
            let idx = radix_index(start, level);
            unsafe {
                let entry = &mut (*node).entries[idx];
                if entry.child.is_null() {
                    return None;
                }
                node = entry.child;
            }
        }

        let leaf_idx = radix_index(start, 0);
        unsafe {
            let vma_ptr = (*node).entries[leaf_idx].vma;
            (*node).entries[leaf_idx].vma = ptr::null_mut();
            if vma_ptr.is_null() {
                None
            } else {
                Some(vma_ptr)
            }
        }
    }

    /// Visit all VMAs in the tree.
    pub fn visit_all<F, R>(&self, mut f: F) -> Option<R>
    where
        F: FnMut(&mut Vma) -> Option<R>,
    {
        if self.root.is_null() {
            return None;
        }
        unsafe { self.visit_node(self.root, 4, &mut f) }
    }

    unsafe fn visit_node<F, R>(
        &self,
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
        let boxed = unsafe {
            let ptr = phys::alloc_page().unwrap() as *mut Vma;
            ptr::write(ptr, vma);
            &mut *ptr
        };
        self.vma_tree.insert(boxed);
        boxed
    }

    /// Handle a page fault. Returns true if the fault was handled (page mapped).
    pub fn handle_page_fault(&mut self, fault_addr: u64, write: bool) -> bool {
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
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) page_addr);
        }

        true
    }
}

// ---- Global kernel VMA tree for demand paging ----

fn kernel_vma_ptr() -> *mut VmaTree {
    &raw mut KERNEL_VMA_TREE
}

static mut KERNEL_VMA_TREE: VmaTree = VmaTree::new_const();

pub fn init_kernel_vmas() {
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
    let ptr = unsafe {
        let p = phys::alloc_page().unwrap() as *mut Vma;
        ptr::write(p, vma);
        p
    };
    unsafe {
        (*kernel_vma_ptr()).insert(&mut *ptr);
    }
    log::info!("Kernel VMA tree initialized: heap {:#x}-{:#x}",
        heap_virt_base, heap_virt_base + heap_size);
}

pub fn handle_page_fault(fault_addr: u64, error_code: u64) -> bool {
    let _is_write = error_code & (1 << 1) != 0;
    let is_user = error_code & (1 << 2) != 0;
    let is_present = error_code & 1 != 0;

    if is_present {
        // Protection violation (page exists but wrong permissions)
        return false;
    }

    if is_user {
        // User-mode fault — would need per-process VMA tree
        // For now, reject (will be handled later with capability system)
        return false;
    }

    // Kernel-mode fault — check kernel VMA tree
    let result = unsafe {
        (*kernel_vma_ptr()).find_covering(fault_addr)
    };

    match result {
        Some(_vma) => {
            let page_addr = fault_addr & !0xFFF;
            let phys_page = match phys::alloc_page() {
                Some(p) => p,
                None => return false,
            };
            let cr3: u64;
            unsafe {
                core::arch::asm!("mov {}, cr3", out(reg) cr3);
                super::virt::map_page_explicit(
                    cr3 & !0xFFF,
                    page_addr,
                    phys_page,
                    super::virt::DATA,
                );
                // Flush the TLB entry for this page — the MMU still caches
                // the old "not present" state and would immediately re-#PF
                // without this invlpg.
                core::arch::asm!("invlpg [{}]", in(reg) page_addr);
            }
            log::trace!("Demand-paged: {:#x} -> {:#x}", page_addr, phys_page);
            true
        }
        None => false,
    }
}
