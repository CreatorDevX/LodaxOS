extern crate alloc;
use alloc::boxed::Box;

use crate::arch::idt::TrapFrame;
use crate::arch::FpuState;
use crate::consts;
use crate::mm::{phys, virt};
use crate::mm::vma::ProcessMemory;

/// Wrapper around `*mut ProcessMemory` that is `Send`.
/// Safety: `ProcessMemory` is only accessed from the CPU that owns this vCPU
/// (the slab lock prevents concurrent access). The pointer is not shared
/// across threads.
pub(crate) struct SendPtr(pub *mut ProcessMemory);
unsafe impl Send for SendPtr {}
impl core::fmt::Debug for SendPtr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SendPtr({:#x})", self.0 as u64)
    }
}
impl Clone for SendPtr {
    fn clone(&self) -> Self { SendPtr(self.0) }
}
impl Copy for SendPtr {}

const FPU_STATE_INIT: [u8; 512] = {
    let mut buf = [0u8; 512];
    buf[0] = 0x7F; buf[1] = 0x03;
    buf[4] = 0xFF;
    buf[24] = 0x80; buf[25] = 0x1F;
    buf[28] = 0xFF; buf[29] = 0xFF;
    buf
};

pub type VcpuId = u32;

pub const GANG_UNSCHEDULED: u32 = 0;

