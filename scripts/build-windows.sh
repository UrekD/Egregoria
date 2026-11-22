#!/bin/sh
# Cross-compile a Windows build of Egregoria from Linux and package it as a zip.
#
# Prerequisites:
#   rustup target add x86_64-pc-windows-gnu
#   mingw-w64 with the posix thread model (gcc, g++, binutils): either on PATH
#   (Debian/Ubuntu: g++-mingw-w64-x86-64-posix) or unpacked under MINGW_PREFIX.
#   Git LFS assets checked out (git lfs pull).
#
# Usage: scripts/build-windows.sh [output-dir] [save-dir-to-bundle]
# Environment: MINGW_PREFIX (default: toolchain on PATH, else ~/.local/egr-mingw/usr),
#              CARGO_TARGET_DIR (default: target).
set -eu

OUT=${1:-build}
SLOTS=${2:-}
TARGET=x86_64-pc-windows-gnu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [ -z "${MINGW_PREFIX:-}" ]; then
	if command -v x86_64-w64-mingw32-gcc-posix >/dev/null 2>&1; then
		MINGW_PREFIX=$(dirname "$(dirname "$(command -v x86_64-w64-mingw32-gcc-posix)")")
	else
		MINGW_PREFIX=$HOME/.local/egr-mingw/usr
	fi
fi
CC_BIN=$MINGW_PREFIX/bin/x86_64-w64-mingw32-gcc-posix
if [ ! -x "$CC_BIN" ]; then
	echo "mingw-w64 posix gcc not found at $CC_BIN (set MINGW_PREFIX)" >&2
	exit 1
fi
if ! rustup target list --installed | grep -qx "$TARGET"; then
	echo "missing Rust target: rustup target add $TARGET" >&2
	exit 1
fi
# LFS pointer files are tiny text files starting with this line.
if head -c 40 assets/sprites/water_pump.png | grep -q 'git-lfs'; then
	echo "assets are Git LFS pointers; run: git lfs pull" >&2
	exit 1
fi

export PATH="$MINGW_PREFIX/bin:$PATH"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$CC_BIN"
export CC_x86_64_pc_windows_gnu="$CC_BIN"
export CXX_x86_64_pc_windows_gnu="$MINGW_PREFIX/bin/x86_64-w64-mingw32-g++-posix"
export AR_x86_64_pc_windows_gnu="$MINGW_PREFIX/bin/x86_64-w64-mingw32-ar"
TARGET_DIR=${CARGO_TARGET_DIR:-target}

cargo build --locked --release -p native_app --target "$TARGET"

VERSION=$(cat VERSION)
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/egr-windows.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
PKG=$STAGE/egregoria
mkdir -p "$PKG"
cp "$TARGET_DIR/$TARGET/release/native_app.exe" "$PKG/egregoria.exe"
# Runtime DLLs of the posix mingw toolchain; everything else ships with Windows.
for dll in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
	path=$("$CC_BIN" -print-file-name="$dll")
	if [ ! -f "$path" ]; then
		echo "cannot find $dll in the mingw toolchain" >&2
		exit 1
	fi
	cp "$path" "$PKG/"
done
cp -r assets base_mod README.md LICENSE "$PKG/"
if [ -n "$SLOTS" ]; then
	mkdir -p "$PKG/world"
	cp -r "$SLOTS"/. "$PKG/world/"
fi

mkdir -p "$OUT"
ZIP=$(cd "$OUT" && pwd)/egregoria-windows-$VERSION.zip
(cd "$STAGE" && zip -qr "$ZIP.tmp" egregoria)
mv "$ZIP.tmp" "$ZIP"
echo "built $ZIP"
