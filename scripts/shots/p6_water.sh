#!/usr/bin/env bash
set -euo pipefail

verify_captures() {
    local shots=$1
    python3 - "$shots" <<'PY'
import json
import sys
from pathlib import Path

shots = Path(sys.argv[1])


def load(name):
    return json.loads((shots / f"p6-water-{name}.json").read_text())


mixed = load("mixed-stamp")
wet = load("wet-stamp")
protected = load("protected-footprint")
if mixed["rejections"] or not 0 < mixed["zone_cells"] < 120:
    raise SystemExit(f"Mixed paint failed to commit only the dry subset: {mixed}")
if wet["rejections"] != [["ZonePaint", "WetFootprint"]] or wet["zone_cells"] != mixed["zone_cells"]:
    raise SystemExit(f"All-wet paint failed to reject atomically: {wet}")
if protected["rejections"] != [["EditWater", "WetFootprint"]]:
    raise SystemExit(f"Flooding a protected footprint failed to reject: {protected}")

starters = load("waters-starters")
road = load("waters-bank-road")
zone = load("waters-bank-zone")
if road["rejections"] or len(road["road_ids"]) != len(starters["road_ids"]) + 1:
    raise SystemExit(f"Waters bank road failed to build: {road}")
if zone["rejections"] or zone["zone_cells"] - starters["zone_cells"] != 210:
    raise SystemExit(f"Waters dry-bank paint failed: {zone}")
PY
}

if [[ ${1:-} == --native ]]; then
    native=$2
    shots=$3
    slots=$4

    # Native validates the empty run directory and writes the marker before X
    # can drive the script. Pause only the private display allocated by xvfb-run.
    display=${DISPLAY#:}
    read -r xpid < "/tmp/.X${display%%.*}-lock"
    [[ $(ps -p "$xpid" -o comm=) == Xvfb ]]
    kill -STOP "$xpid"
    trap 'kill -CONT "$xpid"' EXIT
    "$native" --script scripts/shots/p6_water.json --exit-after-script --save-dir "$shots" &
    pid=$!
    trap 'kill -CONT "$xpid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true' EXIT
    while [[ ! -f $shots/.egregoria-run ]]; do
        kill -0 "$pid"
        sleep 0.05
    done
    for name in water_acceptance water_dry; do
        cp -- "$slots/$name.zip" "$slots/$name.meta.json" "$shots/"
    done
    kill -CONT "$xpid"
    wait "$pid"
    trap - EXIT
    verify_captures "$shots"
    exit
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."
if ! rg -q '^[[:space:]]*WaterBodies,' native_app/src/gui/overlays/mod.rs; then
    printf 'The P6 Water Bodies overlay from W4 is required for this shot script.\n' >&2
    exit 1
fi

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
sysroot=${SYSROOT_LIB:-$HOME/.local/egr-sysroot/usr/lib/x86_64-linux-gnu}
export PKG_CONFIG_PATH=${PKG_CONFIG_PATH:-$sysroot/pkgconfig}
export LIBRARY_PATH=${LIBRARY_PATH:-$sysroot}

mkdir -p /var/tmp/egr
root=$(mktemp -d /var/tmp/egr/p6-water-shots.XXXXXX)
printf 'WATER captures and fixtures in %s\n' "$root"
cargo build --locked --release -p scenarios -p native_app
"$CARGO_TARGET_DIR/release/scenarios" water-fixtures --save-root "$root/slots"

timeout 900 xvfb-run -a -s '-screen 0 1600x900x24' env \
    WGPU_BACKEND=vulkan VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}" \
    bash scripts/shots/p6_water.sh --native "$CARGO_TARGET_DIR/release/native_app" "$root/shots" "$root/slots"
