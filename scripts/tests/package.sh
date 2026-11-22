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

assert_archive() {
    tar -tzf "$repo/goria.tar.gz" > "$root/content"
    for entry in egregoria_build/egregoria_bin egregoria_build/base_mod/data.lua egregoria_build/assets/keep.jpg; do
        grep -Fxq "$entry" "$root/content" || { printf 'Archive is missing %s\n' "$entry" >&2; exit 1; }
    done
    if grep -Eq 'egregoria_build/(stale$|build/|assets/screen[^/]*\.(jpg|png)$)' "$root/content"; then
        printf 'Archive contains stale staging data or screenshots\n' >&2
        exit 1
    fi
}

cd "$repo"
if CARGO_TARGET_DIR="$root/missing" sh package.sh > "$root/missing.log" 2>&1; then
    printf 'Packaging succeeded with a missing binary\n' >&2
    exit 1
fi
test -f egregoria_build/stale
test ! -e goria.tar.gz

mkdir "$root/bin"
cat > "$root/bin/tar" <<'EOF'
#!/bin/sh
set -eu
touch "$READY"
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if [ -f "$RELEASE" ]; then
        exec "$REAL_TAR" "$@"
    fi
    sleep 1
done
printf 'Timed out waiting to resume packaging\n' >&2
exit 1
EOF
chmod +x "$root/bin/tar"
real_tar=$(command -v tar)
first_target="$root/first target"
mkdir -p "$first_target/release"
printf 'first run\n' > "$first_target/release/native_app"
PATH="$root/bin:$PATH" REAL_TAR="$real_tar" READY="$root/ready" RELEASE="$root/release" \
    CARGO_TARGET_DIR="$first_target" sh package.sh > "$root/first.log" 2>&1 &
first_pid=$!
for attempt in 1 2 3 4 5; do
    [ ! -f "$root/ready" ] || break
    sleep 1
done
if [ ! -f "$root/ready" ]; then
    touch "$root/release"
    wait "$first_pid" || cat "$root/first.log" >&2
    printf 'First packaging run did not reach tar\n' >&2
    exit 1
fi
second_status=0
CARGO_TARGET_DIR="$target" sh package.sh > "$root/second.log" 2>&1 || second_status=$?
second_binary=$(tar -xOzf goria.tar.gz egregoria_build/egregoria_bin 2> "$root/second-archive.log") ||
    second_status=1
touch "$root/release"
if ! wait "$first_pid"; then
    cat "$root/first.log" >&2
    printf 'First packaging run failed after the overlapping run\n' >&2
    exit 1
fi
if [ "$second_status" -ne 0 ]; then
    cat "$root/second.log" "$root/second-archive.log" >&2
    exit 1
fi
test "$second_binary" = 'fixture binary'
assert_archive
test "$(tar -xOzf goria.tar.gz egregoria_build/egregoria_bin)" = 'first run'
test -f egregoria_build/stale
test -z "$(find egregoria_build -type d ! -path egregoria_build -print)"

printf 'outside archive\n' > "$outside/keep.tar.gz"
rm goria.tar.gz
ln -s "$outside/keep.tar.gz" goria.tar.gz
CARGO_TARGET_DIR="$target" sh package.sh > /dev/null
assert_archive
test ! -L goria.tar.gz
test "$(cat "$outside/keep.tar.gz")" = 'outside archive'
test -f assets/screen1.jpg

cp goria.tar.gz "$root/published.tar.gz"
if PATH="$root/bin:$PATH" REAL_TAR=false READY="$root/ready" RELEASE="$root/release" \
    CARGO_TARGET_DIR="$first_target" sh package.sh > "$root/failed-tar.log" 2>&1; then
    printf 'Packaging succeeded after tar failed\n' >&2
    exit 1
fi
cmp goria.tar.gz "$root/published.tar.gz"
test -z "$(find egregoria_build -type d ! -path egregoria_build -print)"

mkdir build
touch build/keep
(cd "$outside" && CARGO_TARGET_DIR="$target" sh "$repo/package.sh" > /dev/null)
test -f build/keep
test -f "$outside/egregoria_build/keep"
test ! -e "$outside/goria.tar.gz"

rm -r egregoria_build
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
