#!/usr/bin/env bash
# Generate and cache the playable slots for make build.
set -euo pipefail

cd "$(dirname "$0")/.."
out=$1
export CARGO_TARGET_DIR=$2
town_generator=${3:-}
published_town=${EGR_CITYMAP_PUBLISHED:-}
cache=$CARGO_TARGET_DIR/windows-slots
current=$cache/current
mkdir -p -- "$cache"
# Keep the selected stage in place until the packager has copied it.
exec 9> "$cache/lock"
flock 9

osm_input=${EGR_OSM_INPUT-${EGR_OSM_CACHE:-target/osm-cache}/lesce-20261122-signals.overpass.json}
if [[ ! -v EGR_OSM_INPUT && ! -e $osm_input ]]; then
    scripts/osm/fetch.sh "$osm_input"
fi
# Split the flags as in make osm-lesce, with pathname expansion disabled.
set -f
osm_flags=(${EGR_OSM_FLAGS:-})
set +f

slots=(starter_town showcase_city lesce_osm)
inputs=("$osm_input")
if [[ -n $town_generator ]]; then
    # These are the pinned defaults of the citymap-build CLI.
    town_lesce=${EGR_CITYMAP_LESCE:-/var/tmp/egr-keep/osm/signals/lesce-20261122-signals.overpass.json}
    town_sava=${EGR_CITYMAP_SAVA:-/var/tmp/egr/osm-cache/lesce-town-sava-20261122.overpass.json}
    slots+=(lesce_town)
    inputs+=("$town_lesce" "$town_sava")
    if [[ -n $published_town ]]; then
        inputs+=("$published_town"/lesce_town.{zip,meta.json,build.json})
    fi
else
    printf 'Skipping lesce_town: no citymap save generator is configured on this branch.\n'
fi

fingerprint() {
    # Include the scenarios crate, its local dependencies and runtime inputs.
    # Hash working-tree contents so local edits invalidate a previous build.
    {
        printf '%s\0' "$town_generator" "${osm_flags[@]}" &&
        find base_mod {scenarios,simulation,common,geom,prototypes,egui-inspect,egui-inspect-derive}/src \
            -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum -- &&
        sha256sum -- VERSION Cargo.toml Cargo.lock Makefile scripts/build-windows-with-slots.sh assets/paris_54000.txt \
            {scenarios,simulation,common,geom,prototypes,egui-inspect,egui-inspect-derive}/Cargo.toml \
            scripts/osm/fetch.sh scripts/osm/*.overpassql scripts/osm/ATTRIBUTION-OSM.txt "${inputs[@]}"
    } | sha256sum
}

check_slots() {
    local slot path
    for slot in "${slots[@]}"; do
        for path in "$1/$slot.zip" "$1/$slot.meta.json"; do
            if [[ ! -f $path || ! -s $path ]]; then
                printf 'Missing or empty generated slot file: %s\n' "$path" >&2
                return 1
            fi
        done
    done
}

check_town() {
    [[ -n $town_generator ]] || return 0
    cargo run --locked --release -p scenarios -- citymap-verify --root "$1"
}

generate() {
    if ! cargo run --locked --release -p scenarios -- "$@"; then
        printf 'Windows save generation failed (%s).\n' "$1" >&2
        exit 1
    fi
}

stamp=$(fingerprint)
if [[ -f $current/stamp && $(< "$current/stamp") == "$stamp" ]] &&
    check_slots "$current/slots" 2>/dev/null; then
    check_town "$current/slots"
    printf 'Reusing staged Windows save slots in %s\n' "$current/slots"
else
    printf 'Generating Windows save slots for %s\n' "$(cat VERSION)"
    pending=$(mktemp -d "$cache/.pending.XXXXXX")
    trap 'rm -rf -- "$pending"' EXIT
    generate demo-cities --save-root "$pending/slots"
    generate osm-city --input "$osm_input" --save-root "$pending/slots" "${osm_flags[@]}"
    if [[ -n $town_generator ]]; then
        # The CITYMAP generator requires its output directory to be absent.
        if [[ -n $published_town ]]; then
            cp -a -- "$published_town"/lesce_town.{zip,meta.json,build.json} "$pending/slots/"
        else
            generate "$town_generator" --out "$pending/town" --input "$town_lesce" --sava-input "$town_sava"
            cp -a -- "$pending/town"/. "$pending/slots/"
            rm -rf -- "$pending/town"
        fi
    fi
    check_slots "$pending/slots"
    check_town "$pending/slots"
    if [[ $(fingerprint) != "$stamp" ]]; then
        printf 'Save generator inputs changed during the build. Rerun make build.\n' >&2
        exit 1
    fi
    printf '%s\n' "$stamp" > "$pending/stamp"
    rm -rf -- "$current"
    mv -T -- "$pending" "$current"
fi

scripts/build-windows.sh "$out" "$current/slots"
