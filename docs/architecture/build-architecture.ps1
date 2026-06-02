param(
    [string]$OutputPath = ""
)

if ($OutputPath -eq "") {
    $OutputPath = Join-Path (Split-Path $PSScriptRoot -Parent) "ARCHITECTURE.md"
}

$sourceDir = $PSScriptRoot

$files = @(
    "00-overview.md",
    "01-crate-structure.md",
    "02-memory-model.md",
    "03-interrupt-model.md",
    "04-task-scheduling.md",
    "05-elf-boot-protocol.md",
    "06-acpi-platform.md",
    "07-build-disk-system.md",
    "08-fault-recovery.md",
    "09-subsystem-interfaces.md",
    "10-future-architecture.md"
)

$output = New-Object System.Text.StringBuilder

[void]$output.AppendLine("# LodaxOS Architecture Reference")
[void]$output.AppendLine("")
[void]$output.AppendLine("This document is automatically assembled from modular files in docs/architecture/.")
[void]$output.AppendLine("To rebuild: .\docs\architecture\build-architecture.ps1")
[void]$output.AppendLine("")
[void]$output.AppendLine("To edit a section, modify the corresponding file in docs/architecture/ and rebuild.")
[void]$output.AppendLine("")

$totalLines = 0

foreach ($file in $files) {
    $path = Join-Path $sourceDir $file
    if (Test-Path $path) {
        $fileContent = Get-Content $path -Raw
        $lineCount = ($fileContent -split "`n").Count
        $totalLines += $lineCount
        [void]$output.AppendLine("")
        [void]$output.AppendLine("---")
        [void]$output.AppendLine("")
        [void]$output.AppendLine($fileContent.TrimEnd())
        Write-Host "Included $file ($lineCount lines)"
    } else {
        Write-Warning "Skipped $file (not found)"
    }
}

[void]$output.AppendLine("")
[void]$output.AppendLine("---")
[void]$output.AppendLine("")
[void]$output.AppendLine("*Generated from " + $files.Count + " module files - " + $totalLines + " total lines*")

$output.ToString() | Set-Content $OutputPath -Encoding UTF8
Write-Host "`nWrote ARCHITECTURE.md to $OutputPath ($totalLines lines)"
