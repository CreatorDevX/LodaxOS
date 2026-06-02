# 10 — Future Architecture: Secure Runtime and Beyond

## Overview

The current LodaxOS kernel is phase 1 of a larger architecture. This document describes the planned Secure Runtime, PyI runtime, Agent model, and migration path from the current monolithic kernel to a capability-based microkernel system.

## Architecture Evolution

### Phase 1 (Current): Monolithic Boot Kernel

- Single address space (ring 0 only)
- All subsystems compiled into kernel ELF
- No process isolation
- No userspace
- Boot chain proven, hardware init complete

### Phase 2: Secure Runtime + Process Model

- Introduce kernel-managed processes with isolation
- Secure Runtime runs as the first userspace process (PID 1)
- Services (filesystem, drivers) run as separate processes managed by SR
- Capability-based IPC for inter-process communication
- Kernel retains scheduling, memory management, IPC primitives

### Phase 3: PyI Runtime

- PyI runs as a process under SR (PID 2+)
- Provides JIT-compiled Python/WASM execution environment
- All user-facing abstractions (files, windows, apps) are PyI objects
- REPL access to system state
- Application sandboxing within PyI's process

### Phase 4: Agent Model

- Multiple independent Agent domains
- Each Agent owns its own userspace environment
- Agent isolation via separate page table roots
- Secure Runtime manages Agent lifecycle and policies
- InstallerAuthority (IA) for system setup (destroyed after installation)
- InteUser (IU) for administrative operations

## Secure Runtime Architecture

### Position in the System

```
┌─────────────────────────────────────────────┐
│ Kernel (Ring 0)                              │
│  Scheduler, Memory, IPC, HAL, Capabilities   │
├─────────────────────────────────────────────┤
│ Secure Runtime (Ring 3, PID 1)               │
│  Service Manager, Policy Engine, Cap Broker  │
├─────────────────────────────────────────────┤
│ Services (Ring 3, various PIDs)              │
│  Filesystem, Network, Audio, Display, etc.   │
├─────────────────────────────────────────────┤
│ PyI Runtime (Ring 3, PID 2)                  │
│  App sandbox, UI, REPL, WASM JIT             │
├─────────────────────────────────────────────┤
│ Agent 0 (Default User)                       │
│  Applications, desktop environment           │
└─────────────────────────────────────────────┘
```

### Responsibilities

| Area | Responsibility |
|---|---|
| Service Management | Spawn, monitor, restart services per policy |
| Capability Issuance | Grant/revoke capabilities to processes |
| Policy Evaluation | Check every privileged operation against policy |
| Process Lifecycle | Define restart policies, handle failures |
| Permission Mutation | Modify process permissions at runtime |
| System Orchestration | Boot order, dependency resolution, health checks |

### Capability Model

Capabilities are the only way to access kernel resources. A process cannot do anything the kernel doesn't explicitly allow.

```
struct Capability {
    id: u64,              // unique capability identifier
    resource: Resource,   // what resource this grants access to
    rights: Rights,       // read, write, execute, manage
    expires: u64,         // optional expiry in ticks
    issuer: u64,          // SR process ID (only SR can issue caps)
}
```

### IPC Mechanism

IPC is built on capability-passing message channels:

```
Process A                          Process B
   │                                  │
   │── send(channel, message, caps)──→│
   │                                  │─ receive(...) → handle
   │←── reply(channel, response)──────│
```

Channels are kernel objects created by SR. Each channel is identified by a capability. Message passing includes:
- Up to 64 bytes of inline data
- Up to 4 capability transfers
- Optional reply channel for request-response patterns

### Service Definition

Services are defined in metadata on Partition Zero (`/SecureRuntime/services/`):

```json
{
    "name": "filesystem",
    "binary": "/SecureRuntime/bin/fsd.elf",
    "type": "service",
    "restart": "on-failure",
    "memory": "64mb",
    "capabilities": ["block_io", "storage:read", "storage:write"],
    "depends_on": ["block"],
    "permissions": {
        "devices": ["ata", "nvme"],
        "paths": ["/system/*"]
    }
}
```

## PyI Runtime

### Architecture

