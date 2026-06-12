#![no_std]

use core::sync::atomic::AtomicU32;

pub const MAX_MEMORY_REGIONS: usize = 128;
pub const MAX_CPUS: usize = 4;

/// Fixed physical address where the chainloader stores an 8-byte pointer
/// to the dynamically allocated BootInfo struct. The bootloader reads this
/// pointer, updates the BootInfo, and passes the pointer to the kernel.
/// This way the BootInfo itself (which is large — ~2 KB) lives at a
/// dynamically chosen address instead of a fragile fixed page.
pub const BOOT_INFO_HANDOFF_ADDR: u64 = 0x1000;

/// Passed from chainloader → bootloader → kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    pub memory_region_count: usize,
    pub framebuffer: FramebufferInfo,
    /// Start LBA of the ext4 partition (Partition Zero).
    pub partition_zero_lba: u64,
    /// Size in bytes of the ext4 partition.
    pub partition_zero_size: u64,
    /// Kernel image loaded by bootloader from ext4 (staging buffer).
    pub kernel_image_addr: u64,
    /// Size in bytes of the kernel image.
    pub kernel_image_size: u64,
    /// Physical address of the RSDP (Root System Description Pointer),
    /// captured from UEFI config table before exit_boot_services.
    pub rsdp_addr: u64,
    /// Physical address of the MADT (APIC) table, discovered by kernel from RSDP.
    pub madt_addr: u64,
    /// Maximum number of CPUs the kernel will bring up.
    pub max_cpus: u32,
    /// BSP LAPIC ID (always 0 on x86).
    pub bsp_apic_id: u32,
    /// Number of enabled application processors (APs) reported by UEFI MP Services.
    pub ap_count: u32,
    /// LAPIC ID of each AP, indexed 0..ap_count.
    pub ap_apic_ids: [u32; MAX_CPUS],
}



#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub phys_start: u64,
    pub size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub phys_addr: u64,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub bytes_per_pixel: usize,
    pub is_bgr: bool,
}

// =====================================================================
// Capability system
// =====================================================================
//
// The capability system is the policy boundary between the kernel and
// the policy process (future). Mechanism (who can do what check) lives in the
// kernel (`kernel/src/cap.rs`); v1 has no IPC, so the cap check is
// **static-only** (does the subject hold the cap bit?). When IPC is
// implemented, the dynamic check will write a `CapRequest` to the
// shared mailbox, IPI-wake the policy process, and wait for a `CapResponse`.
//
// On ring 0, subjects are kernel tasks (no userspace yet). The cap set
// lives in the `Task` struct and is updated atomically.

bitflags::bitflags! {
    /// Capability bitfield. Each bit is one capability.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Caps: u64 {
        // --- Output / logging ---
        const CAP_LOG            = 1 << 0;
        const CAP_TERMINAL       = 1 << 1;
        const CAP_DEBUG          = 1 << 2;

        // --- Memory ---
        const CAP_MM_ALLOC       = 1 << 3;  // phys::allocate_frame / free_frame
        const CAP_MM_MAP         = 1 << 4;  // vma::map_page / unmap (user half)
        const CAP_MM_MAP_KERNEL  = 1 << 5;  // map into the kernel half

        // --- Tasks ---
        const CAP_TASK_CREATE    = 1 << 6;
        const CAP_TASK_DESTROY   = 1 << 7;
        const CAP_TASK_SCHED     = 1 << 8;  // yield, wake (self)
        const CAP_TASK_WAKE_OTHER= 1 << 9;  // wake a task that is not self
        const CAP_TASK_PIN       = 1 << 10; // pin/migrate tasks to CPUs

        // --- Interrupts / drivers ---
        const CAP_INTR_INSTALL   = 1 << 11;
        const CAP_INTR_MASK      = 1 << 12;
        const CAP_INTR_EOI       = 1 << 13;
        const CAP_DRIVER_PCI     = 1 << 14;
        const CAP_DRIVER_BLOCK   = 1 << 15;
        const CAP_DRIVER_NET     = 1 << 16;
        const CAP_DRIVER_INPUT   = 1 << 17;

        // --- IPC (future) ---
        const CAP_IPC_CREATE     = 1 << 18;
        const CAP_IPC_SEND       = 1 << 19;
        const CAP_IPC_RECV       = 1 << 20;

        // --- Filesystem (future) ---
        const CAP_FS_MOUNT       = 1 << 21;
        const CAP_FS_READ        = 1 << 22;
        const CAP_FS_WRITE       = 1 << 23;

        // --- Policy / power ---
        const CAP_POLICY_READ    = 1 << 24; // inspect a task's caps
        const CAP_POLICY_WRITE   = 1 << 25; // grant/revoke caps
        const CAP_REBOOT         = 1 << 26;
        const CAP_HALT           = 1 << 27;
    }
}

