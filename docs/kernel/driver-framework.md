# Driver Framework (GDF)

The **G**eneric **D**river **F**ramework manages driver services, kernel-driver
IPC via mailboxes, and service lifecycle (start, crash, restart, stop).

---

## 1. GDF Core (`kernel/src/gdf.rs`)

### Constants

| Constant              | Value  | Description                    |
|-----------------------|--------|--------------------------------|
| `MAX_DRIVERS`         | `16`   | Max registered drivers         |
| `SERVICE_STACK_SIZE`  | `16384`| Driver ELF stack size          |
| `MAX_RESTARTS`        | `3`    | Default restart limit          |

### DriverClass

```rust
pub enum DriverClass {
    Hardware = 0,     // ring-0 driver with MMIO/PCI/DMA access
    Abstraction = 1,  // higher-level driver (filesystem, etc.)
}
```

### ServiceOrigin

```rust
pub enum ServiceOrigin {
    BakedIn,       // compiled into kernel
    DiskLoaded,    // loaded from filesystem
}
```

### ServiceKind

```rust
pub enum ServiceKind {
    BootCritical,  // must succeed for system to function
    Optional,      // can fail gracefully
}
```

### Mailbox

```c
struct Mailbox {
    cmd: u32,       // command identifier
    flags: u32,     // MAILBOX_IDLE=0, MAILBOX_PENDING=1, MAILBOX_RESPONSE=2
    arg0: u64,      // argument 0
    arg1: u64,      // argument 1
    arg2: u64,      // argument 2
    result: u64,    // response value
}
// Total: 32 bytes (6 × 8 = 48, but packed as repr(C))
```

### DriverEntry

```c
struct DriverEntry {
    name: [u8; 32],           // driver name, null-terminated
    vcpu_id: VcpuId,          // associated VCPU
    mailbox: Mailbox,         // kernel↔driver communication
    state: u8,                // driver state (1 = registered)
    vcpu_type: VcpuType,      // HardwareDriver or AbstractionDriver
    caller_vcpu: VcpuId,      // VCPU waiting for synchronous call response
    blocked_buf_ptr: u64,     // userspace buf for blocking recv
}
```

### Driver Table

```c
struct DriverTableData {
    entries: [Option<DriverEntry>; MAX_DRIVERS],  // 16 slots
    count: usize,
}
```

Protected by `IrqSaveSpinLock`.

### Public API

| Function | Description |
|----------|-------------|
| `register_driver(name, vcpu_id, vcpu_type) -> bool` | Add entry to driver table (checks duplicates) |
| `find_by_name(name) -> Option<usize>` | Look up driver index by name |
| `find_by_vcpu(vcpu_id) -> Option<usize>` | Look up driver index by VCPU ID |
| `poll_result(name) -> Option<u64>` | Read result from mailbox (if IDLE) |
| `send_cmd(name, cmd, arg0, arg1, arg2) -> bool` | Send command to driver mailbox, wake VCPU |
| `recv(vcpu_id) -> Option<(u32, u64, u64, u64)>` | Read pending message from driver's mailbox |
| `send_response(vcpu_id, result) -> bool` | Write response, wake caller |
| `driver_call(target_name, cmd, arg0, arg1, arg2, frame) -> bool` | Synchronous call to another driver (polls with yield) |
| `set_blocked_buf(vcpu_id, buf_ptr)` | Store user buffer for blocking recv |
| `track_service_mmio(vcpu_id, phys, virt, pages) -> bool` | Track MMIO region for service |
| `track_service_irq(vcpu_id, vector) -> bool` | Track IRQ line for service |
| `track_service_dma(vcpu_id, phys, pages) -> bool` | Track DMA buffer for service |

### Driver Package Loading

```c
pub fn init_from_package(package: &'static [u8])
```

Package format (defined in `lodaxos_system`):

```
[DriverPkgHeader]    12 bytes  (magic + count)
[DriverPkgEntry × N] N * 40 bytes (name[32] + class + elf_offset + elf_size)
[driver ELF 0 data]
[driver ELF 1 data]
...
```

For each entry:
1. Extract ELF slice from package.
2. Call `start_service(name, elf_data, class)`.

### Service Startup

```c
pub fn start_service(name: &[u8], binary: &[u8], class: DriverClass) -> Option<u32>
```

