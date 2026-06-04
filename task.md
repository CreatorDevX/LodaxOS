# LodaxOS — Active Work Plan

Last updated: 2026-06-02

## Constraints

- `MAX_CPUS = 4` (compile-time); QEMU runs with `-smp 2` for testing.
- SMP via **UEFI MP Services** — no real mode, no INIT/SIPI, no 16-bit stub.
- **NO** ring-3, **NO** syscalls, **NO** init processes, **NO** fork/COW/spawn_elf.
- Kernel is **one piece** — no Linux-style init hierarchy.
- **Mechanism → kernel**, **Policy → ExRun**.
- Renames: `Secure Runtime` → `Executive Runtime`; `SR` → `ExRun`; `sr/` → `exrun/`; `sr.elf` → `exrun.elf`; `sr_loader.rs` → `exrun_loader.rs`; `sr_image_addr`/`sr_image_size` → `exrun_image_addr`/`exrun_image_size`.
- Full capability system **now** (mechanism in kernel, policy in ExRun).

---

## Phase 0 — ExRun rename (mechanical)

> **Status: completed (2026-06-03).** Items 1-12 below were the rename plan.
> Note that the final implementation uses `kernel/src/exec.rs` (named
> `mod exec;`, with entry point `exec::load(&info)`) rather than
> `kernel/src/exrun_loader.rs` — the loader is a full ELF loader +
> PML4 fork + shared-mailbox mapper + Task create (not just a symbol
> lookup). See `kernel/src/exec.rs` for the actual contract. Doc text
> below is preserved as the historical plan.

1. **Filesystem**:
   - `sr/` → `exrun/`
   - `sr/Cargo.toml` → `exrun/Cargo.toml` (rename `name = "lodaxos-sr"` → `name = "lodaxos-exrun"`)
   - `sr/target.json` → `exrun/target.json`
   - `sr/linker.ld` → `exrun/linker.ld`
   - `kernel/src/sr_loader.rs` → `kernel/src/exrun_loader.rs` *(actual: `kernel/src/exec.rs`)*
   - `sr.elf` → `exrun.elf` (output binary)

2. **Workspace** (`Cargo.toml`): `"sr"` → `"exrun"`.

3. **`system/src/lib.rs`**: `BootInfo.sr_image_addr` → `exrun_image_addr`; `sr_image_size` → `exrun_image_size`; doc comments "Secure Runtime" → "Executive Runtime".

4. **`kernel/src/main.rs`** (`_start`): `mod sr_loader;` → `mod exrun_loader;` *(actual: `mod exec;`)*; `sr_loader::load_sr(&info)` → `exrun_loader::load(&info)` *(actual: `exec::load(&info)`)*.

5. **`kernel/src/exec.rs`** (actual file, not the planned `exrun_loader.rs`): `pub fn load(info: &BootInfo) -> Option<usize>` — full ELF64 loader: forks the kernel's PML4, maps a shared 4 KiB mailbox into the new PML4 at a fixed address, parses the ELF, maps each `PT_LOAD` segment, allocates a fresh kernel stack, creates a `Task` with `e_entry` as RIP and `RDI = mailbox_virt_in_process_space`. Returns the new task id. See the file's module docstring for the full contract.

6. **`boot/src/main.rs`**: `let sr_elf_data = load_kernel::load_sr_from_ext4()` → `load_exrun_from_ext4`; `boot_info.sr_image_addr/sr_image_size` → `exrun_image_addr/size`; log strings updated.

7. **`boot/src/load_kernel.rs`**: `load_sr_from_ext4` → `load_exrun_from_ext4`; `load_file_from_ext4(b"sr.elf")` → `b"exrun.elf"`.

8. **`chain/src/main.rs`**: `sr_image_addr: 0, sr_image_size: 0` → `exrun_image_addr: 0, exrun_image_size: 0`.

9. **`build.bat`**: `lodaxos-sr` → `lodaxos-exrun`; `target\target\debug\deps\lodaxos_sr-*` → `lodaxos_exrun-*`; `copy … sr.elf` → `exrun.elf`.

10. **`create_disk_image.py`**: `sr.elf` → `exrun.elf`; log strings; `ext4_part.img` paths.