/// Capability bit index (0..63). Returned by `Caps::iter_names`.
pub type CapId = u8;

/// Subject identity. On ring 0 this is a `TaskId` (== index into the
/// kernel's task table). When ring 3 is added later, this will also
/// cover process identities.
pub type SubjectId = u32;

/// What kind of operation the kernel is performing. Passed to the policy
/// hook so it can make a fine-grained decision (e.g. "allow MMU map to
/// kernel half only when caller is task 0").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapOp {
    MmAlloc       { frames: usize },
    MmMap         { vaddr: u64, paddr: u64, flags: u32, kernel_half: bool },
    MmUnmap       { vaddr: u64 },
    TaskCreate    { parent: Option<SubjectId> },
    TaskDestroy   { target: SubjectId },
    TaskPin       { target: SubjectId, cpu: u8 },
    IntrInstall   { vector: u8 },
    IntrMask      { vector: u8, mask: bool },
    IntrEoi       { vector: u8 },
    IpcSend       { endpoint: u64 },
    IpcRecv       { endpoint: u64 },
    IpcCreate,
    FsMount       { path: u64 },
    FsRead        { path: u64, size: usize },
    FsWrite       { path: u64, size: usize },
    Reboot,
    Halt,
    CapGrant      { target: SubjectId, cap: CapId },
    CapRevoke     { target: SubjectId, cap: CapId },
    CapInspect    { target: SubjectId },
    Log           { len: usize },
    TerminalWrite { len: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDecision {
    Allow,
    Deny,
    /// Allow and emit an audit log entry. The kernel records the op in
    /// its audit ring buffer (future).
    Audit,
}

#[derive(Debug)]
pub enum CapError {
    /// Subject does not hold at least one of the required bits.
    Denied { subject: SubjectId, required: Caps, missing: Caps },
    /// Subject ID is outside the task table.
    UnknownSubject(SubjectId),
    /// CapId > 63.
    InvalidCap(CapId),
    /// No policy provider is installed (e.g. policy process not running).
    NoPolicyProvider,
    /// Dynamic policy hook returned Deny.
    PolicyDenied { subject: SubjectId, op: CapOp },
    /// Caller lacks `CAP_POLICY_WRITE` for grant/revoke.
    NotAuthorised,
}

// =====================================================================
// Kernel ↔ policy-process mailbox (reserved for future use)
// =====================================================================
//
// The mailbox protocol is reserved for a future policy process
// (Secure Runtime or similar). When implemented, it will use a single
// shared 4 KiB page mapped at fixed virtual addresses:
//
//   - kernel higher-half: 0xFFFF_A000_0000_0000
//   - policy-process address space: 0x0000_4000_0000_0000
//
// v1 uses static-only capability checks (no IPC). The mailbox
// types and constants are defined here so they don't need to be
// revisited when the policy process is added.

/// Fixed higher-half virtual address where the kernel maps the mailbox
/// page. We pick an address that's **outside** the kernel's existing
/// higher-half map (which covers `HIGHER_HALF + 0..4 GB` as 2 MB huge
/// pages). Splitting huge pages at runtime is complex, so we just use
/// a fresh slot beyond the physical-memory range.
pub const MAILBOX_KERNEL_VIRT: u64 = 0xFFFF_A000_0000_0000;

/// Fixed virtual address where a future policy process maps the mailbox
/// page (in its own address space). We pick an address that's **outside**
/// the identity map (which covers `0..4 GB` as 2 MB huge pages at
/// PML4[0x1FF]). This is a fresh PML4 entry (PML4[8]) — no conflict
/// with existing huge pages, so a future ELF loader can map it as a
/// 4 KB page.
pub const MAILBOX_POLICY_VIRT: u64 = 0x0000_4000_0000_0000;

/// CapOp discriminator — matches the order of the `CapOp` variants.
/// Used to serialise `CapOp` over the mailbox without depending on
/// `enum` layout (which is `non_exhaustive`).
pub mod cap_op_kind {
    pub const MM_ALLOC:        u32 = 0;
    pub const MM_MAP:          u32 = 1;
    pub const MM_UNMAP:        u32 = 2;
    pub const TASK_CREATE:     u32 = 3;
    pub const TASK_DESTROY:    u32 = 4;
    pub const TASK_PIN:        u32 = 5;
    pub const INTR_INSTALL:    u32 = 6;
    pub const INTR_MASK:       u32 = 7;
    pub const INTR_EOI:        u32 = 8;
    pub const IPC_SEND:        u32 = 9;
    pub const IPC_RECV:        u32 = 10;
    pub const IPC_CREATE:      u32 = 11;
    pub const FS_MOUNT:        u32 = 12;
    pub const FS_READ:         u32 = 13;
    pub const FS_WRITE:        u32 = 14;
    pub const REBOOT:          u32 = 15;
    pub const HALT:            u32 = 16;
    pub const CAP_GRANT:       u32 = 17;
    pub const CAP_REVOKE:      u32 = 18;
    pub const CAP_INSPECT:     u32 = 19;
    pub const LOG:             u32 = 20;
    pub const TERMINAL_WRITE:  u32 = 21;
}

/// Fixed-size serialised form of `CapOp`. The `kind` field is a
/// `cap_op_kind` constant. The remaining 56 bytes are a payload
/// (little-endian, host order — the kernel and policy process must agree).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapOpWire {
    pub kind: u32,
    pub payload: [u8; 56],
}

