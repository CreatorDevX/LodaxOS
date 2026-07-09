; SIPI trampoline — real-mode → 64-bit long mode AP startup.
; Parallel-boot version: each AP looks up its mailbox slot via a
; pre-populated mapping table written by the BSP.  All mailboxes are
; written before any IPIs are sent.
;
; Assembled with NASM to a flat binary, placed at TRAMP_BASE (must match
; `consts::TRAMPOLINE_PHYS` in kernel).
; Slot map table at TRAMP_BASE + 0x300 (256 bytes, one u8 per APIC ID).
; Mailbox slots start at TRAMP_BASE + 0x400, each 0x80 bytes.
; After the trampoline finishes it writes status=1 to its slot and
; jumps to ap_entry.
;
; SIPI sets CS.base = SIPI_VECTOR * 0x1000 = TRAMP_BASE.

; Constants (must match consts.rs and smp.rs):
TRAMP_BASE       equ 0x8000
SLOT_MAP_OFF     equ 0x300
SLOT_MAP         equ TRAMP_BASE + SLOT_MAP_OFF
MAILBOX_OFF      equ 0x400
MAILBOX_BASE     equ TRAMP_BASE + MAILBOX_OFF
MAILBOX_SLOT_SIZE equ 0x80
MAX_CPUS_MASK    equ 3      ; (MAX_CPUS - 1) where MAX_CPUS = 4

; Mailbox field offsets (must match smp.rs):
MB_STACK     equ 0x40   ; u64
MB_GDT_LIMIT equ 0x48   ; u16 (2 bytes)
;                       ; 6 bytes padding to align MB_GDT_BASE
MB_GDT_BASE  equ 0x50   ; u64
MB_IDT_LIMIT equ 0x58   ; u16 (2 bytes)
;                       ; 6 bytes padding to align MB_IDT_BASE
MB_IDT_BASE  equ 0x60   ; u64
MB_ENTRY     equ 0x68   ; u64
MB_STATUS    equ 0x70   ; u8
;                       ; 7 bytes padding to align MB_PML4
MB_PML4      equ 0x78   ; u64

[ORG TRAMP_BASE]
[BITS 16]

sipi_entry:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, TRAMP_BASE - 16          ; temp stack below trampoline

    lgdt [gdtr32]                    ; load minimal GDT for protected mode

    mov eax, cr0
    and eax, 0x9FFFFFFF              ; Clear CD (bit 30) and NW (bit 29) to enable caching
    or eax, 0x00010001               ; Enable PE (bit 0) and WP (bit 16)
    mov cr0, eax

    jmp dword 0x08:pm_entry          ; far jump to 32-bit protected mode

[BITS 32]
pm_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    ; Get APIC ID → mailbox slot offset (via pre-populated mapping table).
    ; The BSP writes one byte per APIC ID at SLOT_MAP before sending IPIs.
    ; Falls back to (apic_id & MAX_CPUS_MASK) if the entry is 0xFF.
    mov eax, 1
    cpuid
    shr ebx, 24                      ; EBX[31:24] = initial APIC ID
    xor ecx, ecx
    mov cl, [SLOT_MAP + ebx]         ; cl = slot_map[apic_id]
    cmp cl, 0xFF
    jne .got_slot
    and ebx, MAX_CPUS_MASK           ; fallback: apic_id & (MAX_CPUS - 1)
    mov cl, bl
.got_slot:
    ; ecx = per-CPU slot number (0-3).
    ; Give each AP its own temp stack so concurrent SIPI trampolines
    ; don't corrupt each other's lgdt/lidt data on the shared stack.
    ; Stack layout: TRAMP_BASE - 16 - (slot * 256), 256 bytes each.
    mov eax, ecx
    shl eax, 8                       ; eax = slot * 256
    mov esp, TRAMP_BASE - 16
    sub esp, eax                     ; per-AP temp stack

    imul ecx, ecx, MAILBOX_SLOT_SIZE ; slot_offset = slot * 0x80
    mov esi, ecx                     ; ESI = slot_offset

    ; Enable PAE
    mov eax, cr4
    or eax, 0x20                     ; CR4.PAE = 1
    mov cr4, eax

    ; EFER: set LME (long mode enable), SCE, and NXE
    mov ecx, 0xC0000080              ; IA32_EFER MSR
    rdmsr
    or eax, 0x901                    ; LME=1, SCE=1, NXE=1 (enable syscall and no-execute protection)
    wrmsr

    ; Load kernel PML4 from mailbox slot
    mov eax, [MAILBOX_BASE + esi + MB_PML4]
    mov cr3, eax

    ; Enable paging → transition to IA-32e compatibility mode
    mov eax, cr0
    or eax, 0x80000000               ; PG=1
    mov cr0, eax

    ; Far jump to 64-bit long mode via the temp GDT's 64-bit code
    ; segment (selector 0x18).
    jmp dword 0x18:lm_entry

[BITS 64]
lm_entry:
    ; Load kernel GDT and IDT from mailbox slot.
    sub rsp, 16

    mov ax, [MAILBOX_BASE + esi + MB_GDT_LIMIT]
    mov [rsp], ax
    mov rax, [MAILBOX_BASE + esi + MB_GDT_BASE]
    mov [rsp + 2], rax
    lgdt [rsp]

    mov ax, [MAILBOX_BASE + esi + MB_IDT_LIMIT]
    mov [rsp], ax
    mov rax, [MAILBOX_BASE + esi + MB_IDT_BASE]
    mov [rsp + 2], rax
    lidt [rsp]

    add rsp, 16

    ; Set up per-AP kernel stack
    mov rsp, [MAILBOX_BASE + esi + MB_STACK]

    ; Reload CS via far return to switch from the temp GDT's 64-bit
    ; code selector (0x18) to the kernel's 64-bit code selector (0x08).
    push 0x08
    lea rax, [rel .done]
    push rax
    retfq

.done:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    ; Signal BSP: AP is ready
    mov byte [MAILBOX_BASE + esi + MB_STATUS], 1

    ; Jump to kernel AP entry
    mov rax, [MAILBOX_BASE + esi + MB_ENTRY]
    jmp rax

; ---- Minimal GDT for protected/long-mode entry ----
ALIGN 4
gdtr32:
    dw (gdt32_end - gdt32 - 1)       ; limit
    dd gdt32                          ; base (physical, ORG=TRAMP_BASE)

ALIGN 4
gdt32:
    dq 0x0000000000000000            ; null descriptor            (0x00)
    dq 0x00CF9A000000FFFF            ; 32-bit code, base=0        (0x08)
    dq 0x00CF92000000FFFF            ; 32-bit data, base=0        (0x10)
    dq 0x00AF9A000000FFFF            ; 64-bit code, L=1, base=0   (0x18)
gdt32_end:
