#![no_main]
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { x86_64::instructions::interrupts::disable(); x86_64::instructions::hlt(); }
}

// ── Syscalls (AbstractionDriver: 20, 21, 30, 31 only) ──────────────

fn syscall7(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    let r; unsafe { core::arch::asm!("syscall", inout("rax") nr => r,
        in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5,
        lateout("rcx") _, lateout("r11") _); } r
}
fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let r; unsafe { core::arch::asm!("syscall", inout("rax") nr => r,
        in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4,
        lateout("rcx") _, lateout("r11") _); } r
}
fn sys_gdf_register(name: &[u8]) -> u64 {
    syscall6(30, name.as_ptr() as u64, name.len() as u64, 0, 0, 0)
}
fn sys_dma_free(phys: u64, size: u64) {
    syscall6(14, phys, size, 0, 0, 0);
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
fn sys_driver_call(name: &[u8], cmd: u32, a0: u64, a1: u64, a2: u64) -> u64 {
    syscall7(31, name.as_ptr() as u64, name.len() as u64, cmd as u64, a0, a1, a2)
}

// ── Physical memory via kernel's higher-half direct map ────────────
const HH: u64 = 0xFFFF_8000_0000_0000;

fn r8(p: u64, o: u64) -> u8 { unsafe { ((HH + p) as *const u8).add(o as usize).read() } }
fn r16(p: u64, o: u64) -> u16 { unsafe { ((HH + p) as *const u16).add(o as usize / 2).read() } }
fn r32(p: u64, o: u64) -> u32 { unsafe { ((HH + p) as *const u32).add(o as usize / 4).read() } }
fn w8(p: u64, o: u64, v: u8) { unsafe { ((HH + p) as *mut u8).add(o as usize).write(v) } }

// ── Ext4 constants ─────────────────────────────────────────────────

const EXT4_MAGIC: u16 = 0xEF53;
const EXT4_EXTENTS_FL: u32 = 0x80000;
const EXT4_EXTENT_MAGIC: u16 = 0xF30A;
const INODE_ROOT: u32 = 2;
const PART_LBA: u64 = 2048;

// ── Ext4 filesystem ────────────────────────────────────────────────

struct Fs {
    use_ide: bool,  // false = use "ahci", true = use "ide"
    part_lba: u64,
    block_size: u64,
    sec_per_blk: u64,
    inodes_per_g: u32,
    inode_size: u16,
    bg_blk: u64,
}

impl Fs {
    fn drv(&self) -> &[u8] { if self.use_ide { b"ide" } else { b"ahci" } }

    fn read_via(&self, name: &[u8], sector: u64, count: u64, dst: u64) -> bool {
        let r = sys_driver_call(name, 10, sector, count, dst);
        r == dst
    }

    #[allow(dead_code)]
    fn write_via(&self, name: &[u8], sector: u64, count: u64, src: u64) -> bool {
        let r = sys_driver_call(name, 11, sector, count, src);
        r == 0
    }

    fn alloc_via(&self, name: &[u8], size: u64) -> u64 {
        let r = sys_driver_call(name, 20, size, 0, 0);
        if r == 0 || r == !0u64 { !0 } else { r }
    }

    fn mount() -> Option<Self> {
        // Retry mount: ahci/ide may not have registered yet when ext4 starts.
        // Spin up to ~500ms (500 iterations × ~1ms each via syscall overhead).
        for attempt in 0..500 {
            // Probe AHCI first
            let sb = sys_driver_call(b"ahci", 10, PART_LBA + 2, 2, 0);
            let (use_ide, sb_phys) = if sb != 0 && sb != !0u64 && r16(sb, 56) == EXT4_MAGIC {
                (false, sb)
            } else {
                // Fall back to IDE
                let sb = sys_driver_call(b"ide", 10, PART_LBA + 2, 2, 0);
                if sb == 0 || sb == !0u64 {
                    // Neither ahci nor ide responded yet — retry
                    if attempt < 499 {
                        for _ in 0..10000 { core::hint::spin_loop(); }
                        continue;
                    }
                    return None;
                }
                if r16(sb, 56) != EXT4_MAGIC { return None; }
                (true, sb)
            };

            let lbs = r32(sb_phys, 24) as u64;
            let bs = 1024u64 << lbs;
            return Some(Fs {
                use_ide,
                part_lba: PART_LBA,
                block_size: bs,
                sec_per_blk: bs / 512,
                inodes_per_g: r32(sb_phys, 40),
                inode_size: r16(sb_phys, 88),
                bg_blk: if bs > 1024 { 1 } else { 2 },
            });
        }
        None
    }

    fn alloc_buf(&self, size: u64) -> u64 {
        self.alloc_via(self.drv(), size)
    }

    fn read_blk(&self, block: u64, dst: u64) -> bool {
        let lba = self.part_lba + block * self.sec_per_blk;
        self.read_via(self.drv(), lba, self.sec_per_blk, dst)
    }

    #[allow(dead_code)]
    fn write_blk(&self, block: u64, src: u64) -> bool {
        let lba = self.part_lba + block * self.sec_per_blk;
        self.write_via(self.drv(), lba, self.sec_per_blk, src)
    }

    fn read_inode(&self, ino: u32, dst: u64) -> bool {
        let g = (ino - 1) / self.inodes_per_g;
        let idx = (ino - 1) % self.inodes_per_g;

        let bg = self.alloc_buf(self.block_size);
        if bg == !0 || !self.read_blk(self.bg_blk + g as u64 * self.sec_per_blk, bg) { return false; }

        let tbl_lo = r32(bg, 8);
        let tbl_hi = r32(bg, 24);
        let tbl = (tbl_lo as u64) | ((tbl_hi as u64) << 32);

        let off = idx as u64 * self.inode_size as u64;
        let blk = tbl + off / self.block_size;
        if !self.read_blk(blk, dst) { sys_dma_free(bg, self.block_size); return false; }

        let in_blk = off % self.block_size;
        if in_blk != 0 {
            let inode_sz = self.inode_size as u64;
            let copy_end = inode_sz.min(self.block_size - in_blk);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (HH + dst + in_blk) as *const u8,
                    (HH + dst) as *mut u8,
                    copy_end as usize,
                );
            }
        }
        sys_dma_free(bg, self.block_size);
        true
    }

    // ── Ext4 Write support (minimal) ──────────────────────────────────
    
    // Note: Does not support resizing files (no inode/extent updates)
    #[allow(dead_code)]
    fn write_file_data(&self, ino: u32, _offset: u64, _data: &[u8], _scratch: u64) -> bool {
        let inode = self.alloc_buf(self.block_size);
        if inode == !0 || !self.read_inode(ino, inode) { return false; }
        
        // ... (simplified: assume data fits in one block, or handle block by block)
        // This is highly complex. For now, I will implement a placeholder
        // that indicates this is where the implementation goes.
        sys_dma_free(inode, self.block_size);
        false
    }

    fn read_file(&self, ino: u32, file_phys: u64) -> Option<u64> {
        let inode = self.alloc_buf(self.block_size);
        if inode == !0 || !self.read_inode(ino, inode) { return None; }

        let sz_lo = r32(inode, 4);
        let sz_hi = r32(inode, 108);
        let sz = ((sz_hi as u64) << 32) | sz_lo as u64;
        if sz == 0 { sys_dma_free(inode, self.block_size); return Some(0); }

        let flags = r32(inode, 32);
        if flags & EXT4_EXTENTS_FL == 0 { sys_dma_free(inode, self.block_size); return None; }

        let ext = self.alloc_buf(self.block_size);
        let dat = self.alloc_buf(self.block_size);
        if ext == !0 || dat == !0 { sys_dma_free(inode, self.block_size); return None; }

        for i in 0u64..60 { w8(ext, i, r8(inode, 40 + i)); }
        sys_dma_free(inode, self.block_size);

        let mut dst = 0u64;
        loop {
            let hdr = r16(ext, 0);
            if hdr != EXT4_EXTENT_MAGIC { sys_dma_free(ext, self.block_size); sys_dma_free(dat, self.block_size); return None; }
            let depth = r16(ext, 6);
            let entries = r16(ext, 2) as usize;

            if depth == 0 {
                for e in 0..entries {
                    let eo = (12 + e * 12) as u64;
                    let lv = r16(ext, eo + 4) as u64;
                    let sh = r16(ext, eo + 6) as u64;
                    let sl = r32(ext, eo + 8) as u64;
                    let start = (sh << 32) | sl;
                    let elen = lv & 0x7FFF;

                    for b in 0..elen {
                        let copy = self.block_size.min(sz.saturating_sub(dst));
                        if copy == 0 { sys_dma_free(ext, self.block_size); sys_dma_free(dat, self.block_size); return Some(sz); }
                        if !self.read_blk(start + b, dat) { sys_dma_free(ext, self.block_size); sys_dma_free(dat, self.block_size); return None; }
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (HH + dat) as *const u8,
                                (HH + file_phys + dst) as *mut u8,
                                copy as usize,
                            );
                        }
                        dst += copy;
                    }
                }
                sys_dma_free(ext, self.block_size);
                sys_dma_free(dat, self.block_size);
                return Some(sz);
            } else {
                let ll = r32(ext, 16) as u64;
                let lh = r16(ext, 20) as u64;
                if !self.read_blk((lh << 32) | ll, ext) { sys_dma_free(ext, self.block_size); sys_dma_free(dat, self.block_size); return None; }
            }
        }
    }

    fn find_in_dir(&self, dir_ino: u32, name: &[u8], scratch: u64) -> Option<u32> {
        let dir_sz = self.read_file(dir_ino, scratch)?;
        if dir_sz == 0 { return None; }

        let mut off = 0u64;
        while off + 8 <= dir_sz {
            let ino = r32(scratch, off);
            if ino == 0 { off += 8; continue; }
            let rl = r16(scratch, off + 4) as u64;
            if rl < 8 { break; }
            let nl = r8(scratch, off + 6) as usize;

            if nl == name.len() {
                let mut ok = true;
                for i in 0..nl {
                    if r8(scratch, off + 8 + i as u64) != name[i] { ok = false; break; }
                }
                if ok { return Some(ino); }
            }
            off += rl;
        }
        None
    }

    // ── Entry point ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    sys_gdf_register(b"ext4");

    let fs = match Fs::mount() {
        Some(f) => f,
        None => loop {
            let mut b2 = [0u64; 4];
            let _ = sys_driver_recv(&mut b2);
            sys_driver_send(!0);
        },
    };

    let result = fs.alloc_buf(4096);
    let scratch = fs.alloc_buf(fs.block_size);
    let mut last_sz = 0u64;

    let mut buf = [0u64; 4];
    loop {
        let ret = sys_driver_recv(&mut buf);
        if ret != 0 { continue; }
        match buf[0] {
            1 => {
                let ino = fs.find_in_dir(INODE_ROOT, b"file.txt", scratch);
                match ino.and_then(|i| fs.read_file(i, result)) {
                    Some(sz) => { last_sz = sz; sys_driver_send(result); }
                    None => { sys_driver_send(!0); }
                };
            }
            2 => { sys_driver_send(last_sz); }
            _ => { sys_driver_send(!0); }
        }
    }
}
}
