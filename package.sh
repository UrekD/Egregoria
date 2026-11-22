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

staging=./egregoria_build
if [ -L "$staging" ] || { [ -e "$staging" ] && [ ! -d "$staging" ]; }; then
    printf 'Expected a staging directory at %s/%s\n' "$PWD" "${staging#./}" >&2
    exit 1
fi
if [ -d "$staging" ]; then
    rm -rf "$staging"
fi

mkdir "$staging"
trap 'rm -rf "$staging"' EXIT
mkdir "$staging/assets" "$staging/base_mod"
cp -R assets/. "$staging/assets"
cp -R base_mod/. "$staging/base_mod"
rm -f "$staging"/assets/screen*.jpg "$staging"/assets/screen*.png
cp "$binary" "$staging/egregoria_bin"
tar -czvf goria.tar.gz "${staging#./}"
