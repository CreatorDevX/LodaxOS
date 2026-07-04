#![no_main]
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { x86_64::instructions::interrupts::disable(); x86_64::instructions::hlt(); }
}

fn syscall(nr: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

fn sys_pci_read(bdf: u64, offset: u64, width: u64) -> u64 {
    syscall(15, bdf, offset, width, 0, 0)
}

fn sys_pci_write(bdf: u64, offset: u64, width: u64, value: u64) -> u64 {
    syscall(15, bdf, offset, width, value, 1)
}

fn sys_mmap_phys(phys: u64, size: u64) -> u64 {
    syscall(10, phys, size, 0, 0, 0)
}

fn sys_dma_alloc(size: u64) -> u64 {
    syscall(13, size, 0, 0, 0, 0)
}

fn sys_dma_free(phys: u64, size: u64) {
    syscall(14, phys, size, 0, 0, 0);
}

fn sys_gdf_register(name: &[u8]) -> u64 {
    syscall(30, name.as_ptr() as u64, name.len() as u64, 0, 0, 0)
}

fn sys_driver_recv(out: &mut [u64; 4]) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 20u64 => result,
            in("rdi") out.as_mut_ptr() as u64,
            in("rsi") 0u64,
            in("rdx") 0u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

fn sys_driver_send(result: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") 21u64 => ret,
            in("rdi") result,
            in("rsi") 0u64,
            in("rdx") 0u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

// ── AHCI register layout (ABAR) ────────────────────────────────────

const HBA_GHC: u64 = 0x04;      // Global HBA Control
const HBA_CAP: u64 = 0x00;      // Capabilities
const HBA_PI: u64 = 0x0C;       // Ports Implemented
const HBA_GHCR_HR: u32 = 1;     // HBA Reset bit

const PORT_BASE: u64 = 0x100;   // Each port register block is 0x80
const PORT_SIZE: u64 = 0x80;
const PORT_CLB: u64 = 0x00;     // Command List Base (lower 32)
const PORT_CLBU: u64 = 0x04;    // Command List Base upper 32
const PORT_FB: u64 = 0x08;      // FIS Base (lower 32)
const PORT_FBU: u64 = 0x0C;     // FIS Base upper 32
const PORT_IE: u64 = 0x14;      // Interrupt Enable
const PORT_CMD: u64 = 0x18;     // Command and Status
const PORT_SIG: u64 = 0x24;     // Signature
const PORT_SSTS: u64 = 0x28;    // Serial ATA Status
const PORT_CI: u64 = 0x38;      // Command Issue

const CMD_ST: u32 = 1;
const CMD_FRE: u32 = 1 << 4;
const CMD_POD: u32 = 1 << 2;
const CMD_SUD: u32 = 1 << 1;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;

const SIG_ATA: u32 = 0x00000101;
const SIG_ATAPI: u32 = 0xEB140101;

const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;

fn mmio_read32(base: u64, offset: u64) -> u32 {
    unsafe { (base as *const u32).add(offset as usize / 4).read_volatile() }
}

fn mmio_write32(base: u64, offset: u64, val: u32) {
    unsafe { (base as *mut u32).add(offset as usize / 4).write_volatile(val) }
}

fn mmio_or32(base: u64, offset: u64, bits: u32) {
    let v = mmio_read32(base, offset);
    mmio_write32(base, offset, v | bits);
}

fn mmio_and32(base: u64, offset: u64, bits: u32) {
    let v = mmio_read32(base, offset);
    mmio_write32(base, offset, v & bits);
}

fn wait_while(base: u64, offset: u64, mask: u32, timeout: u32) -> bool {
    for _ in 0..timeout {
        if mmio_read32(base, offset) & mask == 0 {
            return true;
        }
        for _ in 0..100 { core::hint::spin_loop(); }
    }
    false
}

fn wait_until(base: u64, offset: u64, mask: u32, timeout: u32) -> bool {
    for _ in 0..timeout {
        if mmio_read32(base, offset) & mask != 0 {
            return true;
        }
        for _ in 0..100 { core::hint::spin_loop(); }
    }
    false
}

// ── PCI scan ───────────────────────────────────────────────────────

fn find_ahci_bdf() -> Option<u64> {
    for bus in 0..1 {
        for dev in 0..32 {
            for func in 0..8 {
                let bdf = ((bus as u64) << 20) | ((dev as u64) << 15) | ((func as u64) << 12);
                let vd = sys_pci_read(bdf, 0, 4);
                if vd == 0 || vd == 0xFFFFFFFF {
                    if func == 0 { break; }
                    continue;
                }
                let class = sys_pci_read(bdf, 8, 4);
                let class_code = (class >> 24) as u8;
                let subclass = ((class >> 16) & 0xFF) as u8;
                if class_code == 0x01 && subclass == 0x06 {
                    return Some(bdf);
                }
            }
        }
    }
    None
}

fn get_bar5(bdf: u64) -> u64 {
    let low = sys_pci_read(bdf, 0x24, 4);
    let high = sys_pci_read(bdf, 0x28, 4);
    let addr = (low & !0xF) as u64 | ((high as u64) << 32);
    addr
}

fn pci_enable_bus_master(bdf: u64) {
    let cmd = sys_pci_read(bdf, 4, 2);
    sys_pci_write(bdf, 4, 2, cmd | 0x6);
}

// ── AHCI init ──────────────────────────────────────────────────────

fn ahci_init(abar: u64) -> bool {
    // Check capabilities
    let cap = mmio_read32(abar, HBA_CAP);
    let n_ports = ((cap >> 8) & 0x1F) + 1;
    let pi = mmio_read32(abar, HBA_PI);

    // HBA reset
    mmio_write32(abar, HBA_GHC, HBA_GHCR_HR);
    if !wait_while(abar, HBA_GHC, HBA_GHCR_HR, 10000) {
        return false;
    }
    mmio_write32(abar, HBA_GHC, 0);
    if !wait_while(abar, HBA_GHC, HBA_GHCR_HR, 10000) {
        return false;
    }

    // Enable AHCI mode
    mmio_or32(abar, HBA_GHC, 1 << 31);

    let mut found = false;
    for p in 0..n_ports {
        if pi & (1 << p) == 0 { continue; }
        let port_base = abar + PORT_BASE + p as u64 * PORT_SIZE;
        let sig = mmio_read32(port_base, PORT_SIG);
        if sig == SIG_ATA || sig == SIG_ATAPI {
            found |= init_port(port_base, abar);
        }
    }
    found
}

fn init_port(port: u64, _abar: u64) -> bool {
    // Spin up device
    mmio_or32(port, PORT_CMD, CMD_POD | CMD_SUD);

    // Wait for device to spin up
    if !wait_until(port, PORT_SSTS, 0x0F, 10000) {
        mmio_and32(port, PORT_CMD, !CMD_POD);
        return false;
    }

    // Stop port DMA
    mmio_and32(port, PORT_CMD, !CMD_ST);
    wait_while(port, PORT_CMD, CMD_CR, 1000);

    mmio_and32(port, PORT_CMD, !CMD_FRE);
    wait_while(port, PORT_CMD, CMD_FR, 1000);

    // Allocate and set command list (1K aligned)
    let clb_phys = sys_dma_alloc(1024);
    if clb_phys == 0 || clb_phys == !0u64 { return false; }
    mmio_write32(port, PORT_CLB, clb_phys as u32);
    mmio_write32(port, PORT_CLBU, (clb_phys >> 32) as u32);

    // Allocate and set FIS (256 bytes)
    let fb_phys = sys_dma_alloc(256);
    if fb_phys == 0 || fb_phys == !0u64 { return false; }
    mmio_write32(port, PORT_FB, fb_phys as u32);
    mmio_write32(port, PORT_FBU, (fb_phys >> 32) as u32);

    // Enable interrupts
    mmio_write32(port, PORT_IE, 0);

    // Start port DMA
    mmio_or32(port, PORT_CMD, CMD_FRE);
    mmio_or32(port, PORT_CMD, CMD_ST);

    true
}

// ── Disk operation ──────────────────────────────────────────────────────

fn ahci_op(port_base: u64, sector: u64, count: u16, dma_phys: u64, write: bool) -> bool {
    let clb_phys = {
        let clb_low = mmio_read32(port_base, PORT_CLB);
        let clb_high = mmio_read32(port_base, PORT_CLBU);
        (clb_low as u64) | ((clb_high as u64) << 32)
    };
    let cmd_list_virt = clb_phys; // already phys addr for DMA, just use directly
    let cmd_header = virt_from_phys(cmd_list_virt);
    let ct_phys = sys_dma_alloc(256);
    if ct_phys == 0 || ct_phys == !0u64 { return false; }
    let ct_virt = virt_from_phys(ct_phys);

    // Clear command header (slot 0)
    for i in 0..32 {
        unsafe { (cmd_header as *mut u32).add(i).write_volatile(0) };
    }

    // PRDT: one entry for the full transfer
    let byte_count = (count as u64).saturating_mul(512);
    let prdt_count = (byte_count.saturating_add(0x1FFFFF) / 0x200000) as u16;
    let prdt_count_final = if prdt_count == 0 { 1 } else { prdt_count };

    // Write command header: 1 PRDT entry, command FIS length = 5 DW
    let cfl = 5; // 5 DWORDS = 20 bytes
    let w = (cfl as u32) | ((prdt_count_final as u32) << 16) | if write { 0x40 } else { 0 }; // W bit
    unsafe { (cmd_header as *mut u32).offset(0).write_volatile(w) };
    // PRDBC: 0
    unsafe { (cmd_header as *mut u32).offset(1).write_volatile(0) };
    // CTBA
    unsafe { (cmd_header as *mut u32).offset(2).write_volatile(ct_phys as u32) };
    unsafe { (cmd_header as *mut u32).offset(3).write_volatile((ct_phys >> 32) as u32) };
    // Reserved + reserved
    unsafe { (cmd_header as *mut u32).offset(4).write_volatile(0) };
    unsafe { (cmd_header as *mut u32).offset(5).write_volatile(0) };

    // PRDT entries ... (same as before)
    for i in 0..prdt_count_final as usize {
        let prdt_base = ct_virt + 128u64 + (i as u64) * 16u64;
        let dma_offset = (i as u64) * 0x200000;
        let dba = dma_phys + dma_offset;
        let dbc = if i == prdt_count_final as usize - 1 {
            let total_bytes = (count as u64).saturating_mul(512);
            let last = total_bytes.saturating_sub(dma_offset);
            if last == 0 { 0x1FFFFF }
            else if last > 0x200000 { 0x1FFFFF }
            else { (last - 1) as u32 }
        } else {
            0x1FFFFFu32
        } | 0x80000000; // I bit
        unsafe {
            (prdt_base as *mut u32).offset(0).write_volatile(dba as u32);
            (prdt_base as *mut u32).offset(1).write_volatile((dba >> 32) as u32);
            (prdt_base as *mut u32).offset(2).write_volatile(0);
            (prdt_base as *mut u32).offset(3).write_volatile(dbc);
        }
    }

    // Build command FIS (at bytes 0 of command table)
    let fis_base = ct_virt;
    // FIS type 0x27 = Register H2D
    unsafe {
        (fis_base as *mut u8).offset(0).write_volatile(0x27);
        (fis_base as *mut u8).offset(1).write_volatile(0x80); // C bit = command
        (fis_base as *mut u8).offset(2).write_volatile(if write { ATA_CMD_WRITE_DMA_EXT } else { ATA_CMD_READ_DMA_EXT });
        (fis_base as *mut u8).offset(3).write_volatile(0);
        // LBA low
        (fis_base as *mut u8).offset(4).write_volatile((sector & 0xFF) as u8);
        (fis_base as *mut u8).offset(5).write_volatile(((sector >> 8) & 0xFF) as u8);
        (fis_base as *mut u8).offset(6).write_volatile(((sector >> 16) & 0xFF) as u8);
        // Device
        (fis_base as *mut u8).offset(7).write_volatile(0x40);
        // LBA high
        (fis_base as *mut u8).offset(8).write_volatile(((sector >> 24) & 0xFF) as u8);
        (fis_base as *mut u8).offset(9).write_volatile(((sector >> 32) & 0xFF) as u8);
        (fis_base as *mut u8).offset(10).write_volatile(((sector >> 40) & 0xFF) as u8);
        // Features
        (fis_base as *mut u8).offset(11).write_volatile(0);
        // Sector count
        (fis_base as *mut u8).offset(12).write_volatile((count & 0xFF) as u8);
        (fis_base as *mut u8).offset(13).write_volatile(((count >> 8) & 0xFF) as u8);
        // Control
        (fis_base as *mut u8).offset(15).write_volatile(0);
    }

    // Issue command
    mmio_write32(port_base, PORT_CI, 1);

    // Wait for completion
    let ok = wait_while(port_base, PORT_CI, 1, 1000000);

    // Free command table DMA buffer
    sys_dma_free(ct_phys, 256);

    ok
}

fn ahci_read(port_base: u64, sector: u64, count: u16, dma_phys: u64) -> bool {
    ahci_op(port_base, sector, count, dma_phys, false)
}

fn ahci_write(port_base: u64, sector: u64, count: u16, dma_phys: u64) -> bool {
    ahci_op(port_base, sector, count, dma_phys, true)
}

fn virt_from_phys(phys: u64) -> u64 {
    // The kernel maps physical memory at HIGHER_HALF
    // We can access it from within the driver's address space too
    // since the driver's PML4 is a fork of the kernel's.
    0xFFFF_8000_0000_0000u64 + phys
}

fn reject_loop() -> ! {
    let mut buf = [0u64; 4];
    loop { let _ = sys_driver_recv(&mut buf); sys_driver_send(!0); }
}

// ── Entry point ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    sys_gdf_register(b"ahci");

    // Find AHCI controller
    let bdf = match find_ahci_bdf() {
        Some(b) => b,
        None => reject_loop(),
    };

    pci_enable_bus_master(bdf);

    let abar_phys = get_bar5(bdf);
    if abar_phys == 0 { reject_loop(); }

    let abar_virt = sys_mmap_phys(abar_phys, 0x10000);
    if abar_virt == 0 || abar_virt == !0u64 { reject_loop(); }

    if !ahci_init(abar_virt) { reject_loop(); }

    // Find first ATA port
    let pi = mmio_read32(abar_virt, HBA_PI);
    let mut port_base = 0u64;
    for p in 0..32 {
        if pi & (1 << p) == 0 { continue; }
        let pb = abar_virt + PORT_BASE + p as u64 * PORT_SIZE;
        let ssts = mmio_read32(pb, PORT_SSTS) & 0x0F;
        if ssts == 0x03 {
            let sig = mmio_read32(pb, PORT_SIG);
            if sig == SIG_ATA || sig == SIG_ATAPI {
                port_base = pb;
                break;
            }
        }
    }

    if port_base == 0 { reject_loop(); }

    // Command loop
    let mut buf = [0u64; 4];
    loop {
        let ret = sys_driver_recv(&mut buf);
        if ret == 0 {
            match buf[0] {
                10 => { // read
                    let sector = buf[1];
                    let count = buf[2] as u16;
                    let dma_phys = if buf[3] != 0 {
                        buf[3]
                    } else {
                        let alloc = ((count as u64) * 512 + 0xFFF) & !0xFFF;
                        sys_dma_alloc(alloc)
                    };
                    if dma_phys == 0 || dma_phys == !0u64 {
                        sys_driver_send(!0);
                    } else if ahci_read(port_base, sector, count, dma_phys) {
                        sys_driver_send(dma_phys);
                    } else {
                        sys_driver_send(!0);
                    }
                }
                11 => { // write
                    let sector = buf[1];
                    let count = buf[2] as u16;
                    let dma_phys = buf[3];
                    if dma_phys == 0 || dma_phys == !0u64 {
                        sys_driver_send(!0);
                    } else if ahci_write(port_base, sector, count, dma_phys) {
                        sys_driver_send(0);
                    } else {
                        sys_driver_send(!0);
                    }
                }
                20 => {
                    let size = (buf[1] + 0xFFF) & !0xFFF;
                    let phys = sys_dma_alloc(size);
                    if phys == 0 || phys == !0u64 {
                        sys_driver_send(!0);
                    } else {
                        sys_driver_send(phys);
                    }
                }
                _ => { sys_driver_send(u64::MAX); }
            }
        }
    }
}
