use crate::arch::idt::TrapFrame;
use crate::arch::FpuState;
use crate::consts;
use crate::mm::{phys, virt};

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VcpuState {
    Ready,
    Running,
    Halted,
    Blocked,
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
    pub kernel_stack_top: u64,
    pub pml4: u64,
    pub vruntime: u64,
    pub fpu_state: FpuState,
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
    unsafe { VCPU_SLAB.unsafe_get().initialized }
}

pub fn alloc(
    gang_id: u32,
    pml4: u64,
    affinity: u64,
    vcpu_type: VcpuType,
) -> Option<VcpuId> {
    let mut s = VCPU_SLAB.lock();
    if s.count >= MAX_VCPUS {
        log::error!("vcpu: slab exhausted (max {})", MAX_VCPUS);
        return None;
    }
    for i in 0..MAX_VCPUS {
        if s.vcpus[i].is_none() {
            let id = i as VcpuId;
            s.vcpus[i] = Some(Vcpu {
                id,
                gang_id,
                vcpu_type,
                state: VcpuState::Ready,
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
                kernel_stack_top: 0,
                pml4,
                vruntime: 0,
                fpu_state: FpuState(FPU_STATE_INIT),
            });
            s.count += 1;
            log::trace!("vcpu: allocated id={}", id);
            return Some(id);
        }
    }
    None
}

pub fn get_vcpu_type(id: VcpuId) -> VcpuType {
    match get(id) {
        Some(v) => v.vcpu_type,
        None => VcpuType::Idle,
    }
}

pub fn free(id: VcpuId) {
    let mut s = VCPU_SLAB.lock();
    let idx = id as usize;
    if idx < MAX_VCPUS && s.vcpus[idx].is_some() {
        // Free the kernel stack pages if they were dynamically
        // allocated (Bug 14 fix).  Created Vcpus have a non-zero
        // kernel_stack_top from alloc_pages(3); idle Vcpus share
        // the boot stack and must not be freed here.
        if let Some(ref vcpu) = s.vcpus[idx] {
            if vcpu.vcpu_type != VcpuType::Idle && vcpu.kernel_stack_top != 0 {
                let stack_phys = vcpu.kernel_stack_top
                    - virt::HIGHER_HALF
                    - consts::PAGE_SIZE
                    - consts::KERNEL_STACK_SIZE;
                phys::free_pages(stack_phys, 8);
            }
        }
        s.vcpus[idx] = None;
        s.count -= 1;
        log::trace!("vcpu: freed id={}", id);
    }
}

pub fn get(id: VcpuId) -> Option<&'static Vcpu> {
    let s = unsafe { VCPU_SLAB.unsafe_get() };
    let idx = id as usize;
    if idx < MAX_VCPUS {
        s.vcpus[idx].as_ref()
    } else {
        None
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

pub fn count() -> usize {
    unsafe { VCPU_SLAB.unsafe_get().count }
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
