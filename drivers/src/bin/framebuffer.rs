#![no_main]
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { x86_64::instructions::interrupts::disable(); x86_64::instructions::hlt(); }
}

// ── Syscall wrappers ──────────────────────────────────────────────

fn syscall(nr: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => result,
            in("rdi") arg0, in("rsi") arg1, in("rdx") arg2,
            lateout("rcx") _, lateout("r11") _,
        );
    }
    result
}

fn sys_mmap_phys(phys: u64, size: u64) -> u64 {
    syscall(10, phys, size, 0)
}

fn sys_mmap(size: u64) -> u64 {
    syscall(5, 0, size, 0)
}

fn sys_gdf_register(name: &[u8]) -> u64 {
    syscall(30, name.as_ptr() as u64, name.len() as u64, 0)
}

fn sys_driver_send(v: u64) -> u64 {
    syscall(21, v, 0, 0)
}

fn sys_driver_recv_block(out: &mut [u64; 4]) -> u64 {
    syscall(22, out.as_mut_ptr() as u64, 0, 0)
}

// ── Command constants (must match system/src/lib.rs) ──────────────

const FB_CMD_ACQUIRE: u32 = 0xFF;
const FB_CMD_CLEAR: u32 = 2;
const FB_CMD_DRAW_TEXT: u32 = 5;
const FB_CMD_SET_FG: u32 = 6;
const FB_CMD_SET_BG: u32 = 7;
const FB_CMD_SCROLL: u32 = 8;
const FB_CMD_PRESENT: u32 = 10;

// ── Types ─────────────────────────────────────────────────────────

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;
const FONT: &[u8; 2048] = include_bytes!("../../font.bin");

struct Fb {
    fb_ptr: *mut u8,
    shadow: *mut u8,
    w: usize,
    h: usize,
    stride: usize,
    bpp: usize,
    is_bgr: bool,
    fg_r: u8,
    fg_g: u8,
    fg_b: u8,
    bg_r: u8,
    bg_g: u8,
    bg_b: u8,
}

impl Fb {
    /// Create Fb from ACQUIRE command arguments.
    /// `fb_phys` = arg0, `fb_size` = arg1, `packed` = arg2.
    /// Packed layout:
    ///   bits  0-15: width
    ///   bits 16-31: height
    ///   bits 32-47: stride (bytes per row)
    ///   bits 48-55: bpp
    ///   bit     56: is_bgr
    fn acquire(fb_phys: u64, fb_size: u64, packed: u64) -> Option<Self> {
        let w = (packed & 0xFFFF) as usize;
        let h = ((packed >> 16) & 0xFFFF) as usize;
        let stride = ((packed >> 32) & 0xFFFF) as usize;
        let bpp = ((packed >> 48) & 0xFF) as usize;
        let is_bgr = ((packed >> 56) & 1) != 0;

        if fb_phys == 0 || w == 0 || h == 0 || bpp != 4 {
            return None;
        }

        let fb_ptr = sys_mmap_phys(fb_phys, fb_size);
        if fb_ptr == 0 || fb_ptr == !0u64 { return None; }

        let shadow = sys_mmap(fb_size);
        if shadow == 0 || shadow == !0u64 { return None; }

        Some(Self {
            fb_ptr: fb_ptr as *mut u8,
            shadow: shadow as *mut u8,
            w,
            h,
            stride,
            bpp,
            is_bgr,
            fg_r: 200,
            fg_g: 255,
            fg_b: 100,
            bg_r: 0,
            bg_g: 0,
            bg_b: 0,
        })
    }

    fn set_pixel(&self, buf: *mut u8, x: usize, y: usize, r: u8, g: u8, b: u8) {
        if x >= self.w || y >= self.h { return; }
        let off = y * self.stride + x * self.bpp;
        unsafe {
            let p = buf.add(off);
            if self.is_bgr {
                p.write_volatile(b);
                p.add(1).write_volatile(g);
                p.add(2).write_volatile(r);
            } else {
                p.write_volatile(r);
                p.add(1).write_volatile(g);
                p.add(2).write_volatile(b);
            }
            p.add(3).write_volatile(0);
        }
    }

    fn clear(&self) {
        let size = self.h * self.stride;
        unsafe {
            core::ptr::write_bytes(self.shadow, 0, size);
        }
    }

    fn fill_rect(&self, x0: usize, y0: usize, x1: usize, y1: usize, r: u8, g: u8, b: u8) {
        for y in y0..y1.min(self.h) {
            for x in x0..x1.min(self.w) {
                self.set_pixel(self.shadow, x, y, r, g, b);
            }
        }
    }

    fn put_char(&self, ch: u8, x: usize, y: usize, fr: u8, fg: u8, fb: u8, br: u8, bg: u8, bb: u8) {
        if ch >= 128 { return; }
        let base = (ch as usize) * GLYPH_H;
        // Background fill
        self.fill_rect(x, y, x + GLYPH_W, y + GLYPH_H, br, bg, bb);
        // Foreground
        for gy in 0..GLYPH_H {
            let row = FONT[base + gy];
            for gx in 0..GLYPH_W {
                if (row >> (7 - gx)) & 1 != 0 {
                    self.set_pixel(self.shadow, x + gx, y + gy, fr, fg, fb);
                }
            }
        }
    }

