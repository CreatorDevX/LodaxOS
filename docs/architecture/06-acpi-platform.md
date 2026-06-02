# 06 — ACPI and Platform Discovery

## Overview

LodaxOS discovers hardware topology through ACPI (Advanced Configuration and Power Interface) tables. The kernel parses the RSDP, XSDT, and MADT to find CPUs, I/O APICs, and interrupt routing information.

## Discovery Order

```
Bootloader captures RSDP from UEFI config table
  → stores physical address in BootInfo.rsdp_addr
Kernel reads RSDP address from BootInfo
  → parses RSDP to find XSDT
    → parses XSDT to find MADT ("APIC" signature)
      → parses MADT to enumerate CPUs, IOAPICs, ISOs
```

## RSDP (Root System Description Pointer)

The RSDP is a 36-byte (v2.0+) or 20-byte (v1.0) structure. The chainloader captures the UEFI configuration table pointer into `BootInfo.rsdp_addr` before `ExitBootServices`; the kernel prefers that hint and only falls back to scanning firmware regions if the hint is missing or invalid.

Scanned regions, in order:
1. The hint from `BootInfo.rsdp_addr` (validated by signature and checksum)
2. EBDA (Extended BIOS Data Area) — word at `0x40E` points to segment
3. Standard BIOS ROM area (`0xE0000–0xFFFFF`)
4. OVMF/UEFI firmware area (`0xFEFF_0000–0xFF00_0000`)

The RSDP signature is `"RSD PTR "` (8 bytes with trailing space). Validation:
- Checksum over all bytes of the RSDP must sum to 0 (mod 256)

### RSDP Fields

| Offset | Size | Field | Description |
|---|---|---|---|
| 0 | 8 | signature | "RSD PTR " |
| 8 | 1 | checksum | Sum of bytes 0–19 = 0 |
| 9 | 6 | oem_id | OEM identifier |
| 15 | 1 | revision | 0 = v1.0, 2 = v2.0+ |
| 16 | 4 | rsdt_addr | RSDT physical address (v1.0) |
| 20 | 4 | length | Total RSDP length (v2.0+) |
| 24 | 8 | xsdt_addr | XSDT physical address (v2.0+) |
| 32 | 1 | ext_checksum | Sum of all bytes = 0 (v2.0+) |

## XSDT (Extended System Description Table)

The XSDT is an array of 64-bit physical addresses pointing to other ACPI tables. It is preceded by a standard SDT (System Description Table) header.

```
XSDT Header (36 bytes):
  signature[4] = "XSDT"
  length: u32
  revision: u8
  checksum: u8
  oem_id[6]
  oem_table_id[8]
  oem_revision: u32
  creator_id: u32
  creator_revision: u32

Entry array (8 bytes each):
  entry[0]: u64 (physical address of first table)
  entry[1]: u64 (physical address of second table)
  ...
```

The kernel scans XSDT entries looking for the `"APIC"` signature (MADT). Each table is validated by checksum (sum of all bytes in the table must equal 0 mod 256).

### RSDT Fallback

If the RSDP revision is 0 (v1.0), the RSDT is used instead. The RSDT entry array has 4-byte entries (32-bit physical addresses) instead of the XSDT's 8-byte entries.

## MADT (Multiple APIC Description Table)

The MADT describes the APIC topology of the system.

### Fixed Header

```
MADT:
  SDT Header (36 bytes, signature = "APIC")
  local_apic_addr: u32    — physical address of LAPIC (typically 0xFEE00000)
  flags: u32              — bit 0 = PC-AT compatibility (dual 8259s)
  entries...              — variable-length entry list
```

### Entry Types

| Type | Name | Length | Description |
|---|---|---|---|
| 0 | Local APIC | 8 | CPU core with LAPIC |
| 1 | I/O APIC | 12 | I/O APIC controller |
| 2 | ISO | 10 | Interrupt Source Override |
| 4 | NMI | 6 | NMI source |
| 5 | Local APIC Override | 12 | 64-bit LAPIC address |
| 6 | I/O APIC NMI | 10 | I/O APIC NMI routing |

### Entry 0: Local APIC

```
type: u8 = 0
length: u8 = 8
acpi_processor_id: u8
apic_id: u8
flags: u32 (bit 0 = enabled)
```

