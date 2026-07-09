use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use crate::arch::idt::TrapFrame;
use crate::mm::{self, virt};
use crate::mm::vma::ProcessMemory;
use crate::percpu;
use crate::scheduler;
use crate::service::{self, ServiceState, RestartPolicy};
use crate::sync::{IrqSaveSpinLock, SyncUnsafeCell};
use crate::vcpu::{self, VcpuId, VcpuType, VcpuState, GANG_UNSCHEDULED, SendPtr};
use lodaxos_system::{DriverPkgHeader, DriverPkgEntry, DRIVER_PKG_MAGIC};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverClass {
    Hardware,
    Abstraction,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceOrigin {
    BakedIn,
    DiskLoaded,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceKind {
    BootCritical,
    Optional,
}

const MAX_DRIVERS: usize = 16;
const SERVICE_STACK_SIZE: u64 = 16384;
const MAX_RESTARTS: u32 = 3;

/// Maximum number of times a driver may be restarted before being declared
/// in a crash loop.  Once exceeded, the driver is stopped permanently.
const MAX_CRASH_COUNT: u32 = 5;

const MAILBOX_IDLE: u32 = 0;
const MAILBOX_PENDING: u32 = 1;
const MAILBOX_RESPONSE: u32 = 2;

/// Per-driver crash counters that survive across restarts (the service is
/// freed and re-created on each restart, so restart_count inside the
/// Service struct is not durable).  Indexed by driver name hash.
struct CrashCounter {
    name: [u8; 32],
    count: u32,
}

static CRASH_COUNTERS: IrqSaveSpinLock<[Option<CrashCounter>; MAX_DRIVERS]> =
    IrqSaveSpinLock::new([const { None }; MAX_DRIVERS]);

/// Increment and return the crash count for a driver name.
fn bump_crash_count(name: &[u8; 32]) -> u32 {
    let mut t = CRASH_COUNTERS.lock();
    for slot in t.iter_mut() {
        if let Some(c) = slot {
            if c.name == *name {
                c.count += 1;
                return c.count;
            }
        }
    }
    // First crash for this driver
    for slot in t.iter_mut() {
        if slot.is_none() {
            *slot = Some(CrashCounter { name: *name, count: 1 });
            return 1;
        }
    }
    1 // table full, allow this restart
}

/// Deferred crash-restart queue.  When a driver vCPU crashes, the restart
/// is pushed here instead of calling `start_service()` from exception
/// context (which is unsafe due to page-table modifications and lock
/// contention with other CPUs).  The BSP idle loop drains this queue.
struct DeferredRestart {
    name: [u8; 32],
    elf_data: &'static [u8],
    class: DriverClass,
}

const MAX_DEFERRED_RESTARTS: usize = 8;
static DEFERRED_RESTARTS: IrqSaveSpinLock<[Option<DeferredRestart>; MAX_DEFERRED_RESTARTS]> =
    IrqSaveSpinLock::new([const { None }; MAX_DEFERRED_RESTARTS]);

/// Push a restart request to the deferred queue.  Called from exception
/// context (GDF crash handler) instead of calling `start_service()` directly.
fn push_deferred_restart(name: &[u8; 32], elf_data: &'static [u8], class: DriverClass) {
    let mut q = DEFERRED_RESTARTS.lock();
    for slot in q.iter_mut() {
        if slot.is_none() {
            *slot = Some(DeferredRestart { name: *name, elf_data, class });
            log::info!("gdf: deferred restart for service queued");
            return;
        }
    }
    log::error!("gdf: deferred restart queue full -- restart dropped");
}

/// Process all pending deferred restarts.  Must be called from a safe
/// context (BSP idle loop) where heavy memory allocation is permitted.
pub fn process_deferred_restarts() {
    loop {
        let entry = {
            let mut q = DEFERRED_RESTARTS.lock();
            match q.iter_mut().find_map(|s| s.take()) {
                Some(e) => e,
                None => return,
            }
        };
        let name_str = core::str::from_utf8(&entry.name)
            .unwrap_or("?")
            .trim_end_matches('\0');
        log::info!("gdf: processing deferred restart for '{}'", name_str);
        match start_service(&entry.name, entry.elf_data, entry.class) {
            Some(id) => log::info!("gdf: deferred restart of '{}' succeeded (service {})", name_str, id),
            None => log::error!("gdf: deferred restart of '{}' failed", name_str),
        }
    }
}

static DRIVER_PACKAGE: SyncUnsafeCell<Option<&'static [u8]>> = SyncUnsafeCell::new(None);

pub struct DriverPkgMeta {
    name: [u8; 32],
    elf_data: &'static [u8],
    class: DriverClass,
}
// Make PKG_META public for try_init_from_package to work (via its TryLock)
pub static PKG_META: IrqSaveSpinLock<[Option<DriverPkgMeta>; MAX_DRIVERS]> =
    IrqSaveSpinLock::new([const { None }; MAX_DRIVERS]);

fn copy_driver_elf(src: &[u8]) -> Option<&'static [u8]> {
    let mut owned = Vec::new();
    if owned.try_reserve_exact(src.len()).is_err() {
        log::error!("gdf: out of memory copying driver ELF ({} bytes)", src.len());
        return None;
    }
    owned.extend_from_slice(src);
    Some(Box::leak(owned.into_boxed_slice()))
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Mailbox {
    pub cmd: u32,
    pub flags: u32,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub result: u64,
}

impl Mailbox {
    const fn empty() -> Self {
        Self { cmd: 0, flags: 0, arg0: 0, arg1: 0, arg2: 0, result: 0 }
    }
}

pub struct DriverEntry {
    pub name: [u8; 32],
    pub vcpu_id: VcpuId,
    pub mailbox: Mailbox,
    pub state: u8,
    pub vcpu_type: VcpuType,
    pub caller_vcpu: VcpuId,
    /// Virtual address of the driver's recv buffer, set by sys_driver_recv_block
    /// when no message is pending. When a message arrives, the kernel writes the
    /// command + args directly here before waking the driver.
    pub blocked_buf_ptr: u64,
    /// Physical address of the driver's PML4 (page table root).
    /// Used by send_cmd to verify blocked_buf_ptr is writable before writing.
    pub pml4: u64,
}

struct DriverTableData {
    entries: [Option<DriverEntry>; MAX_DRIVERS],
    count: usize,
}

static DRIVER_TABLE: IrqSaveSpinLock<DriverTableData> = IrqSaveSpinLock::new(DriverTableData {
    entries: [const { None }; MAX_DRIVERS],
    count: 0,
});

// -- Driver table operations ---------------------------------------

pub fn register_driver(name: &[u8], vcpu_id: VcpuId, vcpu_type: VcpuType) -> bool {
    let mut t = DRIVER_TABLE.lock();
    if t.count >= MAX_DRIVERS {
        log::error!("gdf: driver table exhausted (max {})", MAX_DRIVERS);
        return false;
    }
    let len = name.len().min(31);
    let mut entry_name = [0u8; 32];
    entry_name[..len].copy_from_slice(&name[..len]);
    for e in t.entries.iter() {
        if let Some(d) = e {
            if d.name == entry_name {
                log::error!("gdf: duplicate driver name");
                return false;
            }
        }
    }
    // Get the vCPU's PML4 so send_cmd can verify blocked_buf_ptr writability.
    let pml4 = match vcpu::get(vcpu_id) {
        Some(vcpu) => vcpu.pml4,
        None => {
            log::error!("gdf: register_driver vcpu {} not found", vcpu_id);
            return false;
        }
    };
    for slot in t.entries.iter_mut() {
        if slot.is_none() {
            *slot = Some(DriverEntry {
                name: entry_name,
                vcpu_id,
                mailbox: Mailbox::empty(),
                state: 1,
                vcpu_type,
                caller_vcpu: 0,
                blocked_buf_ptr: 0,
                pml4,
            });
            t.count += 1;
            log::info!("gdf: registered driver '{}' vcpu={}", core::str::from_utf8(&entry_name[..len]).unwrap_or("?"), vcpu_id);
            return true;
        }
    }
    false
}

/// Get a driver's name as a fixed byte array, or None if the slot is empty.
pub fn pkg_name(index: usize) -> Option<[u8; 32]> {
    let pkgs = PKG_META.lock();
    pkgs.get(index)?.as_ref().map(|p| p.name)
}

/// Get a driver's class name as &str, or None if the slot is empty.
pub fn pkg_class(index: usize) -> Option<&'static str> {
    let pkgs = PKG_META.lock();
    pkgs.get(index)?.as_ref().map(|p| match p.class {
        DriverClass::Hardware => "Hardware",
        DriverClass::Abstraction => "Abstraction",
    })
}

pub fn find_by_name(name: &[u8]) -> Option<usize> {
    let t = DRIVER_TABLE.lock();
    let len = name.len().min(31);
    for (i, slot) in t.entries.iter().enumerate() {
        if let Some(d) = slot {
            if d.name[..len] == name[..len] && (len == 31 || d.name[len] == 0) {
                return Some(i);
            }
        }
    }
    None
}

pub fn find_by_vcpu(vcpu_id: VcpuId) -> Option<usize> {
    let t = DRIVER_TABLE.lock();
    for (i, slot) in t.entries.iter().enumerate() {
        if let Some(d) = slot {
            if d.vcpu_id == vcpu_id {
                return Some(i);
            }
        }
    }
    None
}

pub fn poll_result(name: &[u8]) -> Option<u64> {
    let t = DRIVER_TABLE.lock();
    let len = name.len().min(31);
    for slot in t.entries.iter() {
        if let Some(d) = slot {
            if d.name[..len] == name[..len] && (len == 31 || d.name[len] == 0) {
                if d.mailbox.flags == MAILBOX_IDLE {
                    return Some(d.mailbox.result);
                }
                return None;
            }
        }
    }
    None
}

pub fn send_cmd(name: &[u8], cmd: u32, arg0: u64, arg1: u64, arg2: u64) -> bool {
    let wake_id;
    {
        let mut t = DRIVER_TABLE.lock();
        let len = name.len().min(31);
        let mut found = false;
        let mut id = 0u64;
        for slot in t.entries.iter_mut() {
            if let Some(d) = slot {
                if d.name[..len] == name[..len] && (len == 31 || d.name[len] == 0) {
                    if d.mailbox.flags != MAILBOX_IDLE {
                        return false;
                    }
                    d.mailbox.cmd = cmd;
                    d.mailbox.arg0 = arg0;
                    d.mailbox.arg1 = arg1;
                    d.mailbox.arg2 = arg2;
                    d.mailbox.result = u64::MAX;
                    d.mailbox.flags = MAILBOX_PENDING;
                    // If the driver is blocked on sys_driver_recv_block, write
                    // the message directly to its saved buffer so it's ready
                    // when the VCPU is woken and resumed.
                    // Validate blocked_buf_ptr is in user-accessible range
                    // before dereferencing to prevent kernel memory corruption.
                    if d.blocked_buf_ptr != 0 {
                        if d.blocked_buf_ptr >= crate::mm::virt::HIGHER_HALF {
                            log::error!("gdf: send_cmd blocked_buf_ptr {:#x} is above HIGHER_HALF", d.blocked_buf_ptr);
                            d.blocked_buf_ptr = 0;
                            return false;
                        }
                        // Verify each page covering the 32-byte write is present
                        // and writable in the driver's page table to prevent a
                        // kernel page fault if the driver unmapped the buffer.
                        let buf_start = d.blocked_buf_ptr;
                        let buf_end = buf_start + 32;
                        let mut writable = true;
                        let mut page = buf_start & !0xFFF;
                        while page < buf_end {
                            if let Some(pte) = crate::mm::virt::read_pte(d.pml4, page) {
                                if pte & crate::mm::virt::PRESENT == 0
                                    || pte & crate::mm::virt::WRITABLE == 0
                                {
                                    writable = false;
                                    break;
                                }
                            } else {
                                writable = false;
                                break;
                            }
                            page += 0x1000;
                        }
                        if !writable {
                            log::warn!("gdf: send_cmd blocked_buf_ptr {:#x} not writable, dropping message", buf_start);
                            d.blocked_buf_ptr = 0;
                            return false;
                        }
                        let out = [cmd as u64, arg0, arg1, arg2];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                out.as_ptr(),
                                d.blocked_buf_ptr as *mut u64,
                                4,
                            );
                        }
                        d.blocked_buf_ptr = 0;
                    }
                    id = d.vcpu_id as u64;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return false;
        }
        wake_id = id;
    }
    // DRIVER_TABLE lock released before wake (Bug 5 fix:
    // wake acquires GANG_TABLE -- holding DRIVER_TABLE while
    // waiting on GANG_TABLE would create a lock-order inversion).
    scheduler::wake(wake_id);
    true
}

pub fn recv(vcpu_id: VcpuId) -> Option<(u32, u64, u64, u64)> {
    let mut t = DRIVER_TABLE.lock();
    for slot in t.entries.iter_mut() {
        if let Some(d) = slot {
            if d.vcpu_id == vcpu_id && d.mailbox.flags == MAILBOX_PENDING {
                let result = (d.mailbox.cmd, d.mailbox.arg0, d.mailbox.arg1, d.mailbox.arg2);
                d.mailbox.flags = MAILBOX_RESPONSE;
                return Some(result);
            }
        }
    }
    None
}

pub fn send_response(vcpu_id: VcpuId, result: u64) -> bool {
    let mut caller = 0u64;
    let mut found = false;
    {
        let mut t = DRIVER_TABLE.lock();
        for slot in t.entries.iter_mut() {
            if let Some(d) = slot {
                if d.vcpu_id == vcpu_id {
                    d.mailbox.result = result;
                    d.mailbox.flags = MAILBOX_IDLE;
                    caller = d.caller_vcpu as u64;
                    d.caller_vcpu = 0;
                    found = true;
                    break;
                }
            }
        }
    }
    // DRIVER_TABLE lock released before wake (Bug 17 fix:
    // wake acquires GANG_TABLE -- holding DRIVER_TABLE while
    // waiting on GANG_TABLE would create a lock-order inversion).
    if caller != 0 {
        crate::vcpu::with_mut(caller as VcpuId, |v| {
            if let Some(vcpu) = v {
                vcpu.saved_frame.rax = result;
            }
        });
        crate::scheduler::wake(caller);
    }
    found
}

/// Send a command to another driver and wait until it responds.
/// Returns the result value from the target driver's `sys_driver_send`.
pub fn driver_call(target_name: &[u8], cmd: u32, arg0: u64, arg1: u64, arg2: u64, frame: &mut TrapFrame) -> bool {
    let target_padded = {
        let mut buf = [0u8; 32];
        let l = target_name.len().min(31);
        if l == 0 { return false; }
        buf[..l].copy_from_slice(&target_name[..l]);
        buf
    };

    // Phase 1: send command while holding GDF lock briefly
    let target_vcpu = {
        let mut t = DRIVER_TABLE.lock();
        let caller_vcpu = crate::scheduler::current_vcpu_id();
        let mut found = None;
        for slot in t.entries.iter_mut() {
            if let Some(d) = slot {
                if d.name == target_padded && d.vcpu_id != caller_vcpu {
                    if d.mailbox.flags != MAILBOX_IDLE {
                        return false;
                    }
                    d.mailbox.cmd = cmd;
                    d.mailbox.arg0 = arg0;
                    d.mailbox.arg1 = arg1;
                    d.mailbox.arg2 = arg2;
                    d.mailbox.flags = MAILBOX_PENDING;
                    d.caller_vcpu = caller_vcpu;
                    found = Some(d.vcpu_id);
                    break;
                }
            }
        }
        found
    };

    let Some(target) = target_vcpu else { return false };

    // Wake the target driver
    crate::scheduler::wake(target as u64);

    // Phase 2: block the caller vCPU until the target responds.
    // send_response() will write the result to frame.rax and wake us.
    crate::scheduler::block_current(frame);
    true
}

// -- Resource tracking ---------------------------------------------

pub fn track_service_mmio(vcpu_id: VcpuId, phys: u64, virt: u64, pages: u64) -> bool {
    let id = match service::find_by_vcpu(vcpu_id) {
        Some(id) => id,
        None => return false,
    };
    service::with_mut(id, |s| {
        match s {
            Some(svc) => svc.track_mmio(phys, virt, pages),
            None => false,
        }
    })
}

pub fn track_service_irq(vcpu_id: VcpuId, vector: u8) -> bool {
    let id = match service::find_by_vcpu(vcpu_id) {
        Some(id) => id,
        None => return false,
    };
    service::with_mut(id, |s| {
        match s {
            Some(svc) => svc.track_irq(vector),
            None => false,
        }
    })
}

pub fn track_service_dma(vcpu_id: VcpuId, phys: u64, pages: u64) -> bool {
    let id = match service::find_by_vcpu(vcpu_id) {
        Some(id) => id,
        None => return false,
    };
    service::with_mut(id, |s| {
        match s {
            Some(svc) => svc.track_dma(phys, pages),
            None => false,
        }
    })
}

pub fn untrack_service_dma(vcpu_id: VcpuId, phys: u64) -> Option<u64> {
    let id = match service::find_by_vcpu(vcpu_id) {
        Some(id) => id,
        None => return None,
    };
    service::with_mut(id, |s| {
        match s {
            Some(svc) => {
                for i in 0..svc.resources().dma_count {
                    if svc.resources().dma[i].0 == phys {
                        let pages = svc.resources().dma[i].1;
                        svc.untrack_dma(phys);
                        return Some(pages);
                    }
                }
                None
            }
            None => None,
        }
    })
}

/// Store the user buffer pointer for a blocked driver recv.
/// The kernel will write the pending message to this buffer directly
/// when `send_cmd` finds the VCPU in Blocked state.
pub fn set_blocked_buf(vcpu_id: VcpuId, buf_ptr: u64) {
    let mut t = DRIVER_TABLE.lock();
    for slot in t.entries.iter_mut() {
        if let Some(d) = slot {
            if d.vcpu_id == vcpu_id {
                d.blocked_buf_ptr = buf_ptr;
                return;
            }
        }
    }
}

// -- GDF lifecycle -------------------------------------------------

/// Parse the driver package manifest and start each driver as its own service.
///
/// The package format (defined in `system` crate):
///   [DriverPkgHeader]    12 bytes
///   [DriverPkgEntry × N] N * 40 bytes
///   [driver ELF 0 data]
///   [driver ELF 1 data]
///   ...
pub fn try_init_from_package(package: &'static [u8]) -> bool {
    // Only lock if we can acquire it immediately
    let Some(mut meta_lock) = PKG_META.try_lock() else { return false; };
    
    // Continue with existing init logic (internal helper)
    internal_init_from_package(package, &mut meta_lock);
    true
}

pub fn init_from_package(package: &'static [u8]) {
    let mut meta_lock = PKG_META.lock();
    internal_init_from_package(package, &mut meta_lock);
}

fn internal_init_from_package(package: &'static [u8], meta_lock: &mut [Option<DriverPkgMeta>; MAX_DRIVERS]) {
    unsafe { *DRIVER_PACKAGE.get() = Some(package); }
    log::info!("gdf: parsing driver package ({} bytes)", package.len());

    if package.len() < core::mem::size_of::<DriverPkgHeader>() {
        log::error!("gdf: package too small for header");
        return;
    }

    let hdr: DriverPkgHeader = unsafe { ptr::read_unaligned(package.as_ptr() as *const DriverPkgHeader) };
    if hdr.magic != DRIVER_PKG_MAGIC {
        log::error!("gdf: bad package magic");
        return;
    }

    let count = hdr.count as usize;
    let header_size = core::mem::size_of::<DriverPkgHeader>();
    let entries_size = count * core::mem::size_of::<DriverPkgEntry>();
    let manifest_end = header_size + entries_size;

    if manifest_end > package.len() {
        log::error!("gdf: package truncated (manifest claims {} entries)", count);
        return;
    }

    let entries: &[DriverPkgEntry] = unsafe {
        core::slice::from_raw_parts(
            package.as_ptr().add(header_size) as *const DriverPkgEntry,
            count,
        )
    };

    let mut started = 0u32;
    for (i, entry) in entries.iter().enumerate() {
        let elf_start = entry.elf_offset as usize;
        let elf_end = elf_start + entry.elf_size as usize;
        if elf_end > package.len() {
            log::error!("gdf: driver {} ELF out of bounds", i);
            continue;
        }
        let elf_src: &'static [u8] = unsafe {
            core::slice::from_raw_parts(package.as_ptr().add(elf_start), entry.elf_size as usize)
        };

        let class = match entry.class {
            0 => DriverClass::Hardware,
            1 => DriverClass::Abstraction,
            _ => {
                log::warn!("gdf: driver {} class {} not handled -- skipping", i, entry.class);
                continue;
            }
        };

        let name = &entry.name;
        let name_str = core::str::from_utf8(name).unwrap_or("?").trim_end_matches('\0');
        log::info!("gdf: starting driver '{}' (class={:?}, elf={} bytes)", name_str, class, entry.elf_size);

        let Some(elf_data) = copy_driver_elf(elf_src) else {
            log::error!("gdf: failed to copy driver '{}' ELF", name_str);
            continue;
        };

        match start_service(name, elf_data, class) {
            Some(id) => {
                // Store metadata for crash restart
                if i < MAX_DRIVERS {
                    let mut n = [0u8; 32];
                    let len = name.iter().position(|&c| c == 0).unwrap_or(32).min(31);
                    n[..len].copy_from_slice(&name[..len]);
                    meta_lock[i] = Some(DriverPkgMeta {
                        name: n,
                        elf_data,
                        class,
                    });
                }
                log::info!("gdf: driver '{}' started as service {}", name_str, id);
                started += 1;
            }
            None => log::error!("gdf: failed to start driver '{}'", name_str),
        }
    }
    log::info!("gdf: {}/{} drivers started", started, count);
}