11. **Docs** (`docs/architecture/*.md`, `docs/ARCHITECTURE.md`, `bootupdetails.md`, `memory.md`, `memsave.md`, `idea.md`, `.gitignore`): all `Secure Runtime` / `SR` / `SecureRuntime` / `secure-runtime` → `Executive Runtime` / `ExRun` / `ExecutiveRuntime` / `executive-runtime`. **Exception**: avoid blind search-and-replace (e.g. "segment register", "set rate" contain "SR"). `HF_SR_*` HealthFlag names → `HF_EXRUN_*` for consistency (no on-disk records exist yet).

12. **`Cargo.lock`**: regenerated automatically by next build.

**Verification**: `cargo +nightly check` on all 6 crates (now `system, shared, chain, boot, kernel, exrun`); `build.bat` produces `exrun.elf`; QEMU boots.

---

## Phase 1 — SMP (UEFI MP Services)

### 1.1 Boot crate (`boot/src/mp.rs` + `boot/src/mp_trampoline.rs`)

13. Locate `MpServicesProtocol`:
    ```rust
    use uefi::proto::mp::MpServicesProtocol;
    let mp = boot_services().locate_protocol::<MpServicesProtocol>();
    ```

14. Call `get_number_of_processors()` → `(cpu_count, enabled_count)`. Assert `enabled_count <= MAX_CPUS (=4)`.

15. For each `ProcessorNumber ∈ 1..enabled_count`:
    - Allocate one `ApArg` (in UEFI-allocated boot-services memory — survives `ExitBootServices`).
    - Call `get_processor_info(proc_num, &mut info)` → get `info.proc_id` (LAPIC ID).
    - Allocate a 16 KiB kernel stack (UEFI `allocate_pages`).
    - The kernel PML4 is allocated by `boot` and recorded; the GDT and IDT are kernel-side and built by the kernel at `_start` — so the trampoline needs to load them from a known address passed via `ApArg`.

16. **`extern "efiapi" fn ap_trampoline(arg: *mut ApArg)`** (in `boot/src/mp_trampoline.rs`, `#![no_std]`, `extern "efiapi"`):
    - `cli`
    - `mov cr3, [arg + 0x00]`
    - `lgdt  [arg + 0x08]`
    - `lidt  [arg + 0x10]`
    - `mov rsp, [arg + 0x18]`
    - `mov dword [arg + 0x28], 1` (`ready = 1`)
    - Spin on `[arg + 0x30]`
    - `jmp  [arg + 0x20]`
    - Single ASM function, no Rust dependencies.

17. **Layout of `ApArg`**:
    ```rust
    struct ApArg {
        target_pml4_phys:    u64,    // 0x00
        target_gdt_ptr:      u64,    // 0x08
        target_idt_ptr:      u64,    // 0x10
        target_kernel_stack: u64,    // 0x18
        target_entry:        u64,    // 0x20  (AP kernel entry; set by BSP after ExRun register)
        ready:               AtomicU32, // 0x28
        go:                  AtomicU32, // 0x30
        lapic_id:            u32,    // 0x34
    }
    ```

18. `StartupThisAP(false, proc_num, ap_arg_ptr)` for each AP, in order. Wait for `ready == 1` with 100 ms timeout (busy poll or `WaitEvent`).

19. **Extend `BootInfo`** (`system/src/lib.rs`):
    ```rust
    BootInfo {
        ...existing fields...
        cpu_count:            u32,
        bsp_apic_id:          u32,
        ap_count:             u32,
        ap_apic_ids:          [u32; MAX_CPUS],
        ap_arg_phys:          [u64; MAX_CPUS], // phys addr of each ApArg
    }
    ```

20. Continue with `ExitBootServices` and jump to kernel.

### 1.2 Kernel crate — AP entry

21. **`kernel/src/main.rs`** (BSP `_start`): after existing init, read `BootInfo.cpu_count`, `BootInfo.ap_arg_phys[]`. For each AP `i`, write `go = 1` in its `ApArg` after the AP's GDT/TSS slot is built.

22. **`kernel/src/ap_start.rs`** (new, AP entry, long mode, paging on, IDT/GDT loaded):
    - `extern "C" fn ap_entry()`:
      - `lapic::read_id()` → `apic_id` (APIC register 0x20).
      - `PERCPU[apic_id].apic_id = apic_id`
      - `PERCPU[apic_id].online.store(true, Release)`
      - Init FPU/SSE (`fninit`, `stmxcsr`).
      - `ltr 0x28 + cpu_index*8` (TSS selector for this CPU).
      - Spin until BSP sets `PERCPU[bsp].kernel_ready` — then enter `task::schedule()`.

