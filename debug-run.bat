@echo off
REM Debug run — starts QEMU with GDB stub on port 1234 (frozen at boot).
REM Connect with:
REM   gdb -ex "target remote localhost:1234" -ex "symbol-file kernel.elf" -ex "continue"
REM
REM GDB must support x86-64 ELF.  Use a cross-gdb (e.g., x86_64-elf-gdb) or
REM WSL gdb-multiarch.  The kernel.elf contains full DWARF debug info.
"C:\Program Files\qemu\qemu-system-x86_64.exe" ^
    -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" ^
    -drive file="%~dp0disk.img",format=raw,if=ide ^
    -serial stdio ^
    -accel tcg ^
    -machine q35 ^
    -m 128M ^
    -smp 1 ^
    -vga std ^
    -d int,cpu_reset ^
    -D qemu_smp.log ^
    -s -S
PAUSE
