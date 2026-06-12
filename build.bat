@echo off
REM Build script for LodaxOS — builds all crates
echo === Building lodaxos-system ===
cargo +nightly build -p lodaxos-system
if errorlevel 1 exit /b 1

echo === Assembling SIPI trampoline (NASM) ===
"%~dp0nasm\nasm.exe" -f bin -o "kernel\src\arch\smp_trampoline.bin" "kernel\src\arch\smp_trampoline.S"
if errorlevel 1 (
    echo NASM assembly failed
    exit /b 1
)
echo NASM assembled smp_trampoline.bin

echo === Building lodaxos-kernel ===
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"
if errorlevel 1 exit /b 1

echo === Building lodaxos-boot ===
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
if errorlevel 1 exit /b 1

echo === Building lodaxos-chain ===
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
if errorlevel 1 exit /b 1

echo === Copying kernel binary to known location ===
if exist "target\target\debug\lodaxos-kernel" (
    copy /Y "target\target\debug\lodaxos-kernel" "kernel.elf"
    echo Copied target\target\debug\lodaxos-kernel to kernel.elf
) else (
    echo ERROR: kernel binary not found
    exit /b 1
)

echo === All builds successful ===
