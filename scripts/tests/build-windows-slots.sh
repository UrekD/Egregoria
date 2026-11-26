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
    citymap-verify)
        [[ $2 == --root ]]
        python3 - "$3" <<'PY_CHECK'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
meta = json.loads((root / "lesce_town.meta.json").read_text())
report = json.loads((root / "lesce_town.build.json").read_text())
if meta["population"] <= 0:
    sys.exit("lesce_town: expected positive population")
if report != {"variant": "wet", "revision": meta["revision"], "save_version": Path("VERSION").read_text().strip()}:
    sys.exit("lesce_town: report and metadata mismatch")
if (root / "lesce_town.zip").read_text() != report["save_version"] + " " + meta["revision"] + "\n":
    sys.exit("lesce_town: corrupt snapshot or header mismatch")
PY_CHECK
        exit
        ;;
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
    if [[ $slot == lesce_town ]]; then
        printf '%s commissioned\n' "$(<VERSION)" > "$out/$slot.zip"
        printf '{"population":%s,"revision":"commissioned"}\n' "${TEST_CITYMAP_POPULATION:-2600}" > "$out/$slot.meta.json"
        printf '{"variant":"wet","revision":"commissioned","save_version":"%s"}\n' "$(<VERSION)" > "$out/$slot.build.json"
    else
        cat VERSION > "$out/$slot.zip"
        printf '{"population":2600}\n' > "$out/$slot.meta.json"
    fi
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
    diff -u <(for slot in "$@"; do
        if [[ $slot == lesce_town ]]; then printf 'lesce_town.build.json\n'; fi
        printf '%s.meta.json\n%s.zip\n' "$slot" "$slot"
    done) \
        "$root/output dir/members"
    [[ $(cat world/user.zip) == 'user save' ]]
    [[ $(find world -type f | wc -l) == 1 ]]
}

build
expect_trace fetch demo-cities osm-city citymap-build citymap-verify package
expect_members lesce_osm lesce_town showcase_city starter_town

build
expect_trace citymap-verify package

meta="$CARGO_TARGET_DIR/windows-slots/current/slots/lesce_town.meta.json"
cp -- "$meta" "$root/good-meta.json"
printf '{"population":0,"revision":"commissioned"}\n' > "$meta"
if build > "$root/cached-unpopulated.log" 2>&1; then
    printf 'Build reused an unpopulated Lesce Town slot\n' >&2
    exit 1
fi
rg 'lesce_town.*population' "$root/cached-unpopulated.log"
expect_trace citymap-verify
cp -- "$root/good-meta.json" "$meta"

town_zip="${meta%.meta.json}.zip"
cp -- "$town_zip" "$root/good-town.zip"
printf 'corrupted snapshot\n' > "$town_zip"
if build > "$root/cached-corrupt.log" 2>&1; then
    printf 'Build reused a corrupt Lesce Town snapshot\n' >&2
    exit 1
fi
expect_trace citymap-verify
printf '%s wrong-revision\n' "$(<VERSION)" > "$town_zip"
if build > "$root/cached-revision.log" 2>&1; then
    printf 'Build reused a snapshot from a different revision\n' >&2
    exit 1
fi
expect_trace citymap-verify
printf '0.9.0 commissioned\n' > "$town_zip"
if build > "$root/cached-version.log" 2>&1; then
    printf 'Build reused a snapshot from a different save version\n' >&2
    exit 1
fi
expect_trace citymap-verify
cp -- "$root/good-town.zip" "$town_zip"
town_report="${meta%.meta.json}.build.json"
cp -- "$town_report" "$root/good-report.json"
printf '{"variant":"dry","revision":"commissioned","save_version":"%s"}\n' "$(<VERSION)" > "$town_report"
if build > "$root/cached-report.log" 2>&1; then
    printf 'Build accepted a dry report for the wet slot\n' >&2
    exit 1
fi
expect_trace citymap-verify
cp -- "$root/good-report.json" "$town_report"

printf '1.0.1\n' > VERSION
build
expect_trace demo-cities osm-city citymap-build citymap-verify package

printf 'edited generator\n' > scenarios/src/generator.rs
build
expect_trace demo-cities osm-city citymap-build citymap-verify package

rm -- "$CARGO_TARGET_DIR/windows-slots/current/slots/starter_town.zip"
build
expect_trace demo-cities osm-city citymap-build citymap-verify package

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

printf 'new Sava input\n' >> sava.json
if TEST_CITYMAP_POPULATION=0 build > "$root/unpopulated.log" 2>&1; then
    printf 'Build accepted an unpopulated Lesce Town slot\n' >&2
    exit 1
fi
rg 'lesce_town.*population' "$root/unpopulated.log"
expect_trace demo-cities osm-city citymap-build citymap-verify
expect_members lesce_osm lesce_town showcase_city starter_town

published="$root/published town"
mkdir -- "$published"
cp -- "${meta%.meta.json}".{meta.json,zip,build.json} "$published/"
EGR_CITYMAP_PUBLISHED="$published" build
expect_trace demo-cities osm-city citymap-verify package
expect_members lesce_osm lesce_town showcase_city starter_town

build EGR_OSM_INPUT="$root/lesce.json" BUILD_CITYMAP=
expect_trace demo-cities osm-city package
expect_members lesce_osm showcase_city starter_town