    fn draw_text(&self, text_virt: u64, text_len: u64, start_x: usize, start_y: usize) {
        if text_virt == 0 || text_len == 0 { return; }
        let max = text_len.min(4096) as usize;
        let mut cx = start_x;
        let mut cy = start_y;
        unsafe {
            for i in 0..max {
                let ch = (text_virt as *const u8).add(i).read();
                match ch {
                    b'\n' => { cx = start_x; cy += GLYPH_H + 2; }
                    b'\r' => { cx = start_x; }
                    b'\t' => { cx = (cx + 4 * GLYPH_W) & !(4 * GLYPH_W - 1); }
                    32..=127 => {
                        self.put_char(ch, cx, cy, self.fg_r, self.fg_g, self.fg_b,
                                      self.bg_r, self.bg_g, self.bg_b);
                        cx += GLYPH_W;
                    }
                    _ => {}
                }
            }
        }
    }

    fn scroll(&self, lines: usize) {
        if lines == 0 || self.h == 0 { return; }
        let row_bytes = GLYPH_H + 2;
        let shift = lines * row_bytes * self.stride;
        let total = self.h * self.stride;
        if shift >= total {
            self.clear();
            return;
        }
        unsafe {
            core::ptr::copy(self.shadow.add(shift), self.shadow, total - shift);
            core::ptr::write_bytes(self.shadow.add(total - shift), 0, shift);
        }
    }

    fn present(&self) {
        let size = self.h * self.stride;
        unsafe {
            core::ptr::copy_nonoverlapping(self.shadow, self.fb_ptr, size);
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    sys_gdf_register(b"framebuffer");

    let mut fb: Option<Fb> = None;
    let mut cmd_buf = [0u64; 4];

    loop {
        let ret = sys_driver_recv_block(&mut cmd_buf);
        if ret != 0 { continue; }

        let cmd = cmd_buf[0] as u32;
        let arg0 = cmd_buf[1];
        let arg1 = cmd_buf[2];
        let arg2 = cmd_buf[3];

        match cmd {
            FB_CMD_ACQUIRE => {
                if fb.is_some() { sys_driver_send(1); continue; }
                // arg0=fb_phys, arg1=fb_size, arg2=packed geometry
                match Fb::acquire(arg0, arg1, arg2) {
                    Some(f) => {
                        f.clear();
                        f.present();
                        fb = Some(f);
                        sys_driver_send(0);
                    }
                    None => { sys_driver_send(2); }
                }
            }

            FB_CMD_CLEAR => {
                if let Some(ref f) = fb {
                    f.clear();
                    sys_driver_send(0);
                } else { sys_driver_send(2); }
            }

            FB_CMD_DRAW_TEXT => {
                if let Some(ref f) = fb {
                    // arg0 = text_phys, arg1 = text_len, arg2 = packed (x<<32 | y)
                    let text_phys = arg0;
                    let text_len = arg1;
                    let start_x = (arg2 >> 32) as usize;
                    let start_y = (arg2 & 0xFFFF_FFFF) as usize;

                    let text_virt = if text_phys != 0 && text_len != 0 {
                        let pages = ((text_len + 0xFFF) / 0x1000) * 0x1000;
                        sys_mmap_phys(text_phys, pages)
                    } else {
                        0
                    };

                    if text_virt == 0 && text_len != 0 {
                        sys_driver_send(2);
                    } else {
                        f.draw_text(text_virt, text_len, start_x, start_y);
                        sys_driver_send(0);
                    }
                } else { sys_driver_send(2); }
            }

            FB_CMD_SET_FG => {
                if let Some(ref mut f) = fb {
                    f.fg_r = ((arg0 >> 16) & 0xFF) as u8;
                    f.fg_g = ((arg0 >> 8) & 0xFF) as u8;
                    f.fg_b = (arg0 & 0xFF) as u8;
                    sys_driver_send(0);
                } else { sys_driver_send(2); }
            }

            FB_CMD_SET_BG => {
                if let Some(ref mut f) = fb {
                    f.bg_r = ((arg0 >> 16) & 0xFF) as u8;
                    f.bg_g = ((arg0 >> 8) & 0xFF) as u8;
                    f.bg_b = (arg0 & 0xFF) as u8;
                    sys_driver_send(0);
                } else { sys_driver_send(2); }
            }

            FB_CMD_SCROLL => {
                if let Some(ref f) = fb {
                    f.scroll(arg0 as usize);
                    sys_driver_send(0);
                } else { sys_driver_send(2); }
            }

            FB_CMD_PRESENT => {
                if let Some(ref f) = fb {
                    f.present();
                    sys_driver_send(0);
                } else { sys_driver_send(2); }
            }

            _ => { sys_driver_send(1); }
        }
    }
}