/// Magic value written into `Vcpu::frame_magic` when a frame is saved.
/// Checked by the scheduler before context switch to detect corruption.
pub const FRAME_MAGIC: u64 = 0xDEAD_BEEF_CAFE_BABE;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VcpuState {
    Ready,
    Running,
    Halted,
    Blocked,
    Terminated,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VcpuType {
    Normal,
    HardwareDriver,
    AbstractionDriver,
    Idle,
}

#[derive(Debug, Clone)]
pub struct Vcpu {
    pub id: VcpuId,
    pub gang_id: u32,
    pub vcpu_type: VcpuType,
    pub state: VcpuState,
    pub affinity: u64,
    pub saved_frame: TrapFrame,
    /// Canary value: `FRAME_MAGIC` when frame is valid, 0 when freshly
    /// allocated (not yet started) or when the vCPU is terminated.
    /// The scheduler checks this before context switch to detect corrupted
    /// TrapFrame data.
    pub frame_magic: u64,
    pub kernel_stack_top: u64,
    pub pml4: u64,
    pub vruntime: u64,
    pub fpu_state: FpuState,
    /// Per-process memory state (VMA tree for demand paging).
    /// Owned by the vCPU; leaked on `free()` if not cleaned up.
    pub process_mem: SendPtr,
}

const MAX_VCPUS: usize = 128;

use crate::sync::IrqSaveSpinLock;

struct VcpuSlab {
    vcpus: [Option<Vcpu>; MAX_VCPUS],
    count: usize,
    initialized: bool,
}

static VCPU_SLAB: IrqSaveSpinLock<VcpuSlab> = IrqSaveSpinLock::new(VcpuSlab {
    vcpus: [const { None }; MAX_VCPUS],
    count: 0,
    initialized: false,
});

pub fn init() {
    let mut s = VCPU_SLAB.lock();
    s.count = 0;
    s.initialized = true;
    log::info!("vcpu: slab ready, {} slots", MAX_VCPUS);
}

pub fn is_initialized() -> bool {
    VCPU_SLAB.lock().initialized
}

pub fn alloc(
    gang_id: u32,
    pml4: u64,
    affinity: u64,
    vcpu_type: VcpuType,
) -> Option<VcpuId> {
    let result = {
        let mut s = VCPU_SLAB.lock();
        if s.count >= MAX_VCPUS {
            log::error!("vcpu: slab exhausted (max {})", MAX_VCPUS);
            return None;
        }
        let mut allocated = None;
        for i in 0..MAX_VCPUS {
            if s.vcpus[i].is_none() {
                let id = i as VcpuId;
                s.vcpus[i] = Some(Vcpu {
                    id,
                    gang_id,
                    vcpu_type,
                    state: VcpuState::Halted,
                    affinity,
                    saved_frame: TrapFrame {
                        r15: 0, r14: 0, r13: 0, r12: 0,
                        r11: 0, r10: 0, r9: 0, r8: 0,
                        rax: 0, rbx: 0, rcx: 0, rdx: 0,
                        rbp: 0, rsi: 0, rdi: 0,
                        vector: 0, error_code: 0,
                        rip: 0, cs: 0x08, rflags: 0x202,
                        rsp: 0, ss: 0x10,
                    },
                    frame_magic: 0, // not yet started
                    kernel_stack_top: 0,
                    pml4,
                    vruntime: 0,
                    fpu_state: FpuState(FPU_STATE_INIT),
                    process_mem: SendPtr(core::ptr::null_mut()),
                });
                s.count += 1;
                allocated = Some(id);
                break;
            }
        }
        allocated
    };
    if let Some(id) = result {
        log::trace!("vcpu: allocated id={}", id);
    }
    result
}

pub fn get_vcpu_type(id: VcpuId) -> VcpuType {
    with(id, |v| v.map_or(VcpuType::Idle, |vcpu| vcpu.vcpu_type))
}

pub fn free(id: VcpuId) {
    let mut freed = false;
    {
        let mut s = VCPU_SLAB.lock();
        let idx = id as usize;
        if idx < MAX_VCPUS && s.vcpus[idx].is_some() {
            if let Some(ref vcpu) = s.vcpus[idx] {
                if vcpu.vcpu_type != VcpuType::Idle && vcpu.kernel_stack_top != 0 {
                    let stack_virt_base = vcpu.kernel_stack_top - consts::KERNEL_STACK_SIZE;
                    // Physical base is one page below the virtual stack base (guard page).
                    let stack_phys = stack_virt_base - virt::HIGHER_HALF - consts::PAGE_SIZE;
                    let kpml4 = virt::kernel_pml4();
                    // Unmap the guard page + all stack pages.
                    // Guard page is one page below stack_virt_base.
                    let guard_addr = stack_virt_base - consts::PAGE_SIZE;
                    if let Some(pte) = virt::read_pte(kpml4, guard_addr) {
                        if pte & virt::PRESENT != 0 {
                            virt::write_pte(kpml4, guard_addr, 0);
                        }
                    }
                    let stack_pages = consts::KERNEL_STACK_SIZE / consts::PAGE_SIZE;
                    for p in 0..stack_pages {
                        let addr = stack_virt_base + p * consts::PAGE_SIZE;
                        if let Some(pte) = virt::read_pte(kpml4, addr) {
                            if pte & virt::PRESENT != 0 {
                                virt::write_pte(kpml4, addr, 0);
                            }
                        }
                    }
                    // Broadcast TLB shootdown for all cleared addresses so
                    // other CPUs don't retain stale translations to the freed
                    // stack pages (the kernel PML4 is shared across all CPUs).
                    crate::mm::virt::tlb_shootdown_range(
                        guard_addr,
                        stack_virt_base + consts::KERNEL_STACK_SIZE,
                    );
                    // Total pages = stack_pages + 1 (guard).
                    let total = stack_pages + 1;
                    phys::free_pages(stack_phys, total);
                }
                // Free ProcessMemory and its VMA tree if present.
                if !vcpu.process_mem.0.is_null() {
                    unsafe { drop(Box::from_raw(vcpu.process_mem.0)); }
                }
            }
            s.vcpus[idx] = None;
            s.count = s.count.saturating_sub(1);
            freed = true;
        }
    }
    if freed {
        log::trace!("vcpu: freed id={}", id);
    }
}

pub fn get(id: VcpuId) -> Option<Vcpu> {
    let s = VCPU_SLAB.lock();
    let idx = id as usize;
    if idx < MAX_VCPUS {
        s.vcpus[idx].clone()
    } else {
        None
    }
}

pub fn with<R>(id: VcpuId, f: impl FnOnce(Option<&Vcpu>) -> R) -> R {
    let s = VCPU_SLAB.lock();
    let idx = id as usize;
    if idx < MAX_VCPUS {
        f(s.vcpus[idx].as_ref())
    } else {
        f(None)
    }
}

pub fn with_mut<R>(id: VcpuId, f: impl FnOnce(Option<&mut Vcpu>) -> R) -> R {
    let mut s = VCPU_SLAB.lock();
    let idx = id as usize;
    if idx < MAX_VCPUS {
        f(s.vcpus[idx].as_mut())
    } else {
        f(None)
    }
}

/// Access the `ProcessMemory` of a vCPU by id. The closure receives `&mut ProcessMemory`
/// if the vCPU exists and has one. Panics if the vCPU doesn't exist.
pub fn with_process_mut<R>(id: VcpuId, f: impl FnOnce(&mut ProcessMemory) -> R) -> R {
    let mut s = VCPU_SLAB.lock();
    let idx = id as usize;
    if idx < MAX_VCPUS {
        if let Some(vcpu) = s.vcpus[idx].as_mut() {
            if !vcpu.process_mem.0.is_null() {
                return f(unsafe { &mut *vcpu.process_mem.0 });
            }
        }
    }
    panic!("vcpu::with_process_mut: vcpu {} has no ProcessMemory", id);
}

pub fn count() -> usize {
    VCPU_SLAB.lock().count
}

/// Unsafe direct mutable access to a Vcpu by id.
/// Caller must guarantee exclusive access (e.g., hold the GangTable lock).
pub unsafe fn get_mut(id: VcpuId) -> Option<&'static mut Vcpu> {
    let s = VCPU_SLAB.unsafe_get();
    let idx = id as usize;
    if idx < MAX_VCPUS {
        s.vcpus[idx].as_mut()
    } else {
        None
    }
}
