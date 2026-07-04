use core::mem;
use core::ptr;
use crate::mm;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;
const X86_64: u16 = 0x3e;

#[repr(C)]
struct Elf64Header {
    magic: [u8; 4],
    class: u8,
    data: u8,
    version: u8,
    osabi: u8,
    abiversion: u8,
    _pad: [u8; 7],
    type_: u16,
    machine: u16,
    version2: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[repr(C)]
struct Elf64Phdr {
    type_: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

#[repr(C)]
struct Elf64Shdr {
    name: u32,
    type_: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
}

#[derive(Debug)]
pub enum LoadError {
    InvalidMagic,
    NotElf64,
    NotExecutable,
    WrongArch,
    BadHeader,
    NoMem,
    MapFailed,
    NoSymbolTable,
    MisalignedBinary,
}

pub struct LoadResult {
    pub entry: u64,
    pub stack_top: u64,
    pub pml4: u64,
    pub stack_pages: u64,
    pub symtab_phys: u64,
    pub symtab_size: u64,
    pub strtab_phys: u64,
}

fn page_align_down(x: u64) -> u64 {
    x & !0xFFF
}

fn page_align_up(x: u64) -> u64 {
    (x + 0xFFF) & !0xFFF
}

/// Load an ELF64 binary into a fresh address space.
///
/// `stack_size` — number of bytes for the initial stack (will be rounded up).
/// `target_pml4` — if `Some`, maps into that PML4; if `None`, creates a fork
///   of the kernel PML4 and returns the new root.
pub fn load_elf(
    binary: &[u8],
    stack_size: u64,
    target_pml4: Option<u64>,
) -> Result<LoadResult, LoadError> {
    if binary.len() < mem::size_of::<Elf64Header>() {
        return Err(LoadError::InvalidMagic);
    }
    // Use read_unaligned so the binary doesn't need to be pointer-aligned.
    // x86-64 handles unaligned accesses in hardware, and the bootloader
    // may place driver ELFs at arbitrary physical addresses.
    let hdr: Elf64Header = unsafe { ptr::read_unaligned(binary.as_ptr() as *const Elf64Header) };

    if hdr.magic != ELF_MAGIC {
        return Err(LoadError::InvalidMagic);
    }
    if hdr.class != ELFCLASS64 || hdr.data != ELFDATA2LSB {
        return Err(LoadError::NotElf64);
    }
    if hdr.type_ != ET_EXEC && hdr.type_ != ET_DYN {
        return Err(LoadError::NotExecutable);
    }
    if hdr.machine != X86_64 {
        return Err(LoadError::WrongArch);
    }
    if hdr.phentsize as usize != mem::size_of::<Elf64Phdr>() {
        return Err(LoadError::BadHeader);
    }

    let pml4 = target_pml4.unwrap_or_else(mm::virt::kernel_pml4);

    // Track segment allocations so we can free them on error (Bug 19)
    const MAX_PTLOAD: usize = 32;
    let mut seg_phys: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_pages: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_count = 0usize;

    // First pass: collect PT_LOAD segment info & check for page-level overlap
    let mut seg_vaddrs: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_load_vaddrs: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_load_ends: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_pages_counts: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_flags: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_off: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_filesz: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];
    let mut seg_memsz: [u64; MAX_PTLOAD] = [0; MAX_PTLOAD];

