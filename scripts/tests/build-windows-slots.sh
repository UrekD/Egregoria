#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
cd "$root"
mkdir -p bin assets base_mod scripts/osm world
for crate in scenarios simulation common geom prototypes egui-inspect egui-inspect-derive; do
    mkdir -p "$crate/src"
    touch "$crate/Cargo.toml"
done
cp "$repo/Makefile" .
cp "$repo/scripts/osm/ATTRIBUTION-OSM.txt" "$repo"/scripts/osm/*.overpassql scripts/osm/
if [[ -f "$repo/scripts/build-windows-with-slots.sh" ]]; then
    cp "$repo/scripts/build-windows-with-slots.sh" scripts/
fi
touch Cargo.toml Cargo.lock assets/paris_54000.txt base_mod/data.lua
printf '1.0.0\n' > VERSION
printf 'user save\n' > world/user.zip
printf 'Lesce input\n' > lesce.json
printf 'Sava input\n' > sava.json

export TRACE="$root/trace"
export PATH="$root/bin:$PATH"
export EGR_OSM_CACHE="$root/osm cache"
export EGR_CITYMAP_LESCE="$root/lesce.json"
export EGR_CITYMAP_SAVA="$root/sava.json"
export CARGO_TARGET_DIR="$root/cargo target"
unset EGR_OSM_INPUT EGR_OSM_FLAGS

cat > scripts/osm/fetch.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'fetch\n' >> "$TRACE"
mkdir -p -- "$(dirname "$1")"
printf 'cached OSM input\n' > "$1"
EOF

cat > bin/cargo <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == 'run --locked --release -p scenarios -- '* ]]
shift 6
printf '%s\n' "$1" >> "$TRACE"
case "$1" in
    demo-cities)
        [[ $2 == --save-root ]]
        out=$3
        slots=(starter_town showcase_city)
        ;;
    osm-city)
        [[ $2 == --input && -s $3 && $4 == --save-root ]]
        [[ ${TEST_OSM_RESULT:-} != fail ]] || exit 17
        [[ ${TEST_OSM_RESULT:-} != omit ]] || exit 0
        out=$5
        slots=(lesce_osm)
        ;;
    citymap-build)
        [[ $2 == --out && ! -e $3 && $4 == --input && $5 == "$EGR_CITYMAP_LESCE" ]]
        [[ $6 == --sava-input && $7 == "$EGR_CITYMAP_SAVA" ]]
        out=$3
        slots=(lesce_town)
        ;;
    *) exit 2 ;;
esac
mkdir -p -- "$out"
for slot in "${slots[@]}"; do
    [[ ! -e $out/$slot.zip ]]
    cat VERSION > "$out/$slot.zip"
    printf 'metadata\n' > "$out/$slot.meta.json"
done
EOF

cat > scripts/build-windows.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'package\n' >> "$TRACE"
mkdir -p -- "$1"
find "$2" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort > "$1/members"
EOF
chmod +x bin/cargo scripts/osm/fetch.sh scripts/build-windows.sh

build() {
    make --no-print-directory build WINDOWS_OUT="$root/output dir" "$@"
}

expect_trace() {
    diff -u <(printf '%s\n' "$@") "$TRACE"
    : > "$TRACE"
}

expect_members() {
    diff -u <(for slot in "$@"; do printf '%s.meta.json\n%s.zip\n' "$slot" "$slot"; done) \
        "$root/output dir/members"
    [[ $(cat world/user.zip) == 'user save' ]]
    [[ $(find world -type f | wc -l) == 1 ]]
}

build
expect_trace fetch demo-cities osm-city citymap-build package
expect_members lesce_osm lesce_town showcase_city starter_town

build
expect_trace package

printf '1.0.1\n' > VERSION
build
expect_trace demo-cities osm-city citymap-build package

printf 'edited generator\n' > scenarios/src/generator.rs
build
expect_trace demo-cities osm-city citymap-build package

rm -- "$CARGO_TARGET_DIR/windows-slots/current/slots/starter_town.zip"
build
expect_trace demo-cities osm-city citymap-build package

for result in fail omit; do
    printf '%s\n' "$result" >> "$EGR_OSM_CACHE/lesce-20261122-signals.overpass.json"
    if TEST_OSM_RESULT=$result build > "$root/$result.log" 2>&1; then
        printf 'Build accepted an unsuccessful OSM generation (%s)\n' "$result" >&2
        exit 1
    fi
    grep -E 'osm-city|lesce_osm' "$root/$result.log"
    if [[ $result == omit ]]; then
        expect_trace demo-cities osm-city citymap-build
    else
        expect_trace demo-cities osm-city
    fi
    expect_members lesce_osm lesce_town showcase_city starter_town
done

build EGR_OSM_INPUT="$root/lesce.json" BUILD_CITYMAP=
expect_trace demo-cities osm-city package
expect_members lesce_osm showcase_city starter_town
