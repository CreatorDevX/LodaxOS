# 08 — Fault Model and Recovery

## Philosophy

LodaxOS treats failures as normal operational events, not exceptional catastrophes. The system is designed so that every component except the kernel can fail without bringing down the entire system. Recovery is layered, with each layer responsible for handling failures at that level.

## Fault Classification

### Hard Faults (Kernel-Level)

A hard fault requires kernel intervention. The kernel halts or resets the CPU. These are always logged to serial before halting.

| Code | Name | Cause |
|---|---|---|
| `HF_KERNEL_PANIC` | 0x01 | Generic kernel panic |
| `HF_DOUBLE_FAULT` | 0x02 | CPU double fault (#DF) |
| `HF_TRIPLE_FAULT` | 0x03 | CPU reset (QEMU `-d cpu_reset`) |
| `HF_SR_UNRECOVERABLE` | 0x04 | SR failed N respawn attempts |
| `HF_SR_SPAWN_FAIL` | 0x05 | Kernel couldn't spawn SR at boot |
| `HF_MEMORY_EXHAUSTED` | 0x06 | PMM completely empty |

### Soft Faults (Service-Level)

A soft fault is handled by Secure Runtime (SR). The kernel is not involved.

| Code | Name | Cause |
|---|---|---|
| `SF_PYI_CRASH` | 0x10 | PyI process death |
| `SF_PYI_TIMEOUT` | 0x11 | PyI heartbeat missed |
| `SF_PYI_OOM` | 0x12 | PyI's memory cap exhausted |

## Current Fault Handling

### Panic Handler

Each boot stage has its own panic handler:

**Chainloader** (`chain/src/main.rs`):
- Writes `"PANIC"` to serial with polling timeout (100K retries per byte)
- Formats and writes the location (file name + line number) when `info.location()` is available
- Writes the panic message body
- Halts: `cli; hlt` loop

**Bootloader** (`boot/src/main.rs`):
- Writes `"PANIC"` to serial
- Formats and writes location (file name, line number) via manual decimal conversion
- Writes panic message via `core::fmt::Write`
- Halts: `cli; hlt` loop

**Kernel** (`kernel/src/main.rs`):
- Writes `"PANIC at "` + file name + line number to serial
- Writes panic message via `core::fmt::Write`
- Halts: `cli; hlt` loop

### Exception Handling

The kernel's exception handler (vector 0–31) logs detailed register state and halts for all exceptions except breakpoints (#BP, vector 3) and page faults (#PF, vector 14 — which logs but the kernel currently cannot resolve them).

Double Faults (#DF, vector 8) use IST1 (Interrupt Stack Table 1) — a dedicated 16 KB stack. This ensures that if the kernel's stack is corrupted, the double fault handler still has a valid stack. The handler logs and halts.

The spurious interrupt vector (0xFF) is a bare `iretq` with no EOI and no logging. The LAPIC may generate spurious interrupts due to bus noise or race conditions; the simplest correct response is to ignore them.

## Planned Recovery Architecture

### Recovery Layers

```
Application Failure
  ↓ (restart application)
PyI Failure
  ↓ (restart PyI runtime)
Agent Safe Mode
  ↓ (recover agent state)
Agent State Restoration
  ↓ (restore from snapshot)
Secure Runtime Recovery
  ↓ (re-spawn SR)
Kernel Recovery (future)
  ↓ (boot backup kernel)
```

### Application-Level Recovery

When an application crashes within PyI:
1. PyI detects the crash (signal handler, exception boundary)
2. PyI logs the failure
3. PyI restarts the application with its defined restart policy:
   - `"on-failure"`: restart automatically
   - `"always"`: restart regardless of exit code
   - `"never"`: don't restart
   - `"backoff"`: restart with exponential backoff
4. If restart fails N times, PyI reports to SR

### PyI-Level Recovery

When PyI itself crashes:
1. SR detects PyI crash via heartbeat miss or signal notification
2. SR checks PyI's defined memory cap (`memory="128mb"`)
3. If PyI exceeded memory: `SF_PYI_OOM` — re-spawn with larger cap or kill processes
4. If PyI crashed: `SF_PYI_CRASH` — re-spawn from known-good binary image
5. If PyI heartbeat is missing: `SF_PYI_TIMEOUT` — wait one interval, then re-spawn

### Agent Safe Mode

Safe Mode is a minimal runtime state that provides just enough functionality to debug and repair a corrupted agent:
- Minimal process management (ls, cd, read, write)
- No PyI, no UI, no device access
- Access to agent-local storage for diagnostics
- REPL access to the agent's state

Agent Safe Mode is entered when:
1. PyI crashes during recovery boot
2. Agent configuration is corrupted
3. User explicitly triggers it

### Secure Runtime Recovery

If SR itself fails:
1. The kernel detects SR failure (signal or heartbeat)
2. Kernel logs `HF_SR_UNRECOVERABLE`
3. Kernel attempts to re-spawn SR from the backup binary on Partition Zero
4. If re-spawn succeeds, SR enters Emergency Mode
5. If re-spawn fails N times, kernel halts with `HF_SR_UNRECOVERABLE`

### Emergency Mode

Emergency Mode is a minimal system state that runs directly on the kernel, bypassing SR and PyI entirely:

| Command | Description |
|---|---|
| `ls [path]` | List directory contents |
| `cd [path]` | Change directory |
| `read [file]` | Display file contents |
| `write [file] [content]` | Write content to file |
| `start-userspace` | Attempt to start SR → PyI → normal mode |
| `restart` | Warm reboot |
| `shutdown` | Power off |

Emergency Mode does not depend on SR, PyI, or any service. It is compiled into the kernel or loaded as a minimal initramfs-style binary.

### Kernel Recovery (Future)

A future kernel recovery mode may:
1. Validate the current kernel's integrity
2. Load a backup kernel from Partition Zero
3. Validate and restore Secure Runtime state
4. Reboot into the backup kernel

This is the deepest recovery layer and requires:
- A reserved Partition Zero region for backup kernel images
- Integrity checking (hash verification) of kernel and SR binaries
- State serialization and restoration protocol

## Fault Propagation Rules

1. **A failure in a lower layer always causes the upper layers to fail.** If the kernel panics, everything stops. If SR crashes, PyI and all agents lose access to services.

2. **Recovery starts at the layer of failure and rebuilds upward.** An SR crash triggers SR recovery, which then restarts PyI, which then restarts agents and applications. Layers below the failure point are unaffected.

3. **Hard faults are fatal.** A kernel panic, double fault, or triple fault is not recoverable below the kernel layer. The system must be rebooted (or, in the future, kernel recovery mode must be triggered).

4. **Soft faults are recoverable.** All soft faults (SF codes) are handled by SR. The kernel is never involved in soft fault recovery.

## Future Data Structures

### Fault Log

```
struct FaultRecord {
    timestamp: u64,          // ticks since boot
    code: u32,               // HF_ or SF_ code
    source_id: u16,          // task/process/agent ID
    details: [u8; 64],       // fault-specific data
    crc: u32,                // integrity check
}
```

Fault records would be stored in a ring buffer accessible from Emergency Mode for post-mortem analysis.

### Service Restart Policy

```
struct RestartPolicy {
    max_retries: u32,        // max consecutive restart attempts
    backoff_ms: u32,         // initial backoff in ms
    backoff_multiplier: f32, // exponential backoff multiplier
    action: enum {
        Restart,
        SafeMode,
        Emergency,
        Halt,
    },
}
```

This would be embedded in the SR's service definition metadata stored on Partition Zero.
