#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -P -- "$(dirname -- "$0")" && pwd)
packager=${1:-"$script_dir/../../package.sh"}
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

repo="$root/repo"
outside="$root/outside"
target="$root/target with spaces"
mkdir -p "$repo/assets" "$repo/base_mod" "$repo/egregoria_build" \
    "$outside/egregoria_build" "$target/release"
cp "$packager" "$repo/package.sh"
touch "$repo/base_mod/data.lua" "$repo/egregoria_build/stale" \
    "$repo/assets/screen1.jpg" "$repo/assets/screen2.png" "$repo/assets/keep.jpg" \
    "$outside/egregoria_build/keep"
printf 'fixture binary\n' > "$target/release/native_app"

cd "$repo"
if CARGO_TARGET_DIR="$root/missing" sh package.sh > "$root/missing.log" 2>&1; then
    printf 'Packaging succeeded with a missing binary\n' >&2
    exit 1
fi
test -f egregoria_build/stale
test ! -e goria.tar.gz

CARGO_TARGET_DIR="$target" sh package.sh > /dev/null
tar -tzf goria.tar.gz > "$root/content"
for entry in egregoria_build/egregoria_bin egregoria_build/base_mod/data.lua egregoria_build/assets/keep.jpg; do
    grep -Fxq "$entry" "$root/content" || { printf 'Archive is missing %s\n' "$entry" >&2; exit 1; }
done
if grep -Eq 'egregoria_build/(stale$|build/|assets/screen[^/]*\.(jpg|png)$)' "$root/content"; then
    printf 'Archive contains stale staging data or screenshots\n' >&2
    exit 1
fi
test ! -e egregoria_build
test -f assets/screen1.jpg

mkdir build
touch build/keep
(cd "$outside" && CARGO_TARGET_DIR="$target" sh "$repo/package.sh" > /dev/null)
test -f build/keep
test -f "$outside/egregoria_build/keep"
test ! -e "$outside/goria.tar.gz"

ln -s "$outside/egregoria_build" egregoria_build
if CARGO_TARGET_DIR="$target" sh package.sh > "$root/symlink.log" 2>&1; then
    printf 'Packaging accepted a staging symlink\n' >&2
    exit 1
fi
test -L egregoria_build
test -f "$outside/egregoria_build/keep"
test ! -e "$outside/egregoria_build/build"

rm egregoria_build
mv assets "$outside/assets"
ln -s "$outside/assets" assets
CARGO_TARGET_DIR="$target" sh package.sh > /dev/null
test -f "$outside/assets/screen1.jpg"
test -f "$outside/assets/screen2.png"
