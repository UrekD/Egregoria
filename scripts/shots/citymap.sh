#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --native ]]; then
    native=$2
    shots=$3
    slots=$4

    # Native writes its run marker after validating the empty save root.
    # Pause the private display until the slot and its sidecar are in place.
    display=${DISPLAY#:}
    read -r xpid < "/tmp/.X${display%%.*}-lock"
    [[ $(ps -p "$xpid" -o comm=) == Xvfb ]]
    kill -STOP "$xpid"
    trap 'kill -CONT "$xpid"' EXIT
    "$native" --script scripts/shots/citymap.json --exit-after-script --save-dir "$shots" &
    pid=$!
    trap 'kill -CONT "$xpid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true' EXIT
    while [[ ! -f $shots/.egregoria-run ]]; do
        kill -0 "$pid"
        sleep 0.05
    done
    cp -- "$slots/lesce_town.meta.json" "$shots/"
    cp -- "$slots/lesce_town.zip" "$shots/"
    kill -CONT "$xpid"
    wait "$pid"
    trap - EXIT
    exit
fi

if [[ $# != 1 ]]; then
    printf 'Usage: %s PUBLISHED_CITYMAP_DIRECTORY\n' "$0" >&2
    exit 2
fi
slots=$(realpath -- "$1")
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."

python3 - "$slots" <<'PY'
import json
import sys
from pathlib import Path

slots = Path(sys.argv[1])
report = json.loads((slots / "lesce_town.build.json").read_text())
meta = json.loads((slots / "lesce_town.meta.json").read_text())
if (report["variant"] != "wet" or report["save_version"] != Path("VERSION").read_text().strip()
        or int(report["snapshot_revision"], 16) != meta["revision"] or meta["population"] <= 0):
    sys.exit("Captures require the commissioned wet Lesce Town slot for this VERSION")
PY

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
export TMPDIR=${TMPDIR:-/var/tmp/egr}
sysroot=${SYSROOT_LIB:-$HOME/.local/egr-sysroot/usr/lib/x86_64-linux-gnu}
export PKG_CONFIG_PATH=${PKG_CONFIG_PATH:-$sysroot/pkgconfig}
export LIBRARY_PATH=${LIBRARY_PATH:-$sysroot}
mkdir -p -- "$TMPDIR"
root=$(mktemp -d "$TMPDIR/citymap-shots.XXXXXX")
native=${EGR_CITYMAP_NATIVE:-$CARGO_TARGET_DIR/release/native_app}
if [[ ! -v EGR_CITYMAP_NATIVE ]]; then
    cargo build --locked --release -p native_app
fi
timeout 900 xvfb-run -a -s '-screen 0 1600x900x24' env \
    WGPU_BACKEND=vulkan VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}" \
    bash scripts/shots/citymap.sh --native "$native" "$root/shots" "$slots"
python3 - "$root/shots" <<'PY'
import json
import sys
from pathlib import Path

output = Path(sys.argv[1])
for step in json.loads(Path("scripts/shots/citymap.json").read_text()):
    if isinstance(step, dict) and "Screenshot" in step:
        path = output / (step["Screenshot"] + ".png")
        with path.open("rb") as capture:
            if capture.read(8) != b"\x89PNG\r\n\x1a\n" or path.stat().st_size < 64:
                sys.exit(f"Invalid capture {path}")
PY
printf 'Lesce Town captures in %s/shots\n' "$root"