PyI (Python Integral) is a self-contained process that provides:
1. A JIT-compiled Python runtime (WASM-backed for sandboxing)
2. System API library (`import system`) for application development
3. REPL access to all system functionality
4. Application isolation (each app is a sub-process or coroutine)

### Application Model

```python
import system

app = system.define(
    name="editor",
    permissions=["display", "storage"],
    restart="on-failure",
    memory="128mb"
)

@app.main
async def main():
    window = system.display.create_window(800, 600, "Editor")
    while True:
        event = await window.events.next()
        if event.type == "close":
            break
```

### REPL Accessibility

Every system function is accessible via the PyI REPL:

```
> system.processes.list()
[PID 1: Secure Runtime, PID 2: PyI, PID 3: fsd, PID 4: editor]

> system.display.list_modes()
[1920x1080@60, 1280x720@60, 1024x768@60]

> system.storage.mount("/dev/ata0", "/mnt/data")
```

### JIT Compilation

PyI compiles Python bytecode to WASM, which is then JIT-compiled to native code:
```
Python source → bytecode → WASM → native code (via WASM runtime)
```

This provides:
- Sandboxed execution (WASM memory isolation)
- Near-native performance (JIT-compiled)
- Language-agnostic runtime (any WASM-compiling language can run)

## Agent Model

### Agent Definition

An Agent is a first-class system domain:

```rust
struct Agent {
    id: AgentId,
    name: String,
    state: AgentState,           // Active, SafeMode, Corrupted, Restoring
    processes: Vec<ProcessId>,
    runtime: AgentRuntime,       // PyI or other
    capabilities: CapabilitySet,
    storage: AgentStorage,       // agent-local persistent storage
}
```

### Agent Lifecycle

1. **Creation**: SR creates the agent, assigns an ID, allocates storage
2. **Boot**: SR starts PyI within the agent's domain
3. **Operation**: Agent runs normally, SR monitors heartbeat
4. **Safe Mode**: On failure, SR boots agent into Safe Mode (minimal REPL)
5. **Restoration**: SR restores agent from last known good state
6. **Deletion**: SR tears down agent, reclaims resources

### Agent Safe Mode

Safe Mode provides exactly 7 commands:

| Command | Purpose |
|---|---|
| `ls` | List files in agent storage |
| `cd` | Navigate agent storage |
| `read` | Display file contents |
| `write` | Write to a file |
| `start-userspace` | Exit Safe Mode, start normal runtime |
| `restart` | Warm restart the agent |
| `shutdown` | Halt the agent |

Safe Mode depends only on the kernel (serial, framebuffer, storage). It does NOT depend on PyI, the filesystem service, or any other service.

### Principal Invariant

LodaxOS requires at least one valid Agent definition at all times. If all agents are deleted or corrupted, the system enters an unrecoverable hard fault. This ensures there is always a principal capable of operating the system.

## Driver Architecture

### Philosophy

Drivers are services, not kernel modules. The kernel provides a hardware access layer (HAL), and driver services implement device-specific logic on top of it.

### Kernel HAL

The kernel provides:

| Interface | Purpose |
|---|---|
| PCI Enumeration | Discover devices, read config space |
| Interrupt Management | Allocate vectors, register handlers |
| DMA Management | Allocate DMA buffers, manage IOMMU |
| MMIO Mapping | Map device BARs into process address space |
| Device Ownership | Track which agent owns which device |

### Driver Service

A driver service is a regular process with additional capabilities:

```rust
struct DriverService {
    pci_device: PciAddress,      // which PCI device this driver manages
    interrupts: Vec<u8>,         // allocated interrupt vectors
    mmio_regions: Vec<MmioRegion>,  // mapped MMIO ranges
    dma_buffers: Vec<DmaBuffer>,    // allocated DMA memory
    ops: DriverOps,              // read, write, ioctl, etc.
}
```

### Device Sharing Models

**True Multiplex**: Hardware naturally supports multiple consumers (CPUs, network queues, audio mixing). The kernel allows direct access.

**Virtual Multiplex**: A service owns the physical device and virtualizes it (display server shares framebuffer, filesystem server shares storage). The service handles arbitration.

## System Boot Order (Future)