/// Start a service: fork PML4, load ELF, create vCPU, push to ready queue.
/// Drivers no longer receive framebuffer args via registers -- they get
/// their configuration through GDF mailbox commands (e.g. FB_CMD_ACQUIRE).
pub fn start_service(name: &[u8], binary: &[u8], class: DriverClass) -> Option<u32> {
    let vcpu_type = match class {
        DriverClass::Hardware => VcpuType::HardwareDriver,
        DriverClass::Abstraction => VcpuType::AbstractionDriver,
    };
    let service_pml4 = mm::virt::fork_pml4(mm::virt::kernel_pml4())?;

    let mut proc_mem = ProcessMemory::new(service_pml4);

    let result = crate::exec::load_elf(binary, SERVICE_STACK_SIZE, Some(service_pml4), Some(&mut proc_mem))
        .map_err(|e| {
            log::error!("gdf: failed to load ELF: {:?}", e);
            // Free the forked PML4 and all its page-table structure pages.
            // The physical pages mapped by ELF segments were already freed by
            // load_elf's error path, but the page-table pages themselves are
            // not -- without this, they leak along with any intermediate
            // PDP/PD/PT tables created during mapping.
            mm::virt::free_pml4(service_pml4);
        })
        .ok()?;

    let service_id = service::alloc(name, service_pml4)?;

    let vcpu_id = vcpu::alloc(GANG_UNSCHEDULED, service_pml4, !0, vcpu_type)?;

    // Attach ProcessMemory to the vCPU for demand paging and mmap VMA tracking.
    let proc_mem_ptr = Box::into_raw(Box::new(proc_mem));
    vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.process_mem = SendPtr(proc_mem_ptr);
            vcpu.saved_frame.rip = result.entry;
            vcpu.saved_frame.rsp = result.stack_top - 8;
            vcpu.saved_frame.cs = 0x1B;
            vcpu.saved_frame.ss = 0x23;
            vcpu.saved_frame.rflags = 0x202;
            vcpu.saved_frame.rdi = 0;
            vcpu.saved_frame.rsi = 0;
            vcpu.saved_frame.rdx = 0;
            // Stamp the canary so the scheduler can detect corruption.
            vcpu.frame_magic = crate::vcpu::FRAME_MAGIC;
            vcpu.state = VcpuState::Ready;
            // kernel_stack_top is set by register_driver_vcpu below
        }
    });

    service::with_mut(service_id, |s| {
        if let Some(svc) = s {
            svc.vcpu_id = vcpu_id;
            svc.state = ServiceState::Running;
        }
    });

    register_driver(name, vcpu_id, vcpu_type);

    if !crate::scheduler::register_driver_vcpu(vcpu_id) {
        log::error!("gdf: failed to register driver Vcpu {} in gang", vcpu_id);
        return None;
    }

    let best_cpu = percpu::find_least_loaded();
    percpu::rq(best_cpu).push(vcpu_id as usize);
    percpu::set_task_count(best_cpu, percpu::task_count(best_cpu) + 1);

    log::info!(
        "gdf: started '{}' service_id={} vcpu_id={} pml4={:#x} entry={:#x}",
        core::str::from_utf8(name).unwrap_or("?"),
        service_id, vcpu_id, service_pml4, result.entry,
    );

    Some(service_id)
}