    let phdr_base = hdr.phoff as usize;
    for i in 0..hdr.phnum {
        let off = phdr_base + i as usize * mem::size_of::<Elf64Phdr>();
        if off + mem::size_of::<Elf64Phdr>() > binary.len() {
            return Err(LoadError::BadHeader);
        }
        let phdr: Elf64Phdr =
            unsafe { ptr::read_unaligned(binary.as_ptr().add(off) as *const Elf64Phdr) };

        if phdr.type_ != PT_LOAD {
            continue;
        }

        // Validate segment bounds before use (Bug 16)
        let filesz = phdr.filesz;
        let memsz = phdr.memsz;
        let copy_end = phdr.offset.checked_add(filesz).ok_or(LoadError::BadHeader)?;
        if copy_end > binary.len() as u64 {
            return Err(LoadError::BadHeader);
        }
        let seg_end = phdr.vaddr.checked_add(memsz).ok_or(LoadError::BadHeader)?;

        let load_vaddr = page_align_down(phdr.vaddr);
        let load_end = page_align_up(seg_end);
        let pages = (load_end - load_vaddr) / 0x1000;

        let mut map_flags = mm::virt::PRESENT | mm::virt::USER;
        if phdr.flags & PF_W != 0 {
            map_flags |= mm::virt::WRITABLE;
        }
        if phdr.flags & PF_X == 0 {
            map_flags |= mm::virt::NO_EXECUTE;
        }

        // Check for page-level overlap with previously collected segments.
        // If sections share a 4KB page, the later PT_LOAD's map_contiguous call
        // would overwrite the earlier segment's PTEs (e.g. .rodata sets NX on
        // a page shared with .text), causing #PF on code execution.
        for j in 0..seg_count {
            let existing_start = seg_load_vaddrs[j];
            let existing_end = seg_load_ends[j];
            if load_vaddr < existing_end && existing_start < load_end {
                log::error!(
                    "exec: segment {}=[{:#x},{:#x}) overlaps segment {}=[{:#x},{:#x}) at page level. \
                     Sections must be page-aligned (use ALIGN(0x1000) in linker script).",
                    seg_count, load_vaddr, load_end,
                    j, existing_start, existing_end,
                );
                return Err(LoadError::MapFailed);
            }
        }

        // Store segment info
        seg_vaddrs[seg_count] = phdr.vaddr;
        seg_load_vaddrs[seg_count] = load_vaddr;
        seg_load_ends[seg_count] = load_end;
        seg_pages_counts[seg_count] = pages;
        seg_flags[seg_count] = map_flags;
        seg_off[seg_count] = phdr.offset;
        seg_filesz[seg_count] = filesz;
        seg_memsz[seg_count] = memsz;
        seg_count += 1;
    }

    let mut symtab_phys = 0u64;
    let mut symtab_size = 0u64;
    let mut strtab_phys = 0u64;
    let mut strtab_idx = 0usize;

    // Second pass: allocate & map each segment
    for i in 0..seg_count {
        let load_vaddr = seg_load_vaddrs[i];
        let pages = seg_pages_counts[i];
        let map_flags = seg_flags[i];

        let phys = match mm::phys::alloc_pages(pages) {
            Some(p) => p,
            None => {
                for j in 0..i {
                    if seg_phys[j] != 0 && seg_pages[j] > 0 {
                        mm::phys::free_pages(seg_phys[j], seg_pages[j]);
                    }
                }
                return Err(LoadError::NoMem);
            }
        };
        seg_phys[i] = phys;
        seg_pages[i] = pages;

        log::info!(
            "exec: segment vaddr={:#x} pages={} map_flags={:#x} phys={:#x}",
            load_vaddr, pages, map_flags, phys
        );

        unsafe {
            mm::virt::map_contiguous(pml4, load_vaddr, phys, pages, map_flags);
        }

        // Copy segment data from binary to physical memory via higher-half.
        let virt_base = mm::virt::HIGHER_HALF + phys;
        let copy_start = virt_base + (seg_vaddrs[i] - load_vaddr);
        let copy_len = seg_filesz[i] as usize;
        if copy_len > 0 {
            let src = unsafe { binary.as_ptr().add(seg_off[i] as usize) };
            unsafe {
                core::ptr::copy_nonoverlapping(src, copy_start as *mut u8, copy_len);
            }
        }

        // Zero BSS (memsz - filesz) — guard against underflow.
        if seg_memsz[i] > seg_filesz[i] {
            let bss_off = seg_vaddrs[i] + seg_filesz[i] - load_vaddr;
            let bss_len = (seg_memsz[i] - seg_filesz[i]) as usize;
            unsafe {
                core::ptr::write_bytes((virt_base + bss_off) as *mut u8, 0, bss_len);
            }
        }
    }