```
1. Firmware → Bootloader → Kernel
2. Kernel initializes (current Phase 1–4)
3. Kernel spawns Secure Runtime (load from Partition Zero)
4. SR loads service definitions from Partition Zero
5. SR spawns core services (block, filesystem, PCI)
6. SR spawns PyI
7. PyI initializes Agent 0's userspace
8. System ready — user login/REPL
9. SR monitors all services, handles failures
```

## State Storage

### Partition Zero Layout

```
Partition Zero (ext4, 512 MB):
  /kernel.elf                    — current kernel binary
  /Bootloader.efi                — current bootloader
  /sr.elf                        — current Secure Runtime stub
  /SecureRuntime/
    ├── bin/
    │   ├── sr.elf               — Secure Runtime binary (future)
    │   ├── fsd.elf              — filesystem daemon
    │   ├── pci.elf              — PCI manager
    │   └── ...
    ├── config/
    │   ├── order                — boot order definition
    │   ├── policies/            — security policies
    │   └── services/            — service definitions
    ├── state/
    │   ├── sr_state.bin         — serialized SR state
    │   └── recovery/            — recovery snapshots
    └── backup/
        ├── kernel.elf           — backup kernel
        └── sr.elf               — backup SR
  /Agents/
    ├── 0/
    │   ├── config               — agent definition
    │   ├── state/               — serialized agent state
    │   └── storage/             — agent-local files
    └── ... (per agent)
  /System/
    └── recovery/                — system-wide recovery metadata
```

## Migration Path

### Step 1: Process Abstraction (Current Kernel)

The kernel needs these additions before userspace can run:
- [ ] Ring 3 execution support (update GDT, IDT, page tables for user pages)
- [ ] Syscall dispatch via `syscall`/`sysret` instructions
- [ ] Process creation (allocate user page tables, map ELF segments)
- [ ] Basic IPC: kernel-level message channels

### Step 2: Secure Runtime (First Userspace)

- [ ] Write SR as a standalone ELF binary
- [ ] Implement capability system in kernel
- [ ] Kernel boots SR as PID 1
- [ ] SR implements service manager
- [ ] SR defines and enforces security policies

### Step 3: Filesystem Service

- [ ] Port ext4 parser from bootloader to a service
- [ ] Implement block device abstraction
- [ ] Filesystem service provides open/read/write/close via IPC
- [ ] Path resolution and permission checking in filesystem service

### Step 4: PyI Runtime

- [ ] Port or implement a WASM runtime
- [ ] Implement Python subset compiler → WASM
- [ ] PyI runs as a process under SR
- [ ] System API library exposes system functions to Python

### Step 5: Agent Framework

- [ ] Implement agent creation/deletion in SR
- [ ] Per-agent page table management in kernel
- [ ] Agent state serialization/restoration
- [ ] Safe Mode implementation

## Capability-Based Security Model

### Principle

A process holds capabilities for exactly those resources it is allowed to access. There is no "root" or "superuser" — all privileges are explicit and granular.

### Capability Types

| Type | Resource | Rights |
|---|---|---|
| Memory | Physical pages, virtual ranges | Read, Write, Execute |
| IPC | Channel endpoints | Send, Receive, Reply |
| Device | PCI devices, MMIO regions | Read, Write, Interrupt |
| Storage | Partitions, filesystem paths | Read, Write, Create, Delete |
| Scheduling | CPU time, priorities | Set quantum, Set priority |
| Management | Process lifecycle | Create, Kill, Set policy |

### Policy Evaluation Flow

```
Process requests operation
  → request routed through SR
    → SR checks capability set
    → SR evaluates policy rules
      → if allowed: grant and cache capability
      → if denied: return error (SIGCAPDENY)
    → kernel enforces the capability
```

### Signal Injection

When SR needs to revoke a capability mid-execution:
1. SR asks the kernel to deliver `SIGCAPREVOKE` to the process
2. Process's signal handler can clean up gracefully
3. If no handler: default action (terminate)
4. Next syscall from the process fails with `CAPREVOKED`

This is softer than hard revocation (which would immediately fail the next memory access or syscall) but requires the process to opt into the signal protocol.
