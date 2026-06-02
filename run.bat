@echo off
"C:\Program Files\qemu\qemu-system-x86_64.exe" -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" -drive file="%~dp0disk.img",format=raw,if=ide -serial stdio -accel whpx -m 512M -smp 2
PAUSE
