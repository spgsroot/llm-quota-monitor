[CmdletBinding()]
param(
    [string]$TargetDir = 'C:\t',
    [switch]$Release
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$export = Join-Path $HOME 'export-esp.ps1'
if (-not (Test-Path -LiteralPath $export)) {
    throw 'Missing export-esp.ps1. Run espup install first.'
}
. $export
if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    $cmakeBin = Join-Path $env:ProgramFiles 'CMake\bin'
    if (Test-Path -LiteralPath (Join-Path $cmakeBin 'cmake.exe')) {
        $env:PATH = "$cmakeBin;$env:PATH"
    } else {
        throw 'CMake not found. Install Kitware.CMake and reopen PowerShell.'
    }
}

$oldTarget = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = $TargetDir
Push-Location $root
try {
    $cargoArgs = @('+esp', 'build', '-p', 'quota-firmware', '--target', 'xtensa-esp32-espidf', '-Z', 'build-std=std,panic_abort', '--locked')
    if ($Release) { $cargoArgs += '--release' }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "Firmware build failed (exit $LASTEXITCODE)." }
    $profile = if ($Release) { 'release' } else { 'debug' }
    Write-Host "ELF: $TargetDir\xtensa-esp32-espidf\$profile\llm-quota-monitor"
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $oldTarget
}
