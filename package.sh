#!/bin/sh
set -eu

CDPATH= cd -P -- "$(dirname -- "$0")"
if [ ! -d ./assets ] || [ ! -d ./base_mod ]; then
    printf 'Cannot find the packaging sources in %s\n' "$PWD" >&2
    exit 1
fi

binary="${CARGO_TARGET_DIR:-target}/release/native_app"
if [ ! -f "$binary" ]; then
    printf 'Missing release binary: %s\n' "$binary" >&2
    exit 1
fi

build=./egregoria_build
if [ -L "$build" ] || { [ -e "$build" ] && [ ! -d "$build" ]; }; then
    printf 'Expected a staging directory at %s/%s\n' "$PWD" "${build#./}" >&2
    exit 1
fi

mkdir -p "$build"
run_dir=$(mktemp -d "$build/package.XXXXXXXX")
trap 'rm -rf "$run_dir"' EXIT
trap 'exit 1' HUP INT TERM
staging="$run_dir/egregoria_build"
mkdir -p "$staging/assets" "$staging/base_mod"
cp -R assets/. "$staging/assets"
cp -R base_mod/. "$staging/base_mod"
rm -f "$staging"/assets/screen*.jpg "$staging"/assets/screen*.png
cp "$binary" "$staging/egregoria_bin"
tar -czvf "$run_dir/goria.tar.gz" -C "$run_dir" egregoria_build
mv -f "$run_dir/goria.tar.gz" .