The kernel counts enabled CPUs (those with `flags & 1 != 0`) for future SMP support. Currently, CPUs beyond the BSP are noted but not started.

### Entry 1: I/O APIC

```
type: u8 = 1
length: u8 = 12
ioapic_id: u8
reserved: u8
ioapic_addr: u32   — MMIO base (typically 0xFEC00000)
gsi_base: u32      — starting GSI number
```

The IOAPIC driver maps each discovered IOAPIC's MMIO region, reads its version and max redirection entry count, then initializes all redirection entries to a masked state.

### Entry 2: Interrupt Source Override (ISO)

```
type: u8 = 2
length: u8 = 10
bus: u8              — bus source (0 = ISA)
source: u8           — ISA IRQ number
gsi: u32             — Global System Interrupt
flags: u16           — bit 1 = polarity, bit 3 = trigger mode
```

ISOs are how ACPI tells the OS about deviations from the standard ISA IRQ→GSI mapping. On modern hardware:
- ISA IRQ 0 usually overrides to GSI 2 (PIT)
- ISA IRQ 2 often cascades differently

The interrupt routing table (`src/intr/mod.rs`) is built from ISOs plus identity mappings for any ISA IRQ without an ISO.

## GSI (Global System Interrupt) Routing

```
ISA IRQ → ISO lookup → GSI → IOAPIC lookup → IOAPIC[index] + pin → vector
```

Each step maps through a table:

1. **ISA → GSI**: Look up ISO entries by `bus==0` and `source==IRQ`. If found, use the ISO's GSI. Otherwise, identity-map (GSI = IRQ).
2. **GSI → IOAPIC**: Scan IOAPIC entries for `gsi_base ≤ GSI < gsi_base + max_redir`. The pin is `GSI - gsi_base`.
3. **GSI → Vector**: Allocate a unique vector from the device range (33–63).

## Non-MADT Tables (Future)

The ACPI subsystem can be extended to parse additional tables. The codebase currently keeps the `XSDT_SIG` and `MADT_SIG` signature constants; other signatures (`"FACP"`, `"MCFG"`, `"HPET"`, `"DSDT"`, `"SSDT"`) are not declared and must be added when the corresponding parsers are written.

### FADT (Fixed ACPI Description Table)

| Signature | "FACP" |
|---|---|
| Purpose | Power management, reset register, sleep states |
| Use | System shutdown, reboot, S3/S4 sleep |

### DSDT/SSDT (Differentiated System Description Table)

| Signature | "DSDT", "SSDT" |
|---|---|
| Purpose | AML bytecode for device enumeration |
| Use | Device discovery, power management, battery/ACPI EC |

### MCFG (PCI Express Memory-Mapped Configuration)

| Signature | "MCFG" |
|---|---|
| Purpose | PCIe ECAM base address |
| Use | PCI enumeration via memory-mapped config space |

### HPET (High Precision Event Timer)

| Signature | "HPET" |
|---|---|
| Purpose | HPET base address and capabilities |
| Use | Alternative to PIT for timekeeping and event scheduling |

## Future Platform Support

### PCI Enumeration

PCI bus enumeration will use:
1. MCFG table for memory-mapped config access (ECAM)
2. For legacy PCI, I/O port config mechanism at `0xCF8/0xCFC`
3. Each discovered device gets a bus:device:function identifier
4. Device BARs (Base Address Registers) determine MMIO/I/O ranges
5. MSI/MSI-X capabilities are detected and configured

### Multi-Processor Startup

Per the Intel Multiprocessor Specification, APs (Application Processors) are started via:
1. Send INIT IPI to the AP
2. Wait 10 ms
3. Send STARTUP IPI with SIPI vector (0x467 = startup code at `0x8000`)
4. Wait for AP to acknowledge
5. AP executes trampoline code that:
   a. Sets up page tables (BSP's PML4 is shared)
   b. Loads GDT (BSP's GDT is shared)
   c. Sets up per-CPU stack
   d. Calls per-CPU initialization routine
   e. Enters idle loop

### ACPI Namespace (AML)

The ACPI namespace and AML interpreter are a significant addition. AML is executed to:
- Discover devices not in the MADT/XSDT
- Evaluate _PRS (possible resources), _CRS (current resources), _SRS (set resources)
- Handle power management events
- Evaluate _OSC (OS capabilities)

AML requires a bytecode interpreter with memory management — a significant undertaking in `no_std`.
