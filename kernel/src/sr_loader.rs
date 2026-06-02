use core::ptr;
use lodaxos_system::BootInfo;
use crate::mm;

const PT_LOAD: u32 = 1;
const PF_W: u32 = 2;

#[repr(C)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

/// Load the Secure Runtime ELF into kernel page tables.
///
/// Parses ELF64 headers, allocates physical pages, copies segment data
/// from the staging buffer, zeroes BSS, and maps at the segment's virtual
/// address in the current address space.
pub fn load_sr(boot_info: &BootInfo) {
    if boot_info.sr_image_size == 0 {
        log::info!("SR: no image loaded (sr_image_size=0)");
        return;
    }

    let image_addr = boot_info.sr_image_addr;
    let image_size = boot_info.sr_image_size as usize;
    if image_addr == 0 {
        log::info!("SR: sr_image_addr is 0, skipping");
        return;
    }

    let image_ptr = image_addr as *const u8;
    let image = unsafe { core::slice::from_raw_parts(image_ptr, image_size) };

    // Check ELF magic
    let magic: u32 = u32::from_le_bytes(image[0..4].try_into().unwrap());
    if magic != 0x464c457f {
        log::error!("SR: bad ELF magic {:#x}", magic);
        return;
    }

    let ehdr: &Elf64Ehdr = unsafe { &*(image.as_ptr() as *const Elf64Ehdr) };
    log::info!(
        "SR: ELF64 entry={:#x} phoff={} phnum={} phentsize={}",
        ehdr.e_entry, ehdr.e_phoff, ehdr.e_phnum, ehdr.e_phentsize
    );

    let pml4_phys = mm::virt::pml4_address();

    let phdr_base = image.as_ptr() as u64 + ehdr.e_phoff;
    for i in 0..ehdr.e_phnum {
        let phdr = (phdr_base + (i as u64) * (ehdr.e_phentsize as u64)) as *const Elf64Phdr;
        let phdr: &Elf64Phdr = unsafe { &*phdr };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr;
        let filesz = phdr.p_filesz;
        let memsz = phdr.p_memsz;
        let offset = phdr.p_offset;
        let flags = phdr.p_flags;

        log::info!(
            "SR: PT_LOAD vaddr={:#x} filesz={:#x} memsz={:#x} offset={:#x} flags={}",
            vaddr, filesz, memsz, offset, flags
        );

        if memsz == 0 {
            continue;
        }

        let start_page = vaddr & !0xfff;
        let end_page = (vaddr + memsz + 0xfff) & !0xfff;
        let num_pages = ((end_page - start_page) / 4096) as usize;

        log::info!("SR: allocating {} pages for vaddr range {:#x}..{:#x}", num_pages, start_page, end_page);

        for page_off in 0..num_pages {
            let page_vaddr = start_page + (page_off as u64) * 4096;
            let phys_addr = mm::phys::alloc_page().expect("SR: OOM allocating page");
            mm::virt::map_page_explicit(pml4_phys, page_vaddr, phys_addr, mm::virt::DATA);

            // TLB flush
            unsafe {
                core::arch::asm!("invlpg [{}]", in(reg) page_vaddr);
            }

            // Copy segment data from staging buffer into the page.
            let page_start = page_vaddr as usize;
            let page_end = page_start + 4096;
            let seg_start = vaddr as usize;
            let seg_end = (vaddr + filesz) as usize;
            let mem_end = (vaddr + memsz) as usize;

            // Copy file-backed data
            let copy_start = page_start.max(seg_start);
            let copy_end = page_end.min(seg_end);
            if copy_start < copy_end {
                let src_offset = offset as usize + (copy_start - seg_start);
                let dst = page_vaddr as *mut u8;
                let src = image.as_ptr().wrapping_add(src_offset);
                unsafe {
                    ptr::copy_nonoverlapping(src, dst.add(copy_start - page_start), copy_end - copy_start);
                }
            }

            // Zero BSS region (.bss within this page)
            let zero_start = page_start.max(seg_end);
            let zero_end = page_end.min(mem_end);
            if zero_start < zero_end {
                let dst = page_vaddr as *mut u8;
                unsafe {
                    ptr::write_bytes(dst.add(zero_start - page_start), 0, zero_end - zero_start);
                }
            }

            // If this is a writable page, zero-fill any portion before and after
            // the segment (e.g., alignment padding).
            if (flags & PF_W) != 0 {
                // Before segment
                let pre_start = page_start;
                let pre_end = page_end.min(seg_start);
                if pre_start < pre_end {
                    let dst = page_vaddr as *mut u8;
                    unsafe { ptr::write_bytes(dst, 0, pre_end - pre_start); }
                }
                // After segment + BSS
                let post_start = page_start.max(mem_end);
                let post_end = page_end;
                if post_start < post_end {
                    let dst = page_vaddr as *mut u8;
                    unsafe { ptr::write_bytes(dst.add(post_start - page_start), 0, post_end - post_start); }
                }
            }
        }
    }

    log::info!("SR: loaded successfully (entry={:#x})", ehdr.e_entry);
}
