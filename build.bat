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

echo === Building lodaxos-kernel (pass 1) ===
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"
if errorlevel 1 exit /b 1

echo === Extracting kernel symbols ===
python kernel\gensymtab.py "target\target\debug\lodaxos-kernel" kernel\src\arch\symtab.rs
if errorlevel 1 (
    echo WARNING: symbol extraction failed, using empty symtab
)

echo === Building lodaxos-kernel (pass 2 — with symbols) ===
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"
if errorlevel 1 exit /b 1

echo === Building lodaxos-boot ===
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
if errorlevel 1 exit /b 1

echo === Generating font ===
python genfont.py
if errorlevel 1 exit /b 1

echo === Building drivers ===
REM Build each individual driver ELF
set DRIVER_FLAGS=-Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"

cargo +nightly build -p lodaxos-drivers --bin framebuffer --target drivers/target.json %DRIVER_FLAGS%
if errorlevel 1 exit /b 1

cargo +nightly build -p lodaxos-drivers --bin ahci --target drivers/target.json %DRIVER_FLAGS%
if errorlevel 1 exit /b 1

cargo +nightly build -p lodaxos-drivers --bin ext4 --target drivers/target.json %DRIVER_FLAGS%
if errorlevel 1 exit /b 1

cargo +nightly build -p lodaxos-drivers --bin ide --target drivers/target.json %DRIVER_FLAGS%
if errorlevel 1 exit /b 1

echo === Generating driver symbols ===
python kernel\gensym.py "target\target\debug\framebuffer" "target\target\debug\framebuffer.sym"
python kernel\gensym.py "target\target\debug\ahci" "target\target\debug\ahci.sym"
python kernel\gensym.py "target\target\debug\ext4" "target\target\debug\ext4.sym"
python kernel\gensym.py "target\target\debug\ide" "target\target\debug\ide.sym"

echo === Packaging drivers.elf ===
python drivers\pkg.py drivers.elf ^
    framebuffer:0:target\target\debug\framebuffer ^
    framebuffer.sym:2:target\target\debug\framebuffer.sym ^
    ahci:0:target\target\debug\ahci ^
    ahci.sym:2:target\target\debug\ahci.sym ^
    ext4:1:target\target\debug\ext4 ^
    ext4.sym:2:target\target\debug\ext4.sym ^
    ide:0:target\target\debug\ide ^
    ide.sym:2:target\target\debug\ide.sym
if errorlevel 1 exit /b 1

echo === Copying kernel binary ===
if exist "target\target\debug\lodaxos-kernel" (
    copy /Y "target\target\debug\lodaxos-kernel" "kernel.elf"
    echo Copied kernel.elf
) else (
    echo ERROR: kernel binary not found
    exit /b 1
)

echo === All builds successful ===
