# Flint Windows Build Script
param (
    [switch]$FullRelease,  # Full release build with Thin-LTO
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'

$env:CARGO_HOME = 'C:\cargo'
$env:RUSTUP_HOME = 'C:\rustup'
$env:Path = "C:\cargo\bin;D:\tools\cmake-3.31.6-windows-x86_64\bin;D:\tools\nasm-2.16.03;D:\tools\sccache\sccache-v0.10.0-x86_64-pc-windows-msvc;$env:Path"

# Enable sccache compiler cache
$env:RUSTC_WRAPPER = "D:\tools\sccache\sccache-v0.10.0-x86_64-pc-windows-msvc\sccache.exe"
$env:SCCACHE_DIR = "D:\.sccache"

# Initialize Visual Studio Developer environment (with Spectre-mitigated libs)
& "C:\Program Files\Microsoft Visual Studio\18\Professional\Common7\Tools\Launch-VsDevShell.ps1" -Arch amd64 -HostArch amd64 -Preview -SkipAutomaticLocation

$env:RELEASE_CHANNEL = 'stable'
$env:ZED_RELEASE_CHANNEL = 'stable'
$env:ZED_RC_TOOLKIT_PATH = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64'

Set-Location (Split-Path $PSScriptRoot -Parent)

if ($Clean) {
    Write-Host "Cleaning target directory..." -ForegroundColor Yellow
    cargo clean
}

# Cargo writes progress to stderr, which `$ErrorActionPreference = 'Stop'` would
# otherwise treat as a terminating error. Relax it for the build and check
# `$LASTEXITCODE` explicitly instead.
$ErrorActionPreference = 'Continue'

if ($FullRelease) {
    Write-Host "===> Building full Release (with Thin-LTO)..." -ForegroundColor Cyan
    cargo build --release -p flint
} else {
    Write-Host "===> Building Release-Fast (no LTO, fast linking)..." -ForegroundColor Green
    cargo build --profile release-fast -p flint
}

$buildExitCode = $LASTEXITCODE

if ($buildExitCode -eq 0) {
    Write-Host "`n[SUCCESS] Build completed successfully!" -ForegroundColor Green
    if ($FullRelease) {
        Write-Host "Output binary: target\release\flint.exe"
    } else {
        Write-Host "Output binary: target\release-fast\flint.exe"
    }
} else {
    Write-Host "`n[FAILED] Build failed with exit code: $buildExitCode" -ForegroundColor Red
    exit $buildExitCode
}