/// Stop a service: mark Stopped and clean up resources.
pub fn stop_service(id: u32) {
    service::with_mut(id, |s| {
        if let Some(svc) = s {
            svc.state = ServiceState::Stopped;
        }
    });
    cleanup_resources(id);
    log::info!("gdf: service {} stopped", id);
}

// -- Crash handling ------------------------------------------------

pub fn handle_crash(vcpu_id: VcpuId) {
    let service_id = match service::find_by_vcpu(vcpu_id) {
        Some(id) => id,
        None => return,
    };

    log::warn!("gdf: service {} (vcpu {}) crashed", service_id, vcpu_id);

    let (restart_policy, restart_count) = service::with_mut(service_id, |s| {
        if let Some(svc) = s {
            svc.state = ServiceState::Crashed;
            svc.restart_count += 1;
            (svc.restart_policy, svc.restart_count)
        } else {
            (RestartPolicy::Never, 0)
        }
    });

    let service_name = service::with_mut(service_id, |s| match s {
        Some(svc) => {
            let mut buf = [0u8; 32];
            let len = svc.name.iter().position(|&c| c == 0).unwrap_or(32);
            buf[..len].copy_from_slice(&svc.name[..len]);
            buf
        }
        None => [0u8; 32],
    });

    cleanup_resources(service_id);

    // Remove stale DRIVER_TABLE entry (Bug 20).
    {
        let mut dt = DRIVER_TABLE.lock();
        for slot in dt.entries.iter_mut() {
            if let Some(d) = slot {
                if d.vcpu_id == vcpu_id {
                    *slot = None;
                    dt.count = dt.count.saturating_sub(1);
                    break;
                }
            }
        }
    }

    let old_pml4 = service::with_mut(service_id, |s| {
        match s {
            Some(svc) => {
                let p = svc.pml4;
                svc.pml4 = 0;
                p
            }
            None => 0,
        }
    });

    // Mark the vCPU as Terminated and remove it from the gang table.
    // The actual vcpu::free and virt::free_pml4 are deferred to the
    // scheduler's cleanup pass so that no other CPU can context_switch
    // to a freed vCPU (Bug 25).
    {
        let mut gt = crate::scheduler::GANG_TABLE.lock();
        vcpu::with_mut(vcpu_id, |v| {
            if let Some(vcpu) = v {
                vcpu.state = VcpuState::Terminated;
                // Stash the PML4 so the scheduler can free it.
                vcpu.pml4 = old_pml4;
            }
        });
        for gang_opt in gt.gangs.iter_mut() {
            if let Some(gang) = gang_opt {
                let mut all_empty = true;
                for slot in gang.vcpu_ids.iter_mut() {
                    if *slot == Some(vcpu_id) {
                        *slot = None;
                        gang.vcpu_count = gang.vcpu_count.saturating_sub(1);
                    }
                    if slot.is_some() {
                        all_empty = false;
                    }
                }
                if all_empty {
                    *gang_opt = None;
                }
            }
        }
    }
    // vcpu::free and virt::free_pml4 are now deferred to the scheduler.

    let should_restart = match restart_policy {
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure(n) => restart_count <= n,
        RestartPolicy::Never => false,
    };

    // Crash-loop detection: track crashes per driver name across restarts.
    // The Service struct is freed/recreated on each restart, so its
    // restart_count is not durable.  bump_crash_count() maintains a
    // persistent counter that survives service reallocation.
    let crash_count = bump_crash_count(&service_name);
    let in_crash_loop = crash_count > MAX_CRASH_COUNT;

    if in_crash_loop {
        let name = core::str::from_utf8(&service_name).unwrap_or("?").trim_end_matches('\0');
        log::error!(
            "gdf: service '{}' in crash loop ({} crashes) -- stopping permanently",
            name, crash_count
        );
        service::with_mut(service_id, |s| {
            if let Some(svc) = s {
                svc.state = ServiceState::Stopped;
            }
        });
        return;
    }

    if should_restart {
        // Look up the per-driver metadata to find its ELF data and class
        let meta = {
            let meta_guard = PKG_META.lock();
            meta_guard.iter().find_map(|m| {
                m.as_ref().and_then(|meta| {
                    if meta.name == service_name {
                        Some((meta.elf_data, meta.class))
                    } else {
                        None
                    }
                })
            })
        };
        if let Some((bin, class)) = meta {
            let name = core::str::from_utf8(&service_name).unwrap_or("?").trim_end_matches('\0');
            log::info!("gdf: deferring restart for service {} (crash #{})", name, crash_count);
            service::with_mut(service_id, |s| {
                if let Some(svc) = s {
                    svc.state = ServiceState::Restarting;
                    svc.clear_resources();
                }
            });
            service::free(service_id);
            // Push to deferred queue instead of calling start_service()
            // directly.  start_service() does heavy page-table work and
            // lock acquisition that is unsafe in exception context.
            push_deferred_restart(&service_name, bin, class);
            return;
        }
    }

    let name = core::str::from_utf8(&service_name).unwrap_or("?").trim_end_matches('\0');
    log::error!("gdf: service {} (restarts={}) stopped permanently", name, restart_count);
    service::with_mut(service_id, |s| {
        if let Some(svc) = s {
            svc.state = ServiceState::Stopped;
        }
    });
}

