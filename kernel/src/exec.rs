//! ELF64 loader for ring-0 processes (e.g. Executive Runtime).
//!
//! This module is the **kernel's mechanism** for spawning a separate
//! ring-0 process. The contract is:
//!
//!   - The caller provides a raw ELF64 image in memory (typically
//!     read from disk by the bootloader into a staging buffer).
//!   - The loader forks a new PML4 from the kernel's current PML4.
//!   - The loader maps the mailbox page (kernel ↔ process shared) into
//!     the new PML4 at a fixed address.
//!   - The loader parses the ELF and maps each `PT_LOAD` segment into
//!     the new PML4 at the segment's requested virtual address.
//!   - The loader allocates a fresh kernel stack and maps it into the
//!     new PML4.
//!   - The loader creates a `Task` with the ELF's `e_entry` as RIP,
//!     `RDI = mailbox_virt_in_process_space`, the new PML4, and the
//!     new kernel stack.
//!
//! The new task is registered in the global task table and added to
//! the CFS runqueue. The next time the scheduler runs, it will pick
//! the new task and switch to it (CR3 + RSP + RIP via the modified
//! TrapFrame).
//!
//! ## No symbol resolution
//!
//! The kernel does **not** look up any symbols in the loaded ELF.
//! Communication is via the shared mailbox page only. The process's
//! `_start(mailbox_virt)` receives the mailbox's virtual address (in
//! the process's own address space) in `RDI`.

use core::ptr;
use lodaxos_system::{BootInfo, MAILBOX_EXRUN_VIRT, MAILBOX_KERNEL_VIRT};
use crate::mm::{phys, virt};
use crate::task;

