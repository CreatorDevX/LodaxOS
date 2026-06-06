@echo off
if exist "%~dp0serial_output.txt" del "%~dp0serial_output.txt"
start /B "" "C:\Program Files\qemu\qemu-system-x86_64.exe" -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" -drive file="%~dp0disk.img",format=raw,if=ide -serial file:"%~dp0serial_output.txt" -display none -accel whpx -m 512M -smp 4 -d int,cpu_reset -D "%~dp0qemu_smp.log"
timeout /t 8 /nobreak >nul
taskkill /F /IM qemu-system-x86_64.exe >nul 2>&1
