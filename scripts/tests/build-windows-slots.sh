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
if [[ $1 == citymap-build && $2 == --verify ]]; then
    printf 'citymap-verify\n' >> "$TRACE"
    set -- citymap-verify "$2" "$3"
elif [[ $1 == bench-slot ]]; then
    printf 'load %s\n' "$5" >> "$TRACE"
else
    printf '%s\n' "$1" >> "$TRACE"
fi
case "$1" in
    citymap-verify)
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
    bench-slot)
        [[ $# == 9 && $2 == --root && $4 == --slot && $6 == --warmup && $7 == 0 && $8 == --ticks && $9 == 1 ]]
        cmp -- VERSION "$3/$5.zip"
        exit
        ;;
    demo-cities)
        [[ $2 == --save-root ]]
        out=$3
        slots=(starter_town showcase_city)
        ;;
    osm-city)
        [[ $2 == --input && -s $3 && $4 == --save-root ]]
        if [[ $3 == */release-slots/* ]]; then
            cmp -- lesce.json "$3"
            [[ $# == 5 ]]
        fi
        [[ ${TEST_OSM_RESULT:-} != fail ]] || exit 17
        [[ ${TEST_OSM_RESULT:-} != omit ]] || exit 0
        out=$5
        slots=(lesce_osm)
        ;;
    citymap-build)
        [[ $2 == --out && ! -e $3 && $4 == --input && $6 == --sava-input ]]
        if [[ $5 == */release-slots/* ]]; then
            cmp -- lesce.json "$5"
            cmp -- sava.json "$7"
        else
            [[ $5 == "$EGR_CITYMAP_LESCE" && $7 == "$EGR_CITYMAP_SAVA" ]]
        fi
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

mkdir -p scripts/osm/release
gzip -n -c lesce.json > scripts/osm/release/lesce.json.gz
gzip -n -c sava.json > scripts/osm/release/sava.json.gz

release_slots() {
    EGR_OSM_INPUT=/missing EGR_OSM_FLAGS=--replace \
        scripts/build-windows-with-slots.sh --release-slots "$1" "$CARGO_TARGET_DIR"
}

release_loads=('load starter_town' 'load showcase_city' 'load lesce_osm' citymap-verify)
release_generation=(demo-cities osm-city citymap-build "${release_loads[@]}")
release_copy=(demo-cities osm-city "${release_loads[@]}")

if ! release_slots "$root/release/world" > "$root/release-slots.log" 2>&1; then
    cat "$root/release-slots.log" >&2
    printf 'Could not stage release slots without running the Windows packager\n' >&2
    exit 1
fi
expect_trace "${release_generation[@]}"
for slot in starter_town showcase_city lesce_osm lesce_town; do
    test -s "$root/release/world/$slot.zip"
    test -s "$root/release/world/$slot.meta.json"
done
test -s "$root/release/world/lesce_town.build.json"

if release_slots world > "$root/existing-world.log" 2>&1; then
    printf 'Release slot generation accepted an existing save directory\n' >&2
    exit 1
fi
test ! -s "$TRACE"
test "$(cat world/user.zip)" = 'user save'

release_slots "$root/second-release/world"
expect_trace "${release_loads[@]}"
diff -r "$root/release/world" "$root/second-release/world"

EGR_CITYMAP_PUBLISHED="$root/release/world" release_slots "$root/third-release/world"
expect_trace "${release_copy[@]}"
diff -r "$root/release/world" "$root/third-release/world"

test_corrupt_release_cache() {
    local slot cache="$CARGO_TARGET_DIR/release-slots/current"
    for slot in starter_town showcase_city lesce_osm lesce_town; do
        printf 'corrupted snapshot\n' > "$cache/slots/$slot.zip"
        release_slots "$root/repaired-$slot/world"
        expect_trace "${release_copy[@]}"
        diff -r "$root/release/world" "$root/repaired-$slot/world"
    done

    # Even with matching checksums, an undecodable snapshot must be regenerated.
    printf 'corrupted snapshot\n' > "$cache/slots/starter_town.zip"
    (cd "$cache/slots" && LC_ALL=C sha256sum -- ./*) > "$cache/checksums"
    release_slots "$root/repaired-after-load/world"
    expect_trace 'load starter_town' "${release_copy[@]}"
    diff -r "$root/release/world" "$root/repaired-after-load/world"
}

test_trailing_slash() {
    release_slots "$root/trailing slash/world///"
    expect_trace "${release_loads[@]}"
    diff -r "$root/release/world" "$root/trailing slash/world"
}

test_publish_tag() (
    # Run the publisher's actual shell body against a local Git remote and a fake release CLI.
    awk '
        /^      - name: Create release with assets$/ { step = 1 }
        step && /^        run: [>|]$/ { run = substr($0, length($0)); next }
        run && /^          / {
            printf "%s%s", substr($0, 11), (run == ">" ? " " : "\n")
            next
        }
        run && /^$/ { print ""; next }
        run { exit }
        END { if (!run) exit 1; print "" }
    ' "$repo/.github/workflows/release.yml" > "$root/publish-release"

    export GITHUB_SERVER_URL="$root" GITHUB_REPOSITORY=tag-remote
    export GITHUB_REF_NAME=v1.0.0 GITHUB_REF=refs/tags/v1.0.0
    export GITHUB_RUN_ID=123 GITHUB_RUN_ATTEMPT=2
    export TEST_REMOTE="$root/tag-remote.git"
    git init -q --bare "$TEST_REMOTE"
    tag_git() {
        git --git-dir="$TEST_REMOTE" -c user.name=Tester -c user.email=test@example.com "$@"
    }
    tree=$(tag_git mktree </dev/null)
    GITHUB_SHA=$(tag_git commit-tree "$tree" -m built)
    TEST_OTHER_SHA=$(tag_git commit-tree "$tree" -p "$GITHUB_SHA" -m moved)
    export GITHUB_SHA TEST_OTHER_SHA

    cat > bin/gh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == release && $3 == "release-draft-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT" ]]
case "$2" in
    create) [[ " $* " == *' --draft '* ]] ;;
    edit)
        [[ " $* " == *' --draft=false '* && " $* " == *' --verify-tag '* &&
            " $* " == *" --tag $GITHUB_REF_NAME "* && " $* " == *" --target $GITHUB_SHA "* ]]
        ;;
    *) exit 2 ;;
esac
printf '%s\n' "$2" >> "$TRACE"
if [[ $2 == create && ${TEST_MOVE_TAG_ON_UPLOAD:-} == 1 ]]; then
    git --git-dir="$TEST_REMOTE" update-ref "$GITHUB_REF" "$TEST_OTHER_SHA"
fi
EOF
    chmod +x bin/gh
    : > "$TRACE"

    tag_git update-ref "$GITHUB_REF" "$TEST_OTHER_SHA"
    if bash -euo pipefail "$root/publish-release" > "$root/moved-tag.log" 2>&1; then
        printf 'Publisher accepted a tag pointing to another commit\n' >&2
        exit 1
    fi
    [[ ! -s $TRACE ]]

    tag_git update-ref -d "$GITHUB_REF"
    if bash -euo pipefail "$root/publish-release" > "$root/missing-tag.log" 2>&1; then
        printf 'Publisher accepted a deleted tag\n' >&2
        exit 1
    fi
    [[ ! -s $TRACE ]]

    tag_git update-ref "$GITHUB_REF" "$GITHUB_SHA"
    bash -euo pipefail "$root/publish-release"
    expect_trace create edit

    tag_git tag -f -a "$GITHUB_REF_NAME" "$GITHUB_SHA" -m release >/dev/null
    bash -euo pipefail "$root/publish-release"
    expect_trace create edit

    if TEST_MOVE_TAG_ON_UPLOAD=1 bash -euo pipefail "$root/publish-release" > "$root/tag-moved-on-upload.log" 2>&1; then
        printf 'Publisher accepted a tag that moved while uploading assets\n' >&2
        exit 1
    fi
    expect_trace create
)

if (( $# == 0 )); then
    set -- corrupt_release_cache trailing_slash publish_tag
fi
for name in "$@"; do
    EGR_CITYMAP_PUBLISHED="$root/release/world" "test_$name"
done