Sequence:
1. Fork PML4 from kernel PML4.
2. Load ELF binary into the new PML4 (`exec::load_elf`).
3. Allocate service table entry (`service::alloc`).
4. Allocate VCPU (`vcpu::alloc` with GANG_UNSCHEDULED).
5. Set initial register state (RIP=entry, RSP=stack_top, CS=0x08, SS=0x10,
   RFLAGS=0x202).
6. Register in driver table.
7. Create driver gang via `scheduler::register_driver_vcpu`.
8. Push VCPU to least-loaded CPU's ready queue.

### Crash Handling

```c
pub fn handle_crash(vcpu_id: VcpuId)
```

1. Mark service as `Crashed`, increment `restart_count`.
2. Clean up resources (unmap MMIO, free DMA pages).
3. Free VCPU and PML4.
4. If `restart_policy` allows: re-allocate VCPU+PML4, reload ELF, re-register.
5. Otherwise: mark service `Stopped`.

### Resource Cleanup

```c
fn cleanup_resources(id: u32)
```

- Unmaps all MMIO regions from the PML4.
- Frees all DMA buffers via `phys::free_pages`.

---

## 2. Service Lifecycle (`kernel/src/service.rs`)

### Constants

| Constant       | Value  | Description          |
|----------------|--------|----------------------|
| `MAX_SERVICES` | `32`   | Max registered services |
| `MAX_MMIO`     | `16`   | Max MMIO regions per service |
| `MAX_IRQ`      | `8`    | Max IRQ lines per service |
| `MAX_DMA`      | `8`    | Max DMA buffers per service |

### ServiceState

```rust
pub enum ServiceState {
    Loaded,      // PML4 allocated, ELF not yet loaded
    Running,     // VCPU scheduled and running
    Crashed,     // caught an exception
    Restarting,  // being re-initialised after crash
    Stopped,     // terminated, will not run again
}
```

### RestartPolicy

```rust
pub enum RestartPolicy {
    Always,             // restart on every crash
    OnFailure(u32),     // restart up to N times (default: 3)
    Never,              // never restart
}
```

### ServiceResources

```c
struct ServiceResources {
    mmio: [(u64, u64, u64); MAX_MMIO],    // (phys, virt, pages)
    mmio_count: usize,
    irq: [u8; MAX_IRQ],                    // IRQ vectors
    irq_count: usize,
    dma: [(u64, u64); MAX_DMA],           // (phys, pages)
    dma_count: usize,
}
```

### Service

```c
struct Service {
    id: u32,
    name: [u8; 32],
    state: ServiceState,
    vcpu_id: VcpuId,
    pml4: u64,
    resources: ServiceResources,      // private
    restart_policy: RestartPolicy,
    restart_count: u32,
}
```

### Public API

| Function | Description |
|----------|-------------|
| `service_init()` | Initialise service table |
| `service_alloc(name, pml4) -> Option<u32>` | Create service entry |
| `service_free(id)` | Remove service entry |
| `service_get(id) -> Option<&Service>` | Immutable reference |
| `service_with_mut(id, f) -> R` | Mutable access via closure |
| `service_find_by_vcpu(vcpu_id) -> Option<u32>` | Look up by VCPU |
| `service_find_by_name(name) -> Option<u32>` | Look up by name |
| `service_count() -> usize` | Number of active services |

Each Service tracks its own resources via `track_mmio`, `track_irq`,
`track_dma` methods. The `resources()` method returns an immutable
reference; `clear_resources()` resets all tracked resources to empty.

---

## 3. Syscall API

All syscalls use the `syscall` instruction. The kernel saves the full
`TrapFrame` and dispatches via `syscall_handler`.

### ABI

```
Register  Purpose
────────  ───────
rax       syscall number
rdi       arg0
rsi       arg1
rdx       arg2
r10       arg3
r8        arg4
r9        arg5
rcx       clobbered (RIP saved by syscall)
r11       clobbered (RFLAGS saved by syscall)
rax       return value (after iretq)
```

### Syscall Table