    // Find symbol table and string table
    let shdr_base = hdr.shoff as usize;
    let shstrndx = hdr.shstrndx as usize;
    let shstr_off = shdr_base + shstrndx * mem::size_of::<Elf64Shdr>();
    let shstr: Elf64Shdr = unsafe { ptr::read_unaligned(binary.as_ptr().add(shstr_off) as *const Elf64Shdr) };
    let str_data = &binary[shstr.offset as usize..(shstr.offset + shstr.size) as usize];

    for i in 0..hdr.shnum as usize {
        let off = shdr_base + i * mem::size_of::<Elf64Shdr>();
        let shdr: Elf64Shdr = unsafe { ptr::read_unaligned(binary.as_ptr().add(off) as *const Elf64Shdr) };
        let name = &str_data[shdr.name as usize..];
        if name.starts_with(b".symtab") {
            let phys = match mm::phys::alloc_pages((shdr.size + 0xFFF) / 0x1000) {
                Some(p) => p,
                None => return Err(LoadError::NoMem),
            };
            unsafe {
                core::ptr::copy_nonoverlapping(binary.as_ptr().add(shdr.offset as usize), (mm::virt::HIGHER_HALF + phys) as *mut u8, shdr.size as usize);
            }
            symtab_phys = phys;
            symtab_size = shdr.size;
            strtab_idx = shdr.link as usize;
        }
    }
    
    if strtab_idx != 0 {
        let off = shdr_base + strtab_idx * mem::size_of::<Elf64Shdr>();
        let shdr: Elf64Shdr = unsafe { ptr::read_unaligned(binary.as_ptr().add(off) as *const Elf64Shdr) };
        let phys = match mm::phys::alloc_pages((shdr.size + 0xFFF) / 0x1000) {
            Some(p) => p,
            None => return Err(LoadError::NoMem),
        };
        unsafe {
            core::ptr::copy_nonoverlapping(binary.as_ptr().add(shdr.offset as usize), (mm::virt::HIGHER_HALF + phys) as *mut u8, shdr.size as usize);
        }
        strtab_phys = phys;
    }

    if symtab_phys == 0 || strtab_phys == 0 {
        return Err(LoadError::NoSymbolTable);
    }

    // ── Allocate initial stack ──
    let stack_pages = page_align_up(stack_size) / 0x1000;
    if stack_pages == 0 {
        return Err(LoadError::BadHeader);
    }
    let stack_end_virt = 0x0000_7FFF_FFFF_0000u64;
    let stack_virt = stack_end_virt - stack_pages * 0x1000;
    if stack_virt >= mm::virt::HIGHER_HALF || stack_virt + stack_pages * 0x1000 > stack_end_virt {
        return Err(LoadError::MapFailed);
    }
    let stack_phys = match mm::phys::alloc_pages(stack_pages) {
        Some(p) => p,
        None => {
            for j in 0..seg_count {
                if seg_phys[j] != 0 && seg_pages[j] > 0 {
                    mm::phys::free_pages(seg_phys[j], seg_pages[j]);
                }
            }
            return Err(LoadError::NoMem);
        }
    };
    let stack_flags = mm::virt::DATA | mm::virt::USER;
    unsafe {
        mm::virt::map_contiguous(
            pml4,
            stack_virt,
            stack_phys,
            stack_pages,
            stack_flags,
        );
    }

    log::info!(
        "exec: loaded ELF entry={:#x} stack={:#x} ({} pages)",
        hdr.entry,
        stack_virt + stack_pages * 0x1000,
        stack_pages,
    );

    Ok(LoadResult {
        entry: hdr.entry,
        stack_top: stack_virt + stack_pages * 0x1000,
        pml4,
        stack_pages,
        symtab_phys,
        symtab_size,
        strtab_phys,
    })
}