23. **BSP waits**: spin until `PERCPU[*].online` is `true` for all APs, then sets `PERCPU[bsp].kernel_ready = true`.

### 1.3 Per-CPU infrastructure (`kernel/src/percpu.rs` + `kernel/src/spinlock_irq.rs`)

24. ```rust
    pub const MAX_CPUS: usize = 4;
    pub struct PerCpu {
        pub apic_id:       u32,
        pub online:        AtomicBool,
        pub kernel_ready:  AtomicBool,
        pub current_task:  AtomicUsize,  // Task index or 0 (idle)
        pub rsp0:          u64,           // TSS.RSP0
        pub ist1:          u64,
        pub ticks:         AtomicU64,
        pub runqueue:      SpinLockIrq<Runqueue>,
        pub idle_task:     usize,
        pub tsc_offset:    i64,
        pub apic_base:     u64,
        pub ticks_per_ms:  u32,
    }
    pub static PERCPU: [PerCpu; MAX_CPUS] = ...;  // const-initialized
    ```

25. Replace existing static state:
    - `static mut LAPIC_BASE: u64` → `PERCPU[cpu].apic_base` (read from MSR on each CPU; constant across CPUs, but stored per-CPU for symmetry)
    - `static mut TICKS_PER_MS: u32` → `PERCPU[cpu].ticks_per_ms`
    - `static TICKS: AtomicU64` (in `idt.rs`) → `PERCPU[cpu].ticks`
    - `static mut TASK0_STACK_TOP: u64` → `PERCPU[0].rsp0` (BSP stack)
    - `static mut MANAGER` (in `task.rs`) → split: `PERCPU[cpu].current_task` + `static TASKS: [Task; MAX_TASKS]` shared
    - `static mut NEXT_VECTOR/VECTOR_EXHAUSTED/TABLE` (in `intr.rs`) → global (IOAPIC redirection is global, not per-CPU)
    - `static ZONE` (in `phys.rs`) → unchanged (global, but access gated by IRQ-disabling lock)
    - `static ALL_SLABS` + per-cache slabs (in `heap.rs`) → unchanged global, IRQ-disabling lock

26. **`SpinLockIrq<T>`** (`kernel/src/spinlock_irq.rs`):
    ```rust
    pub struct SpinLockIrq<T> {
        inner: UnsafeCell<T>,
        state: AtomicU8,  // 0 = unlocked, 1 = locked
    }
    impl<T> SpinLockIrq<T> {
        pub fn lock(&self) -> IrqGuard<T> {
            loop {
                if self.state.compare_exchange(0, 1, Acquire, Relaxed).is_ok() {
                    let flags = disable_irqs();
                    return IrqGuard { lock: self, flags };
                }
            }
        }
    }
    pub fn disable_irqs() -> u64 {
        let flags: u64;
        unsafe { asm!("pushfq; pop {0}; cli", out(reg) flags, options(preserves_flags)) };
        flags
    }
    pub fn restore_irqs(flags: u64) {
        unsafe { asm!("push {0}; popfq", in(reg) flags) };
    }
    ```

27. Convert all `SpinLock<...>` users to `SpinLockIrq<...>`: `phys::ZONE`, `heap::SlabList` per cache, `vma::KERNEL_VMA`, `intr::TABLE`, `task::MANAGER`'s parts.

### 1.4 Per-CPU TSS & GDT

28. `static TSS: [Tss; MAX_CPUS]`. Each TSS gets its own 16 KiB RSP0 stack and 8 KiB IST1 stack.

29. GDT layout:
    - 0x00: null
    - 0x08: kernel code (0x9A)
    - 0x10: kernel data (0x92)
    - 0x18: user code (0xFA, 64-bit)
    - 0x20: user data (0xF2)
    - 0x28 + 8*i: TSS descriptor for CPU i (i ∈ 0..MAX_CPUS)
    - Total: 5 + MAX_CPUS = 9 entries, fits in a 96-byte GDT.

30. BSP at `_start`: `gdt::init()` builds the GDT, loads GDTR, loads TR with `0x28`. APs at `ap_entry`: `gdt::init_ap(cpu_index)` updates TSS descriptor for that CPU, `ltr 0x28 + cpu_index*8`.