| Nr  | Name                | Access          | Description |
|-----|---------------------|-----------------|-------------|
| 0   | `yield`             | All non-idle    | Yield VCPU |
| 1   | `exit`              | All non-idle    | Halt VCPU |
| 2   | `get_vcpu_id`       | All non-idle    | Return VcpuId |
| 3   | `wake`              | All non-idle    | Wake another VCPU |
| 4   | `get_ticks`         | All non-idle    | Uptime ticks |
| 5   | `mmap`              | All non-idle    | Allocate + map anonymous pages |
| 6   | `munmap`            | All non-idle    | Unmap + free pages |
| 7   | `create_gang`       | All non-idle    | Spawn new VCPU gang |
| 10  | `mmap_phys`         | HW only         | Map MMIO (uncacheable) |
| 11  | `register_intr`     | HW only         | Register IRQ handler (stub) |
| 12  | `intr_ack`          | HW only         | Send EOI |
| 13  | `dma_alloc`         | HW only         | Allocate DMA buffer |
| 14  | `dma_free`          | HW only         | Free DMA buffer |
| 15  | `pci_config`        | HW only         | PCI config space R/W |
| 20  | `driver_recv`       | HW+AB           | Read mailbox (non-blocking) |
| 21  | `driver_send`       | HW+AB           | Write mailbox response |
| 22  | `driver_recv_block` | HW+AB           | Read mailbox (blocking) |
| 30  | `gdf_register`      | HW+AB           | Register driver name |
| 31  | `driver_call`       | HW+AB           | Synchronous cross-driver call |

### Syscall Details

**`sys_mmap` (nr 5)**
```
rdi = hint address (0 for auto)
rsi = size in bytes
→ rax = virtual address, or u64::MAX on failure
```
Allocates physical pages, maps them into the current PML4. If hint is 0,
maps at `HIGHER_HALF + phys`. Rejects hints >= HIGHER_HALF.

**`sys_munmap` (nr 6)**
```
rdi = virtual address
rsi = size in bytes
→ rax = 0 or u64::MAX
```
Unmaps and frees physical pages. Rejects addresses >= HIGHER_HALF.

**`sys_mmap_phys` (nr 10)**
```
rdi = physical address
rsi = size in bytes
→ rax = virtual address (identity-mapped), or u64::MAX
```
Maps physical memory with `PRESENT|WRITABLE|CACHE_DISABLE|NO_EXECUTE`.
Tracks the MMIO region for the calling service.

**`sys_dma_alloc` (nr 13)**
```
rdi = size in bytes
→ rax = physical address, or u64::MAX
```
Allocates contiguous physical pages, zeroes them, tracks for the service.

**`sys_dma_free` (nr 14)**
```
rdi = physical address
rsi = size in bytes
→ rax = 0 or u64::MAX
```
Frees DMA pages.

**`sys_pci_config` (nr 15)**
```
rdi = BDF (bus:device:function)
rsi = config offset (0..255, aligned to width)
rdx = width (1, 2, or 4)
r10 = value (for writes)
r8  = is_write (0 = read, non-zero = write)
→ rax = read value, or u64::MAX on error
```
Accesses PCI config space via I/O ports 0xCF8/0xCFC.

**`sys_driver_recv` (nr 20)**
```
rdi = buffer pointer (4 × u64, userspace)
→ rax = 0 on success (message written to buffer), u64::MAX if no message
```
Non-blocking: returns immediately with `u64::MAX` if no message pending.

**`sys_driver_send` (nr 21)**
```
rdi = result value
→ rax = 0 on success, u64::MAX if mailbox not found
```
Writes result to the driver's mailbox, sets flags to IDLE, wakes the caller
if `caller_vcpu` is set.

**`sys_driver_recv_block` (nr 22)**
```
rdi = buffer pointer (4 × u64, userspace)
→ rax = 0 (message written by kernel before wake)
```
Fast path: if a message is already pending, copies it and returns.
Slow path: saves `blocked_buf_ptr`, yields the VCPU (calls
`scheduler::block_current`). When a message arrives, `send_cmd` writes the
message directly to the buffer before waking the VCPU.

**`sys_gdf_register` (nr 30)**
```
rdi = name pointer (userspace)
rsi = name length (1..31)
→ rax = 0 on success, u64::MAX on failure
```
Copies the name from userspace, calls `gdf::register_driver`.

**`sys_driver_call` (nr 31)**
```
rdi = target name pointer (userspace)
rsi = target name length (1..31)
rdx = command
r10 = arg0
r8  = arg1
r9  = arg2
→ rax = result from target driver
```
Synchronous cross-driver call. Sends command via `gdf::driver_call`, which
polls for the response (yielding every 100 iterations) until the target
responds.

### Access Control

Defined in `kernel/src/arch/idt.rs:797`:

| VcpuType            | Allowed Syscall Ranges        |
|---------------------|-------------------------------|
| Idle                | None                          |
| Normal              | 0..7 (universal)              |
| HardwareDriver      | 0..7, 10..15, 20..22, 30..31 |
| AbstractionDriver   | 0..7, 20..22, 30..31         |

Disallowed syscalls return `u64::MAX` and log a warning.