// -- Resource cleanup ----------------------------------------------

fn cleanup_resources(id: u32) {
    let (pml4, mmio_entries, mmio_cnt, dma_entries, dma_cnt) = service::with_mut(id, |s| {
        let svc = match s {
            Some(svc) => svc,
            None => return (0, [(0, 0, 0); 16], 0usize, [(0, 0); 8], 0usize),
        };
        let p = svc.pml4;
        let mc = svc.resources().mmio_count;
        let mut mm = [(0u64, 0u64, 0u64); 16];
        mm[..mc].copy_from_slice(&svc.resources().mmio[..mc]);
        let dc = svc.resources().dma_count;
        let mut dm = [(0u64, 0u64); 8];
        dm[..dc].copy_from_slice(&svc.resources().dma[..dc]);
        svc.clear_resources();
        (p, mm, mc, dm, dc)
    });

    if pml4 != 0 {
        for i in 0..mmio_cnt {
            let (_phys, virt, pages) = mmio_entries[i];
            if virt != 0 && pages > 0 {
                for p in 0..pages {
                    virt::unmap(virt + p * 0x1000);
                }
            }
        }
    }

    for i in 0..dma_cnt {
        let (phys, pages) = dma_entries[i];
        if phys != 0 && pages > 0 {
            mm::phys::free_pages(phys, pages);
        }
    }

    log::trace!("gdf: cleaned up service {} ({} mmio, {} dma)", id, mmio_cnt, dma_cnt);
}

