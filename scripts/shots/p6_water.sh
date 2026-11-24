#!/usr/bin/env bash
set -euo pipefail

verify_captures() {
    local shots=$1
    python3 - "$shots" "${BASH_SOURCE[0]%.sh}.json" <<'PY'
import ctypes as c
import json
import sys
from pathlib import Path

shots = Path(sys.argv[1])
steps = json.loads(Path(sys.argv[2]).read_text())
shot_names = [step["Screenshot"].removeprefix("p6-water-") for step in steps if "Screenshot" in step]


def artifact(name, extension):
    path = shots / f"p6-water-{name}.{extension}"
    if not path.is_file() or not path.stat().st_size:
        raise SystemExit(f"Missing capture artifact: {path}")
    return path


def require(condition, message):
    if not condition:
        raise SystemExit(message)


def load(name):
    return json.loads(artifact(name, "json").read_text())


for name in shot_names:
    artifact(name, "png")

mixed = load("mixed-stamp")
wet = load("wet-stamp")
protected = load("protected-footprint")
expected = {(x, y) for x in range(122, 134) for y in range(88, 90)}
require(
    not mixed["rejections"]
    and mixed["zone_cells"] == len(expected)
    and {tuple(cell) for cell in mixed["zone_coordinates"]} == expected,
    "Mixed paint failed to commit the 24 expected dry cells",
)
require(
    wet["rejections"] == [["ZonePaint", "WetFootprint"]],
    "All-wet paint did not report WetFootprint",
)
require(
    protected["rejections"] == [["EditWater", "WetFootprint"]],
    "Flooding a protected footprint did not report WetFootprint",
)
for after in (wet, protected):
    require(after["map_hash"] == mixed["map_hash"], "Rejected action changed the map")
    require(after["zone_coordinates"] == mixed["zone_coordinates"], "Rejected action changed zones")
    require(after["water_revision"] == mixed["water_revision"], "Rejected action changed the water revision")
require(
    protected["freight_stations"] == 1 and protected["water_samples"][3] is None,
    "Protected freight station is missing or wet",
)

before = load("before-replacement")
after = load("dry-replacement")
require(before["overlay"] == after["overlay"] == "WaterBodies", "Water Bodies overlay was not active across replacement")
require(before["water_revision"] == after["water_revision"], "Replacement did not exercise the equal-revision case")
require(before["water_tiles"] > 0 and after["water_tiles"] == 0, "Replacement did not drain all water")
require(
    before["water_samples"][:3] == ["Sea", "River", "Lake"] and after["water_samples"] == [None] * 5,
    "Replacement water samples do not match their slots",
)

starters = load("waters-starters")
road = load("waters-bank-road")
zone = load("waters-bank-zone")
require(
    starters["preset"] == "Waters"
    and starters["water_tiles"] > 0
    and starters["water_samples"][4] == "Sea"
    and starters["overlay"] == "WaterBodies",
    "New Game did not open the Waters preset with a wet coastline and active overlay",
)
require(
    not starters["rejections"]
    and starters["road_ids"]
    and starters["rail_lanes"] >= 2
    and starters["external_markets"] == starters["freight_stations"] == 1,
    "Waters starter infrastructure is incomplete",
)
ids = lambda state: {(road["idx"], road["version"]) for road in state["road_ids"]}
require(
    not road["rejections"] and ids(starters) < ids(road) and len(ids(road) - ids(starters)) == 1,
    "Waters bank road failed to build",
)
require(
    not zone["rejections"] and ids(road) == ids(zone) and zone["zone_cells"] - starters["zone_cells"] == 210,
    "Waters dry-bank paint failed",
)


class PngImage(c.Structure):
    _fields_ = [
        ("opaque", c.c_void_p),
        *((field, c.c_uint32) for field in (
            "version", "width", "height", "format", "flags", "colormap_entries", "warning_or_error",
        )),
        ("message", c.c_char * 64),
    ]


png = c.CDLL("libpng16.so.16")


def pixels(path):
    header = PngImage(version=1)
    try:
        require(
            png.png_image_begin_read_from_file(c.byref(header), bytes(path)),
            f"{path}: {header.message.decode()}",
        )
        require(64 <= header.width <= 8192 and 48 <= header.height <= 8192, f"{path}: invalid dimensions")
        header.format = 3  # PNG_FORMAT_RGBA
        rgba = c.create_string_buffer(header.width * header.height * 4)
        require(
            png.png_image_finish_read(c.byref(header), None, rgba, 0, None),
            f"{path}: {header.message.decode()}",
        )
        return header.width, header.height, rgba.raw
    finally:
        png.png_image_free(c.byref(header))


def lake_color_fraction(width, height, rgba):
    count = 0
    for y in range(height // 2 - 24, height // 2 + 24):
        for x in range(width // 2 - 32, width // 2 + 32):
            i = (y * width + x) * 4
            red, green, blue = rgba[i:i + 3]
            count += green > red + 40 and blue > green
    return count / (64 * 48)


lake_color = {}
for name in shot_names:
    width, height, rgba = pixels(artifact(name, "png"))
    if name in ("before-replacement", "dry-replacement"):
        lake_color[name] = lake_color_fraction(width, height, rgba)
require(
    lake_color["before-replacement"] > 0.9 and lake_color["dry-replacement"] < 0.1,
    f"Lake pixels did not clear after replacement: {lake_color}",
)
PY
}

if [[ ${1:-} == --verify ]]; then
    verify_captures "$2"
    exit
fi

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
if ! rg -q '^[[:space:]]*WaterBodies,' native_app/src/gui/overlays/mod.rs ||
    ! rg -q 'zone_coordinates:' native_app/src/automation.rs; then
    printf 'This shot script requires W4 Water Bodies and the W5 automation state fields.\n' >&2
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