31. IDT is **shared** across all CPUs (one IDT, one IDTR, one `interrupt_dispatcher`).

### 1.5 LAPIC timer per CPU

32. `apic::enable_timer(period_us)` is called by each CPU. Uses `PERCPU[cpu].apic_base` + `PERCPU[cpu].ticks_per_ms`. Each CPU programs its own LAPIC timer at 1 ms periodic.

33. **TSC sync**: BSP reads TSC at `_start` and stores it; each AP reads TSC on entry and computes `tsc_offset = bsp_tsc - ap_tsc`. For now, TSC is used only for timing measurements; `ticks` remains the canonical clock.

### 1.6 IPI infrastructure

34. **`arch::apic::send_ipi(target_lapic_id, vector)`** — wraps LAPIC ICR (`0x300` low, `0x310` high). Used for:
    - **TLB shootdown** (future; not needed for SMP itself)
    - **Reschedule IPI** when a remote CPU should re-evaluate its runqueue
    - **Stop IPI** (vector 0xFD) for emergency CPU halt

35. **`arch::apic::broadcast_ipi(vector)`** — write to ICR with `destination shorthand = all-but-self`.

### 1.7 Scheduler changes

36. `static RUNQUEUES: [SpinLockIrq<Runqueue>; MAX_CPUS]` — per-CPU runqueue of CFS-style min-vruntime task.

37. Timer ISR (per-CPU): on tick, `pick_next(this_cpu)`:
    - Pop the head of `RUNQUEUES[this_cpu]`.
    - Context-switch to it.
    - On 1 ms tick, current task has its `vruntime += 20` and is re-inserted.

38. **`task::wake_remote(task, target_cpu)`** — enqueue to `RUNQUEUES[target_cpu]` and send reschedule IPI.

39. **Load balancer** (simple): every 100 ms (run from a soft timer on BSP), check `RUNQUEUES[c].len()` for all CPUs; if imbalance > 2, move the highest-vruntime task from the busiest to the least-busy.

### 1.8 BSP release loop