// -- Exception handler helper --------------------------------------

pub fn switch_frame_to_idle(frame: &mut TrapFrame) {
    let cpu = scheduler::current_cpu_slot();
    let idle_id = percpu::idle_vcpu(cpu);
    vcpu::with_mut(idle_id, |v| {
        if let Some(idle) = v { *frame = idle.saved_frame; }
    });
    // Update the current vCPU to idle so that subsequent exceptions
    // are not misidentified as driver crashes (which would cause an
    // infinite exception loop: crash -> switch_to_idle -> crash -> ...).
    percpu::set_current_vcpu(cpu, idle_id as usize);
}

/// Switch the current CPU's frame to the next ready driver vCPU.
/// Returns `(pml4, kernel_stack_top)` for the selected vCPU so the caller
/// can switch CR3 and update TSS.rsp0 before iretq.
pub fn switch_frame_to_next_driver(frame: &mut TrapFrame) -> Option<(u64, u64)> {
    let cpu = scheduler::current_cpu_slot();
    let rq = percpu::rq(cpu);
    // Peek at the queue to find a driver vCPU without draining non-drivers
    let mut checked = 0usize;
    let max_check = 64; // prevent infinite loop on corrupted queue

    // First pass: look for a driver vCPU by scanning the queue
    // We can't peek by index, so we pop and re-push non-drivers
    let mut temp_buf: [usize; 64] = [0; 64];
    let mut temp_len = 0usize;

    while let Some(next_id) = rq.pop() {
        let next_id_u32 = next_id as VcpuId;
        let vtype = vcpu::get_vcpu_type(next_id_u32);
        let is_driver = vtype == VcpuType::HardwareDriver || vtype == VcpuType::AbstractionDriver;

        if is_driver {
            // Try to load this driver's frame
            let loaded = vcpu::with_mut(next_id_u32, |v| {
                if let Some(vcpu) = v {
                    if vcpu.state != VcpuState::Ready {
                        return None;
                    }
                    // Validate the frame before loading — mirrors the check
                    // in schedule_inner.  A freshly allocated vCPU may still
                    // have the default frame (rsp=0, cs=0x08) if start_service
                    // hasn't finished initialising it yet.
                    let f = &vcpu.saved_frame;
                    let cs_ring = f.cs & 3;
                    let cs_valid = f.cs == 0x08 || f.cs == 0x1B;
                    let ss_valid = match cs_ring {
                        0 => f.ss == 0x10 || f.ss == 0x18,
                        3 => f.ss == 0x23 || f.ss == 0x2B,
                        _ => false,
                    };
                    let rip_canonical = f.rip == 0
                        || (f.rip as i64 >> 47) == -1
                        || (f.rip >> 47) == 0;
                    if f.rsp == 0 || !cs_valid || !ss_valid || !rip_canonical {
                        log::error!(
                            "gdf: skipping driver vCPU {} with invalid frame: CS={:#x} RSP={:#x} RIP={:#x} SS={:#x}",
                            next_id_u32, f.cs, f.rsp, f.rip, f.ss
                        );
                        return None;
                    }
                    vcpu.state = VcpuState::Running;
                    // Clear the canary so a double-load is caught.
                    vcpu.frame_magic = 0;
                    *frame = vcpu.saved_frame;
                    Some((vcpu.pml4, vcpu.kernel_stack_top))
                } else {
                    None
                }
            });
            if let Some((pml4, kstack_top)) = loaded {
                // Push back any non-drivers we popped
                for &id in &temp_buf[..temp_len] {
                    rq.push(id);
                }
                percpu::set_current_vcpu(cpu, next_id_u32 as usize);
                log::info!(
                    "gdf: CPU{} recovered to driver vCPU {} pml4={:#x} kstack={:#x}",
                    cpu, next_id_u32, pml4, kstack_top
                );
                return Some((pml4, kstack_top));
            }
            // Driver was dead/invalid -- push back temps and continue
            for &id in &temp_buf[..temp_len] {
                rq.push(id);
            }
            temp_len = 0;
        } else {
            // Not a driver -- save for later re-push
            if temp_len < temp_buf.len() {
                temp_buf[temp_len] = next_id;
                temp_len += 1;
            } else {
                // Buffer full -- push back immediately to avoid
                // permanently losing this vCPU from the scheduler.
                rq.push(next_id);
            }
        }

        checked += 1;
        if checked >= max_check {
            break;
        }
    }

    // Push back all non-drivers we popped
    for &id in &temp_buf[..temp_len] {
        rq.push(id);
    }

    None
}
