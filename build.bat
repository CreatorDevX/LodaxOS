@echo off
REM Build script for LodaxOS — builds all crates
echo === Building lodaxos-system ===
cargo +nightly build -p lodaxos-system
if errorlevel 1 exit /b 1

echo === Building lodaxos-kernel ===
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zjson-target-spec "-Zbuild-std=core,alloc" "-Zbuild-std-features=compiler-builtins-mem"
if errorlevel 1 exit /b 1

echo === Building lodaxos-boot ===
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
if errorlevel 1 exit /b 1

echo === Building lodaxos-chain ===
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
if errorlevel 1 exit /b 1

echo === Building lodaxos-sr ===
cargo +nightly build -p lodaxos-sr --target sr/target.json -Zjson-target-spec "-Zbuild-std=core" "-Zbuild-std-features=compiler-builtins-mem"
if errorlevel 1 exit /b 1

echo === Copying kernel binary to known location ===
for %%I in (target\target\debug\deps\lodaxos_kernel-*) do (
    if not "%%~xI"==".d" if not "%%~xI"==".o" if not "%%~xI"==".pdb" (
        copy /Y "%%I" "kernel.elf"
        echo Copied %%I to kernel.elf
    )
)

echo === Copying SR binary ===
for %%I in (target\target\debug\deps\lodaxos_sr-*) do (
    if not "%%~xI"==".d" if not "%%~xI"==".o" if not "%%~xI"==".pdb" (
        copy /Y "%%I" "sr.elf"
        echo Copied %%I to sr.elf
    )
)

echo === All builds successful ===
