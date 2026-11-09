#!/usr/bin/env bash
# Native Linux verification and build, then a Windows GNU build, with only user-owned tooling.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -f "$HOME/.cargo/env" ]]; then source "$HOME/.cargo/env"; fi
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
source "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt,clippy
export RUSTUP_TOOLCHAIN=1.99.1

tools="${WISPX_TOOLS_DIR:-$PWD/artifacts/tools/linux}"
mkdir -p "$tools"
tools="$(cd "$tools" && pwd)"

# CPAL's Linux backend links ALSA. Extract development packages locally when the
# runner has no development headers/pkg-config. This never invokes sudo or dpkg -i.
if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists alsa; then
    packages="$tools/debs"
    sysroot="$tools/sysroot"
    mkdir -p "$packages" "$sysroot"
    (
        cd "$packages"
        apt-get download libasound2-dev pkgconf-bin libpkgconf3
        if apt-cache show libasound2t64 >/dev/null 2>&1; then
            apt-get download libasound2t64
        else
            apt-get download libasound2
        fi
        for package in ./*.deb; do dpkg-deb -x "$package" "$sysroot"; done
    )
    export PATH="$sysroot/usr/bin:$PATH"
    # Empty components search the working directory. Besides loading unintended
    # libraries, GCC can mistake a local specs/ directory for its specs file.
    export LD_LIBRARY_PATH="$sysroot/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export PKG_CONFIG="$sysroot/usr/bin/pkgconf"
    export PKG_CONFIG_PATH="$sysroot/usr/lib/x86_64-linux-gnu/pkgconfig"
    export PKG_CONFIG_SYSROOT_DIR="$sysroot"
    export LIBRARY_PATH="$sysroot/usr/lib/x86_64-linux-gnu${LIBRARY_PATH:+:$LIBRARY_PATH}"
fi

cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# The Linux app links only ALSA; X11, Wayland, fontconfig and xkbcommon load at run time.
cargo build --locked --release
linux_exe="${CARGO_TARGET_DIR:-target}/release/WispX"
"$linux_exe" --version
# Paints every pill state offscreen, so the binary's fonts and renderer run without a display.
"$linux_exe" --pill-tour "${CARGO_TARGET_DIR:-target}/pill-tour"

toolchain="$tools/mingw"
if [[ ! -x "$toolchain/usr/bin/x86_64-w64-mingw32-gcc-posix" ]]; then
    mkdir -p "$toolchain/debs"
    (
        cd "$toolchain/debs"
        apt-get download gcc-mingw-w64-x86-64-posix gcc-mingw-w64-x86-64-posix-runtime \
            gcc-mingw-w64-base binutils-mingw-w64-x86-64 mingw-w64-x86-64-dev mingw-w64-common
        for package in ./*.deb; do dpkg-deb -x "$package" "$toolchain"; done
    )
    # Debian normally creates this name through system alternatives. Keep it local.
    ln -sf x86_64-w64-mingw32-gcc-posix "$toolchain/usr/bin/x86_64-w64-mingw32-gcc"
fi
export PATH="$toolchain/usr/bin:$PATH"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$toolchain/usr/bin/x86_64-w64-mingw32-gcc"
export CC_x86_64_pc_windows_gnu="$toolchain/usr/bin/x86_64-w64-mingw32-gcc"
export AR_x86_64_pc_windows_gnu="$toolchain/usr/bin/x86_64-w64-mingw32-ar"
export RC_x86_64_pc_windows_gnu="$toolchain/usr/bin/x86_64-w64-mingw32-windres"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS='-C target-feature=+crt-static'
# Linux-only sysroot settings must not leak into the Windows build.
unset PKG_CONFIG_SYSROOT_DIR PKG_CONFIG_PATH PKG_CONFIG LIBRARY_PATH
rustup target add x86_64-pc-windows-gnu
cargo build --locked --release --target x86_64-pc-windows-gnu