const PT_LOAD: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Ehdr {
    e_ident:     [u8; 16],
    e_type:      u16,
    e_machine:   u16,
    e_version:   u32,
    e_entry:     u64,
    e_phoff:     u64,
    e_shoff:     u64,
    e_flags:     u32,
    e_ehsize:    u16,
    e_phentsize: u16,
    e_phnum:     u16,
    e_shentsize: u16,
    e_shnum:     u16,
    e_shstrndx:  u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Phdr {
    p_type:   u32,
    p_flags:  u32,
    p_offset: u64,
    p_vaddr:  u64,
    p_paddr:  u64,
    p_filesz: u64,
    p_memsz:  u64,
    p_align:  u64,
}

const KERNEL_STACK_PAGES: u64 = 4; // 16 KiB

#[inline]
unsafe fn read_unaligned<T: Copy>(base: *const u8, offset: usize) -> T {
    ptr::read_unaligned(base.add(offset) as *const T)
}

/// Load an ELF64 image as a separate ring-0 process. The image is
/// `&[u8]` of the raw ELF (PT_LOAD segments only — symbols are
/// ignored). Returns the new task's id, or `None` on failure.
pub fn load(boot_info: &BootInfo) -> Option<usize> {
    use crate::cap;
    use lodaxos_system::{CapOp, Caps};

    if boot_info.exrun_image_size == 0 || boot_info.exrun_image_addr == 0 {
        log::info!("exec: no ExRun image in BootInfo, skipping spawn");
        return None;
    }
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        Caps::CAP_TASK_CREATE,
        CapOp::TaskCreate { parent: Some(cap::current_subject()) },
    ) {
        log::warn!("exec::load: cap denied: {:?}", e);
        return None;
    }

    let image_ptr = boot_info.exrun_image_addr as *const u8;
    let image_size = boot_info.exrun_image_size as usize;
    if image_size < core::mem::size_of::<Elf64Ehdr>() {
        log::error!("exec: ExRun image too small: {} bytes", image_size);
        return None;
    }

    // Check ELF magic.
    let magic: u32 = unsafe { read_unaligned(image_ptr, 0) };
    if magic != 0x464c457f {
        log::error!("exec: bad ELF magic {:#x}", magic);
        return None;
    }

    let ehdr: Elf64Ehdr = unsafe { read_unaligned(image_ptr, 0) };
    if ehdr.e_machine != 0x3E {
        log::error!("exec: not x86-64 (e_machine={})", ehdr.e_machine);
        return None;
    }
    if ehdr.e_phentsize as usize != core::mem::size_of::<Elf64Phdr>() {
        log::error!("exec: bad phentsize {}", ehdr.e_phentsize);
        return None;
    }
    let phdr_bytes = (ehdr.e_phnum as usize).saturating_mul(ehdr.e_phentsize as usize);
    let phdr_end = (ehdr.e_phoff as usize).saturating_add(phdr_bytes);
    if phdr_end > image_size {
        log::error!(
            "exec: program header table out of bounds: off={} bytes={} image={}",
            ehdr.e_phoff,
            phdr_bytes,
            image_size
        );
        return None;
    }
    log::info!(
        "exec: ELF64 entry={:#x} phoff={} phnum={} phentsize={}",
        ehdr.e_entry, ehdr.e_phoff, ehdr.e_phnum, ehdr.e_phentsize
    );

    // 1. Fork a new PML4 from the kernel's current PML4.
    let kernel_pml4 = virt::pml4_address();
    let new_pml4 = virt::fork_pml4(kernel_pml4)?;
    log::info!("exec: forked PML4 — kernel={:#x} → process={:#x}", kernel_pml4, new_pml4);

    // 2. Allocate a mailbox page. Map it into both the kernel's PML4
    //    (at MAILBOX_KERNEL_VIRT) and the new PML4 (at MAILBOX_EXRUN_VIRT).
    //    Both PML4s see the same physical bytes.
    let mailbox_phys = phys::alloc_page().expect("exec: OOM for mailbox");
    virt::map_page_explicit(kernel_pml4, MAILBOX_KERNEL_VIRT, mailbox_phys, virt::DATA);
    virt::map_page_explicit(new_pml4,    MAILBOX_EXRUN_VIRT,  mailbox_phys, virt::DATA);
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) MAILBOX_KERNEL_VIRT) };
    log::info!(
        "exec: mailbox page phys={:#x} mapped at kernel={:#x} process={:#x}",
        mailbox_phys, MAILBOX_KERNEL_VIRT, MAILBOX_EXRUN_VIRT
    );

    // 3. Map each PT_LOAD segment into the new PML4. We do not touch
    //    the kernel's PML4 — the process's code lives only in the
    //    forked PML4. (This is the architectural separation: the
    //    kernel cannot accidentally read the process's code/data.)
    for i in 0..ehdr.e_phnum {
        let phdr_off = ehdr.e_phoff as usize + (i as usize) * (ehdr.e_phentsize as usize);
        let phdr: Elf64Phdr = unsafe { read_unaligned(image_ptr, phdr_off) };
        if phdr.p_type != PT_LOAD {
            continue;
        }
        let vaddr = phdr.p_vaddr;
        let filesz = phdr.p_filesz;
        let memsz = phdr.p_memsz;
        let offset = phdr.p_offset;
        let flags = phdr.p_flags;
        if memsz == 0 {
            continue;
        }
        let file_end = (offset as usize).saturating_add(filesz as usize);
        if file_end > image_size || filesz > memsz {
            log::error!(
                "exec: invalid PT_LOAD bounds offset={:#x} filesz={:#x} memsz={:#x} image={:#x}",
                offset,
                filesz,
                memsz,
                image_size
            );
            return None;
        }
        let pt_flags: u64 = if (flags & PF_W) != 0 {
            virt::DATA
        } else {
            virt::PRESENT // read-only, executable
        };

        log::info!(
            "exec: PT_LOAD vaddr={:#x} filesz={:#x} memsz={:#x} offset={:#x} flags={}",
            vaddr, filesz, memsz, offset, flags
        );

        let start_page = vaddr & !0xfff;
        let end_page = (vaddr + memsz + 0xfff) & !0xfff;
        let num_pages = ((end_page - start_page) / 4096) as usize;
        for page_off in 0..num_pages {
            let page_vaddr = start_page + (page_off as u64) * 4096;
            let phys_addr = phys::alloc_page().expect("exec: OOM for PT_LOAD page");

            // Populate the physical page via the kernel's higher-half map
            // (`HIGHER_HALF + phys`). The kernel's CR3 is currently active,
            // and ExRun's virtual address is NOT mapped in the kernel's
            // PML4 (architectural separation: the kernel cannot reach
            // ExRun's code). Writing through the higher-half alias is
            // the only way to fill the page contents from the kernel.
            let kernel_va = virt::HIGHER_HALF + phys_addr;
            unsafe {
                ptr::write_bytes(kernel_va as *mut u8, 0, 4096);
            }

            // Copy file-backed data.
            let page_start = page_vaddr as usize;
            let page_end = page_start + 4096;
            let seg_start = vaddr as usize;
            let seg_end = (vaddr + filesz) as usize;
            let mem_end = (vaddr + memsz) as usize;
            let copy_start = page_start.max(seg_start);
            let copy_end = page_end.min(seg_end);
            if copy_start < copy_end {
                let src_offset = offset as usize + (copy_start - seg_start);
                let dst_off = (copy_start - page_start) as isize;
                let src = image_ptr.wrapping_add(src_offset);
                unsafe {
                    ptr::copy_nonoverlapping(
                        src,
                        (kernel_va as *mut u8).offset(dst_off),
                        copy_end - copy_start,
                    );
                }
            }
            // Zero BSS (within the segment, beyond filesz).
            let zero_start = page_start.max(seg_end);
            let zero_end = page_end.min(mem_end);
            if zero_start < zero_end {
                let dst_off = (zero_start - page_start) as isize;
                unsafe {
                    ptr::write_bytes(
                        (kernel_va as *mut u8).offset(dst_off),
                        0,
                        zero_end - zero_start,
                    );
                }
            }
            if (flags & PF_W) != 0 {
                // Zero alignment padding (before seg_start / after mem_end)
                // for writable segments so the page is fully initialised.
                let pre_end = page_end.min(seg_start);
                if page_start < pre_end {
                    unsafe { ptr::write_bytes(kernel_va as *mut u8, 0, pre_end - page_start) };
                }
                let post_start = page_start.max(mem_end);
                if post_start < page_end {
                    let dst_off = (post_start - page_start) as isize;
                    unsafe {
                        ptr::write_bytes(
                            (kernel_va as *mut u8).offset(dst_off),
                            0,
                            page_end - post_start,
                        );
                    }
                }
            }

            // Now install the 4KB mapping in the new (ExRun) PML4 only.
            virt::map_page_explicit(new_pml4, page_vaddr, phys_addr, pt_flags);
        }
    }

    // 4. Allocate a kernel stack (in the kernel's physical allocator)
    //    and map it into the new PML4 (only). The stack's physical
    //    address is in the kernel's PML4 already (via the higher-half
    //    map) and now also in the new PML4 (via the deep copy +
    //    re-mapping). When the scheduler switches to this task, it
    //    will set RSP to the top of this stack.
    let stack_phys = phys::alloc_pages(KERNEL_STACK_PAGES)
        .expect("exec: OOM for kernel stack");
    let stack_virt = virt::HIGHER_HALF + stack_phys;
    let stack_top = stack_virt + KERNEL_STACK_PAGES * 4096;
    // Map into the new PML4 (kernel already has this via higher-half).
    for i in 0..KERNEL_STACK_PAGES {
        let v = stack_virt + i * 4096;
        let p = stack_phys + i * 4096;
        virt::map_page_explicit(new_pml4, v, p, virt::DATA);
    }
    log::info!(
        "exec: kernel stack phys={:#x} virt={:#x} top={:#x}",
        stack_phys, stack_virt, stack_top
    );

    // 5. Create the task. The entry is the ELF's e_entry; RDI is the
    //    mailbox's virtual address in the process's address space.
    let arg = MAILBOX_EXRUN_VIRT;
    let task_id = task::create_task_in(ehdr.e_entry, arg, new_pml4)?;
    log::info!(
        "exec: spawned task {} (entry={:#x} arg=RDI={:#x} pml4={:#x})",
        task_id, ehdr.e_entry, arg, new_pml4
    );

    // 6. The task is now in the runqueue. It will be picked up by
    //    the next schedule() call (from the LAPIC timer IRQ).
    Some(task_id)
}
