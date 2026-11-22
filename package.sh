#!/bin/sh
set -eu

binary="${CARGO_TARGET_DIR:-target}/release/native_app"
if [ ! -f "$binary" ]; then
    printf 'Missing release binary: %s\n' "$binary" >&2
    exit 1
fi

mkdir build
cp -r assets base_mod build
rm -f build/screen*.png
rm -f build/crate_architecture.png
cp "$binary" build/egregoria_bin
mv build egregoria_build
tar -czvf goria.tar.gz egregoria_build
rm -rf egregoria_build
