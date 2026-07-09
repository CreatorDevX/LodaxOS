@echo off
"C:\Program Files\qemu\qemu-system-x86_64.exe" ^
  -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" ^
  -drive file="%~dp0disk.img",format=raw,if=ide ^
  -serial stdio ^
  -serial tcp::4444,server,nowait ^
  -accel tcg -machine q35 -m 128M -smp 4 -vga std ^
  -d int,cpu_reset -D qemu_smp.log 
PAUSE