impl CapOpWire {
    pub const fn zeroed() -> Self {
        Self { kind: 0, payload: [0; 56] }
    }
}

/// Fixed-size request packet. Written by the kernel, read by the policy process.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapRequest {
    pub subject: u32,
    pub op: CapOpWire,
    /// Sequence number — policy process echoes this in the response so the
    /// kernel can match replies to outstanding requests.
    pub seq: u64,
    _pad: [u8; 16],
}

impl CapRequest {
    pub const fn zeroed() -> Self {
        Self {
            subject: 0,
            op: CapOpWire::zeroed(),
            seq: 0,
            _pad: [0; 16],
        }
    }
}

/// Fixed-size response packet. Written by the policy process, read by the kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapResponse {
    /// 0 = Allow, 1 = Deny, 2 = Audit.
    pub decision: u32,
    /// Echoed from the request — kernel matches on this.
    pub seq: u64,
    _pad: [u8; 48],
}

impl CapResponse {
    pub const fn zeroed() -> Self {
        Self {
            decision: 0,
            seq: 0,
            _pad: [0; 48],
        }
    }
}

/// The 4 KiB mailbox page. Both kernel and policy process map this and access
/// the same physical bytes.
///
/// Layout:
/// ```text
/// 0x000  request_ready:  AtomicU32  (kernel sets, policy process reads)
/// 0x004  response_ready: AtomicU32  (policy process sets, kernel reads)
/// 0x008  request:        CapRequest
/// 0x088  response:       CapResponse
/// ```
/// Fields are 8-byte aligned where needed. Total size of the struct
/// below must be ≤ 4096 bytes; it currently fits in ~256 bytes, leaving
/// the rest of the page for future expansion (e.g. additional channels).
#[repr(C)]
pub struct Mailbox {
    pub request_ready: AtomicU32,
    pub response_ready: AtomicU32,
    _pad0: [u8; 120],
    pub request: CapRequest,
    _pad1: [u8; 64],
    pub response: CapResponse,
    /// Trailing padding to fill the 4 KiB page. Sized by
    /// `4096 - (sum of fields above)`.
    _pad2: [u8; 3752],
}

impl Mailbox {
    pub const fn zeroed() -> Self {
        Self {
            request_ready: AtomicU32::new(0),
            response_ready: AtomicU32::new(0),
            _pad0: [0; 120],
            request: CapRequest::zeroed(),
            _pad1: [0; 64],
            response: CapResponse::zeroed(),
            _pad2: [0; 3752],
        }
    }
}

// Compile-time guard: the Mailbox must be exactly one 4 KiB page. If
// a field is added or resized without recomputing `_pad2`, the build
// will fail here. To regenerate `_pad2` after a change:
//
//     4096 - core::mem::size_of::<Mailbox_no_pad>() + actual_used
//
// or simply: `const _: [(); 4096 - core::mem::size_of::<Mailbox>(); 1] = [(); 1];`
const _: [(); 4096] = [(); core::mem::size_of::<Mailbox>()];
const _: () = {
    // The `Mailbox` size includes trailing padding. If a new field is
    // added, the explicit `3752` in `_pad2` will be wrong and the
    // build will fail. Update `_pad2` to keep the total at 4096.
    if core::mem::size_of::<Mailbox>() != 4096 {
        panic!("Mailbox size is not 4096 — update _pad2");
    }
};