40. After BSP `_start` does all init, it iterates `BootInfo.ap_arg_phys[]`, writes `go = 1` to each `ApArg` (and sets the target_entry, target_pml4_phys etc. — boot was passed a pointer to the kernel's PML4 build).

41. `MAX_CPUS = 4` defined in `system/src/lib.rs` (shared with boot, which needs it for the per-AP `ApArg` array sizing).

---

## Phase 2 — Capability System (mechanism in kernel)

### 2.1 `src/cap.rs` (new; shared via `shared/src/cap.rs`)

42. ```rust
    bitflags! {
        pub struct Caps: u64 {
            const CAP_LOG            = 1 << 0;
            const CAP_TERMINAL       = 1 << 1;
            const CAP_MM_ALLOC       = 1 << 2;
            const CAP_MM_MAP         = 1 << 3;
            const CAP_MM_MAP_KERNEL  = 1 << 4;
            const CAP_TASK_CREATE    = 1 << 5;
            const CAP_TASK_DESTROY   = 1 << 6;
            const CAP_TASK_SCHED     = 1 << 7;
            const CAP_TASK_WAKE_OTHER= 1 << 8;
            const CAP_TASK_PIN       = 1 << 9;
            const CAP_INTR_INSTALL   = 1 << 10;
            const CAP_INTR_MASK      = 1 << 11;
            const CAP_INTR_EOI       = 1 << 12;
            const CAP_IPC_CREATE     = 1 << 13;
            const CAP_IPC_SEND       = 1 << 14;
            const CAP_IPC_RECV       = 1 << 15;
            const CAP_DRIVER_PCI     = 1 << 16;
            const CAP_DRIVER_BLOCK   = 1 << 17;
            const CAP_DRIVER_NET     = 1 << 18;
            const CAP_DRIVER_INPUT   = 1 << 19;
            const CAP_FS_MOUNT       = 1 << 20;
            const CAP_FS_READ        = 1 << 21;
            const CAP_FS_WRITE       = 1 << 22;
            const CAP_POLICY_READ    = 1 << 23;
            const CAP_POLICY_WRITE   = 1 << 24;
            const CAP_REBOOT         = 1 << 25;
            const CAP_HALT           = 1 << 26;
            const CAP_DEBUG          = 1 << 27;
        }
    }
    ```

43. `CapId = u8` (bit index). Helpers: `Caps::from_bit(b: u8)`, `Caps::bit_index()`.

44. **Subject identity**: `pub type SubjectId = u32;` (== TaskId == index into `TASKS[]`). The kernel doesn't have a `Process` type yet (no ring 3); subjects are kernel tasks.

45. **Default cap set** (until ExRun's policy is installed):
    - Task 0 (BSP, "main"): `Caps::all()` — kernel-internal init
    - Task 1+ (AP idle, simple_task2): `Caps::empty()` — must be granted caps by ExRun
    - Override via `pub fn set_default_caps(task: SubjectId, caps: Caps)` (kernel-internal, no check)

46. **`CapError`**:
    ```rust
    pub enum CapError {
        Denied { subject: SubjectId, required: Caps, missing: Caps },
        UnknownSubject,
        InvalidCap(u8),
        NoPolicyInstalled,
        PolicyDenied { subject: SubjectId, op: CapOp },
    }
    ```

47. **`CapOp`** (tagged enum for policy dispatch):
    ```rust
    pub enum CapOp {
        MmAlloc { frames: usize },
        MmMap   { vaddr: u64, paddr: u64, flags: u32 },
        MmUnmap { vaddr: u64 },
        TaskCreate { parent: Option<SubjectId> },
        TaskDestroy { target: SubjectId },
        IntrInstall { vector: u8 },
        IntrMask    { vector: u8, mask: bool },
        IpcSend { endpoint: u64 },
        IpcRecv { endpoint: u64 },
        Reboot,
        Halt,
        CapGrant  { target: SubjectId, cap: u8 },
        CapRevoke { target: SubjectId, cap: u8 },
    }
    ```

### 2.2 Policy hooks

48. ```rust
    pub struct PolicyHooks {
        pub on_op:     Option<fn(subject: SubjectId, op: &CapOp) -> CapDecision>,
        pub on_grant:  Option<fn(subject: SubjectId, target: SubjectId, cap: u8) -> bool>,
        pub on_revoke: Option<fn(subject: SubjectId, target: SubjectId, cap: u8) -> bool>,
        pub on_create: Option<fn(subject: SubjectId, parent: Option<SubjectId>) -> Caps>,
        pub on_destroy:Option<fn(subject: SubjectId)>,
    }
    pub enum CapDecision { Allow, Deny, Audit }
    pub static POLICY: AtomicPtr<PolicyHooks> = AtomicPtr::new(ptr::null_mut());
    pub fn install_policy(hooks: &'static PolicyHooks) {
        POLICY.store(hooks as *const _ as *mut _, Release);
    }
    ```

49. **Two-layer check at every kernel op**:
    ```rust
    pub fn check_and_authorize(
        subject: SubjectId,
        required: Caps,
        op: CapOp,
    ) -> Result<(), CapError> {
        let task = current_task();
        if !task.capabilities.contains(required) {
            return Err(CapError::Denied { subject, required, missing: !task.capabilities & required });
        }
        if let Some(p) = POLICY.load(Acquire) {
            let p = unsafe { &*p };
            if let Some(on_op) = p.on_op {
                if matches!(on_op(subject, &op), CapDecision::Deny) {
                    return Err(CapError::PolicyDenied { subject, op });
                }
            }
        }
        Ok(())
    }
    ```

50. **When `POLICY` is null** (ExRun not yet loaded): the static check still runs. The dynamic check is skipped. This is safe: the kernel only invokes cap checks once a task is doing real work, and task 0 is fully capped.

### 2.3 Wire cap checks into existing kernel APIs

51. `phys::allocate_frame` / `free_frame` — `CAP_MM_ALLOC`.
52. `vma::map_page` (public entry) — `CAP_MM_MAP` (or `CAP_MM_MAP_KERNEL` for kernel half).
53. `vma::unmap` — `CAP_MM_MAP`.
54. `task::create_task` — `CAP_TASK_CREATE`; policy `on_create` provides default caps.
55. `task::destroy_task` — `CAP_TASK_DESTROY`.
56. `task::wake` — `CAP_TASK_WAKE_OTHER` if target ≠ self, else `CAP_TASK_SCHED`.
57. `task::yield_now` — `CAP_TASK_SCHED`.
58. `intr::install_route` — `CAP_INTR_INSTALL`.
59. `intr::set_mask` — `CAP_INTR_MASK`.
60. **Future IPC calls** (placeholder): `CAP_IPC_*`.
61. `arch::apic::send_ipi` — `CAP_INTR_INSTALL`.
62. `kernel::reboot` / `kernel::halt` — `CAP_REBOOT` / `CAP_HALT`.
63. `log::log!` macros — **not** gated (kernel-internal; ring-0 threads; not a policy boundary).
64. `serial::write_byte` from a task — `CAP_LOG` (deferred; BSP serial during init is un-gated).

### 2.4 `Task` extensions

65. ```rust
    pub struct Task {
        pub id:           u32,
        pub state:        TaskState,
        pub vruntime:     u64,
        pub stack_top:    u64,
        pub caps:         AtomicU64,   // Caps bits; atomic for policy changes
        pub cpu:          u8,
        pub priority:     u8,
        pub entry:        u64,
        pub arg:          u64,
        pub parent:       Option<u32>,
        pub name:         [u8; 32],
    }
    ```

66. `Task::create(name, entry, arg, parent, initial_caps)` — applies policy `on_create` if installed, else uses `initial_caps`.

67. **Cap API**:
    ```rust
    pub fn grant_caps(target: SubjectId, caps: Caps) -> Result<(), CapError> {
        // requires CAP_POLICY_WRITE on caller
        // calls policy on_grant hook
        // OR's caps into target.caps
    }
    pub fn revoke_caps(target: SubjectId, caps: Caps) -> Result<(), CapError> {
        // requires CAP_POLICY_WRITE on caller
        // calls policy on_revoke hook
        // AND's NOT caps into target.caps
    }
    pub fn inspect_caps(target: SubjectId) -> Result<Caps, CapError> {
        // requires CAP_POLICY_READ
    }
    ```

68. **Atomic cap updates**: `target.caps.fetch_or(caps.bits(), AcqRel)` for grant; `fetch_and(!caps.bits(), AcqRel)` for revoke.

### 2.5 Tests

69. Bit set/clear round-trips.
70. `check_and_authorize` with no policy: only static bits matter.
71. `check_and_authorize` with policy: policy can deny even with the bit set.
72. `check_and_authorize` with policy: policy cannot allow when bit is missing (static check wins).
73. `grant_caps` requires `CAP_POLICY_WRITE` on caller.
74. `on_create` policy returns initial cap set for new task.
75. `PolicyHooks::default()` is all-None (no dynamic policy).

---

## Phase 3 — ExRun policy (`exrun/`)

### 3.1 Crate setup

76. `exrun/Cargo.toml`:
    ```toml
    [package]
    name = "lodaxos-exrun"
    version = "0.1.0"
    edition = "2024"

    [lib]
    crate-type = ["staticlib"]

    [dependencies]
    lodaxos-system = { path = "../system" }
    ```

77. `exrun/linker.ld`: keep current setup (base 0xFFFF_9000_0000_0000, code-model=large).

78. `exrun/target.json`: rename to `exrun/target.json`; update linker path to `-Texrun/linker.ld`.

### 3.2 ExRun exports

79. `exrun/src/lib.rs`:
    ```rust
    #![no_std]
    #![no_main]

    use lodaxos_system::cap;

    static POLICY: cap::PolicyHooks = cap::PolicyHooks {
        on_op: Some(exrun_on_op),
        on_grant: Some(exrun_on_grant),
        on_revoke: Some(exrun_on_revoke),
        on_create: Some(exrun_on_create),
        on_destroy: Some(exrun_on_destroy),
    };

    #[no_mangle]
    pub extern "C" fn exrun_register() {
        cap::install_policy(&POLICY);
        exrun_log("ExRun: policy installed");
    }

    fn exrun_on_op(subject: u32, op: &cap::CapOp) -> cap::CapDecision {
        match op {
            cap::CapOp::MmMap { .. } if subject != 0 => cap::CapDecision::Deny,
            cap::CapOp::Reboot | cap::CapOp::Halt => cap::CapDecision::Deny,
            _ => cap::CapDecision::Allow,
        }
    }
    fn exrun_on_grant(_s: u32, _t: u32, _c: u8) -> bool { true }
    fn exrun_on_revoke(_s: u32, _t: u32, _c: u8) -> bool { true }
    fn exrun_on_create(_s: u32, parent: Option<u32>) -> cap::Caps {
        let mut caps = match parent {
            Some(p) => lookup_caps(p),
            None => cap::Caps::empty(),
        };
        caps.remove(cap::Caps::CAP_REBOOT | cap::Caps::CAP_HALT);
        caps
    }
    fn exrun_on_destroy(_s: u32) {}

    fn exrun_log(_s: &str) {}  // silent for v1
    ```

80. **Symbol resolution**: the kernel's `exrun_loader` looks up `exrun_register` in the loaded ELF and stores its address in a `static EXRUN_REGISTER: AtomicPtr<()>`. After ExRun's segments are loaded, the kernel does:
    ```rust
    let f: extern "C" fn() = core::mem::transmute(EXRUN_REGISTER.load(Acquire));
    f();  // installs policy
    ```

### 3.3 ExRun data

81. ExRun keeps an internal `static mut CAPS_DB: [Caps; MAX_TASKS]` mirroring task cap sets, populated by `on_grant`/`on_revoke`/`on_create`. For v1, this is a write-only audit log; queries not yet supported.

82. ExRun does **not** spawn tasks, hold references to kernel objects, or do any kernel work. It is a pure policy module.

### 3.4 Future ExRun extensions (deferred)

- Self-revocation of policy (install a no-op policy).
- Per-namespace policy tables.
- Cap derivation / attenuation.
- Time-bounded capabilities.
- Audit log persistence.

---

## Phase 4 — Documentation

83. **`docs/architecture/00..10.md`**:
    - 00 (overview): add "Mechanism / Policy split" section: "The kernel provides **mechanism** (page tables, scheduling, IPC primitives, capability enforcement). The Executive Runtime provides **policy** (who gets which capabilities, default behavior, audit). Policy is installed via a function pointer table at boot."
    - 00: rename all `SR` / `Secure Runtime` → `ExRun` / `Executive Runtime`.
    - 06 (process model): **deferred** — leave the existing "no process model yet" wording; add note "ring 3 / Process type / syscalls deferred to a later phase".
    - 07 (build): rename `sr/` → `exrun/`, `lodaxos-sr` → `lodaxos-exrun`, `sr.elf` → `exrun.elf`.
    - 08 (interfaces): add "Capability system" subsection describing `Caps`, `PolicyHooks`, and the static + dynamic check sequence.
    - 09 (subsystem interfaces): add `cap::check_and_authorize` to the list of subsystem entry points.
    - 10 (future): rewrite to reflect the new direction (Mechanism/Policy, MAX_CPUS=4, no init process, no ring 3 yet).

84. **`docs/ARCHITECTURE.md`**: regenerate via `build-architecture.ps1`.

85. **`bootupdetails.md`**, **`memory.md`**, **`memsave.md`**, **`idea.md`**: rename pass, plus a short note on the new capability system and the mechanism/policy split.

86. **New `docs/architecture/11-capabilities.md`** (or extend 08): detailed spec of `Caps`, the cap set, the policy hook semantics, and the default policy. Becomes the canonical reference.

---

## Open Decisions (resolved)

1. **HealthFlag names** — rename to `HF_EXRUN_*` for consistency.
2. **Default cap set for task 0** — `Caps::all()` for BSP init; ExRun can revoke post-`exrun_register`.
3. **Default cap set for AP idle / simple_task2** — `Caps::empty()`; ExRun grants as needed.
4. **Logging from ExRun** — silent for v1 (option A).
5. **Capability format on disk** — strictly in-memory for v1.
6. **`MAX_CPUS = 4`** — defined in `system/src/lib.rs`.
7. **PIT** — keep masked, LAPIC is canonical.
8. **False-positive `SR` strings** — handled manually during rename.
9. **Driver cap bits** — include now (`CAP_DRIVER_PCI/_BLOCK/_NET/_INPUT`) to fix the cap set's identity.
10. **Pinning tasks** — `task::pin(task, cpu)` gated by `CAP_TASK_PIN`; `create` accepts initial `Option<u8>` CPU.

---

# FUTURE — Ring-3 + Syscalls (deferred, not part of this work)

This is preserved for future memory. **Not implemented now** per user instruction.

## SMP scope (deferred ring-3 work that touches SMP)

1. **Trampoline** — already in `boot/src/mp_trampoline.rs` from Phase 1; reuse for ring-3 bring-up.
2. **Per-CPU state** — already in `PERCPU[]` from Phase 1.
3. **Per-CPU TSS** — already done in Phase 1.
4. **Per-CPU kernel stack** — already done in Phase 1.
5. **Scheduler rewrite** — already done in Phase 1 (per-CPU runqueues + IPI wake).
6. **Spinlocks** — already done in Phase 1 (IRQ-disabling).
7. **TLB shootdown** — wraps `apic::send_ipi` + per-CPU handler that does `invlpg`.
8. **Timer** — already done in Phase 1.
9. **AP bring-up sequence** — already done in Phase 1.
10. **Pin/migrate** — added in Phase 2.4.

## Ring-3 syscalls (full design, deferred)

### Process model

1. Add `Process` alongside `Task` (`kernel/src/process.rs`):
   ```rust
   struct Process {
       pid:            u32,
       parent:         Option<u32>,
       pml4_phys:      u64,
       kernel_stack:   u64,
       usermode_rip:   u64,
       usermode_rsp:   u64,
       usermode_rflags:u64,
       ring3_cs:       u16,         // 0x23 (RPL=3, base 0x18)
       ring3_ss:       u16,         // 0x2B (RPL=3, base 0x20)
       capabilities:   u64,
       vma:            VmaTree,     // user half
       exit_status:    AtomicI32,
       state:          AtomicU8,
   }
   ```

2. Per-process PML4 — kernel higher half shared, user half per-process. TLB shootdown on PML4 change.

3. TSS.RSP0 written on context switch to a process.

### syscall / sysret (recommended over int 0x80)

4. **MSR setup** (`kernel/src/arch/syscall.rs`):
   - `IA32_EFER.SCE` set
   - `IA32_STAR = 0x0010_0000_0000_0000` → SYSCALL CS=0x08, SS=0x10; SYSRET CS=0x1B, SS=0x23
   - `IA32_LSTAR` → `syscall_entry` ASM stub
   - `IA32_FMASK` → mask IF, AC, etc.

5. **ASM `syscall_entry`** — saves user GPRs to a fresh TrapFrame on the per-CPU kernel stack, calls `syscall_dispatch`.

6. **Rust `syscall_dispatch`** — reads `rax` (number), reads `rdi, rsi, rdx, r10, r8, r9` (args), gates on `process.capabilities`, dispatches:
   - `exit(status)`
   - `write(fd, buf, len)`
   - `read(fd, buf, len)`
   - `brk(addr)`
   - `mmap(addr, len, prot, flags)`
   - `munmap(addr, len)`
   - `spawn_elf(path)` *(deferred — see below)*
   - `wait(pid)`
   - `ipc_send(endpoint, msg)`
   - `ipc_recv(endpoint)`
   - `getpid`
   - `get_ticks`
   - `yield`

7. **SMAP/SMEP** — set `CR4.SMAP`, `CR4.SMEP`. `copy_from_user`/`copy_to_user` use `stac`/`clac`.

8. **Capability check** at every syscall, using `check_and_authorize` from Phase 2.

### User ELF loader (deferred)

9. **`kernel/src/user_loader.rs`** — mirror of `exrun_loader` but for user ELFs. Allocates user pages, sets up user stack, pushes `argc, argv, envp, auxv` per System V ABI.

10. **Boot chain update** — add `/System/init.elf` to disk image. **DEFERRED** — kernel stays one piece, no init process per user decision.

11. **Init process** — DEFERRED.

### Deferred indefinitely

- `spawn_elf`, fork, COW — deferred per user.
- `wait`, `getpid`, `get_ticks`, `yield`, `exit` — implementable without ring 3 (operate on Tasks); gate via caps.

---

## Build commands (for verification)

```powershell
cargo +nightly build -p lodaxos-system
cargo +nightly build -p lodaxos-core
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"
cargo +nightly build -p lodaxos-exrun --target exrun/target.json -Zjson-target-spec "-Zbuild-std=core" "-Zbuild-std-features=compiler-builtins-mem"
```

QEMU launch: `run.bat` (uses `-smp 2 -m 512M -accel whpx`).
