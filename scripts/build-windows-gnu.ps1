# Uses a complete GCC toolchain: Rust's GNU standard library needs libgcc/libgcc_eh.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
# GCC 13 / MinGW 11 matches Ubuntu's cross toolchain and avoids WinLibs GCC 16's
# extra default-manifest.o, which conflicts with application-owned manifests.
$tools = Join-Path (Get-Location) 'artifacts/tools/winlibs-gcc13'
$bin = Join-Path $tools 'mingw64/bin'
if (-not (Test-Path -LiteralPath (Join-Path $bin 'x86_64-w64-mingw32-gcc.exe'))) {
    New-Item -ItemType Directory -Force -Path $tools | Out-Null
    $archive = Join-Path $tools 'winlibs.zip'
    $release = '13.2.0posix-18.1.5-11.0.1-ucrt-r8'
    $package = 'winlibs-x86_64-posix-seh-gcc-13.2.0-mingw-w64ucrt-11.0.1-r8.zip'
    curl.exe -fL --retry 3 "https://github.com/brechtsanders/winlibs_mingw/releases/download/$release/$package" -o $archive
    if ($LASTEXITCODE -ne 0) { throw 'Download MinGW GCC failed' }
    tar.exe -xf $archive -C $tools
    if ($LASTEXITCODE -ne 0) { throw 'Extract MinGW GCC failed' }
}
$env:PATH = "$bin;$env:PATH"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $bin 'x86_64-w64-mingw32-gcc.exe'
$env:CC_x86_64_pc_windows_gnu = Join-Path $bin 'x86_64-w64-mingw32-gcc.exe'
$env:AR_x86_64_pc_windows_gnu = Join-Path $bin 'ar.exe'
$env:RC_x86_64_pc_windows_gnu = Join-Path $bin 'windres.exe'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = '-C target-feature=+crt-static'
rustup target add x86_64-pc-windows-gnu
if ($LASTEXITCODE -ne 0) { throw 'Install Rust GNU target failed' }
cargo build --locked --release --target x86_64-pc-windows-gnu
if ($LASTEXITCODE -ne 0) { throw 'Windows GNU build failed' }
