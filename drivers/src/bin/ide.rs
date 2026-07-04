#![no_main]
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("cli; hlt") } }
}

fn syscall(nr: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let r; unsafe { core::arch::asm!("syscall",
        inout("rax") nr => r, in("rdi") arg0, in("rsi") arg1, in("rdx") arg2,
        lateout("rcx") _, lateout("r11") _); } r
}

fn sys_dma_alloc(size: u64) -> u64 {
    let alloc = (size + 0xFFF) & !0xFFF;
    syscall(13, alloc, 0, 0)
}

fn sys_gdf_register(name: &[u8]) -> u64 {
    syscall(30, name.as_ptr() as u64, name.len() as u64, 0)
}

fn sys_driver_recv(out: &mut [u64; 4]) -> u64 {
    let r; unsafe { core::arch::asm!("syscall", inout("rax") 20u64 => r,
        in("rdi") out.as_mut_ptr() as u64, in("rsi") 0, in("rdx") 0,
        lateout("rcx") _, lateout("r11") _); } r
}

fn sys_driver_send(v: u64) -> u64 {
    let r; unsafe { core::arch::asm!("syscall", inout("rax") 21u64 => r,
        in("rdi") v, in("rsi") 0, in("rdx") 0,
        lateout("rcx") _, lateout("r11") _); } r
}

// ── Port I/O ───────────────────────────────────────────────────────

fn outb(port: u16, val: u8) {
    unsafe { x86_64::instructions::port::Port::<u8>::new(port).write(val) }
}

fn inb(port: u16) -> u8 {
    unsafe { x86_64::instructions::port::Port::<u8>::new(port).read() }
}

fn inw(port: u16) -> u16 {
    unsafe { x86_64::instructions::port::Port::<u16>::new(port).read() }
}

// ── IDE registers (primary channel, legacy) ────────────────────────

const IDE_DATA: u16 = 0x1F0;
const IDE_SEC_COUNT: u16 = 0x1F2;
const IDE_LBA_LO: u16 = 0x1F3;
const IDE_LBA_MI: u16 = 0x1F4;
const IDE_LBA_HI: u16 = 0x1F5;
const IDE_DRIVE: u16 = 0x1F6;
const IDE_CMD: u16 = 0x1F7;
const IDE_STATUS: u16 = 0x1F7;

const ATA_READ: u8 = 0x20;
const STS_BSY: u8 = 0x80;
const STS_DRQ: u8 = 0x08;
const STS_ERR: u8 = 0x01;

// ── PIO read ───────────────────────────────────────────────────────

fn ide_poll() -> bool {
    for _ in 0..10000000 {
        let sts = inb(IDE_STATUS);
        if sts & STS_BSY == 0 {
            if sts & STS_ERR != 0 { return false; }
            if sts & STS_DRQ != 0 { return true; }
            return false;
        }
        for _ in 0..10 { core::hint::spin_loop(); }
    }
    false
}

fn ide_read_sectors(sector: u64, count: u16, buf_phys: u64) -> bool {
    // 28-bit LBA only (disk is < 128 GB)
    if sector > 0x0FFF_FFFF { return false; }

    outb(IDE_DRIVE, 0xE0 | ((sector >> 24) as u8 & 0x0F));
    outb(IDE_SEC_COUNT, count as u8);
    outb(IDE_LBA_LO, sector as u8);
    outb(IDE_LBA_MI, (sector >> 8) as u8);
    outb(IDE_LBA_HI, (sector >> 16) as u8);
    outb(IDE_CMD, ATA_READ);

    let mut buf_ptr = (0xFFFF_8000_0000_0000u64 + buf_phys) as *mut u16;
    for _s in 0..count {
        if !ide_poll() { return false; }
        // Read 256 words (512 bytes) per sector
        for _w in 0..256 {
            unsafe { buf_ptr.write_volatile(inw(IDE_DATA)); buf_ptr = buf_ptr.add(1); }
        }
    }
    true
}

// ── Entry point ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    sys_gdf_register(b"ide");

    let mut buf = [0u64; 4];
    loop {
        let ret = sys_driver_recv(&mut buf);
        if ret != 0 { continue; }
        match buf[0] {
            10 => {
                // READ_BLOCKS(sector, count, dma_phys)
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
                } else if ide_read_sectors(sector, count, dma_phys) {
                    sys_driver_send(dma_phys);
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
            _ => { sys_driver_send(!0); }
        }
    }
}
