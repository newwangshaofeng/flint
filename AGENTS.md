.rules

# Windows Build & Compilation Guidelines

## 1. Environment & Toolchain Locations
- **Rust Toolchain**: Rust 1.95.0 (`channel = "1.95.0"`, targets `x86_64-pc-windows-msvc`, `wasm32-wasip2`, `wasm32-unknown-unknown`).
  - `CARGO_HOME=C:\cargo`, `RUSTUP_HOME=C:\rustup`, `Path` contains `C:\cargo\bin`.
- **Native Tools**:
  - CMake: `D:\tools\cmake-3.31.6-windows-x86_64\bin`
  - NASM (assembler for `aws-lc-sys`): `D:\tools\nasm-2.16.03`
  - Sccache (compiler cache): `D:\tools\sccache\sccache-v0.10.0-x86_64-pc-windows-msvc\sccache.exe` (`RUSTC_WRAPPER` set to sccache, cache dir `D:\.sccache`).
  - Git Perl: `C:\Program Files\Git\usr\bin\perl.exe`.
- **MSVC & Windows SDK**:
  - Visual Studio 2026 Developer environment + Windows 11 SDK `10.0.26100.0`.
  - **Important Trap**: In `Launch-VsDevShell.ps1`, **must** pass `-Preview` (`Launch-VsDevShell.ps1 -Arch amd64 -HostArch amd64 -Preview -SkipAutomaticLocation`) to activate MSVC toolset `14.52.36418` which contains `lib\spectre\x64` libraries. Without `-Preview`, the default toolset lacks Spectre libraries and the build fails on `msvc_spectre_libs`.

## 2. Compilation Commands & Scripts
- **Fast Build (Recommended for development)**:
  - Run `.\script\build-windows.ps1` (uses `--profile release-fast`).
  - Preserves full runtime optimizations while skipping Thin-LTO and multi-gigabyte PDB generation, reducing link time from ~70+ minutes to ~1-2 minutes. Output is at `target/release-fast/flint.exe`.
- **Full Release Build**:
  - Run `.\script\build-windows.ps1 -FullRelease` (uses `--release`). Output is at `target/release/flint.exe`.
- **Clean Build**:
  - Run `.\script\build-windows.ps1 -Clean`.

## 3. Performance & Speed Optimization Rules
- **Use sccache**: Ensure `$env:RUSTC_WRAPPER` points to `sccache.exe` so unchanged C/C++ dependencies (`libsqlite3-sys`, `aws-lc-sys`, `zstd-sys`, `tree-sitter-*`) and Rust crates are cached.
- **Windows Defender Exclusions**: Exclude `D:\workspace\flint\flint\target`, `C:\cargo`, and `D:\.sccache` from real-time antivirus scanning to prevent severe file I/O degradation during compilation.
- **JSON Search Performance**: Never run full-text grep on giant single-line Visual Studio installer JSON files (`catalog.json`, `state.packages.json` ~18MB). Use PowerShell `ConvertFrom-Json` to parse and query properties in seconds.