#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --run-private ]]; then
    native=$2
    shots=$3
    slots=$4
    script=$5
    slot=$6
    display=${DISPLAY#:}
    read -r xpid < "/tmp/.X${display%%.*}-lock"
    [[ $(ps -p "$xpid" -o comm=) == Xvfb ]]
    kill -STOP "$xpid"
    trap 'kill -CONT "$xpid"' EXIT
    "$native" --script "$script" --exit-after-script --save-dir "$shots" &
    pid=$!
    trap 'kill -CONT "$xpid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true' EXIT
    while [[ ! -f $shots/.egregoria-run ]]; do
        kill -0 "$pid"
        sleep 0.05
    done
    cp -- "$slots/$slot.meta.json" "$slots/$slot.zip" "$shots/"
    if [[ -f $slots/${slot}_replay.json ]]; then
        cp -- "$slots/${slot}_replay.json" "$shots/"
    fi
    kill -CONT "$xpid"
    wait "$pid"
    trap - EXIT
    exit
fi

native=${EGR_P7_NATIVE:-}
scenario_runner=${EGR_P7_SCENARIOS:-}
slot=lesce_town
out=
slots=
if [[ $# == 1 && $1 != -* ]]; then
    slots=$1
else
    while [[ $# -ge 2 ]]; do
        case $1 in
            --native) native=$2 ;;
            --root) slots=$2 ;;
            --slot) slot=$2 ;;
            --out) out=$2 ;;
            *) break ;;
        esac
        shift 2
    done
    if [[ $# != 0 || -z $slots ]]; then
        printf 'Usage: %s --native APP --root PUBLISHED_SLOTS --slot lesce_town|showcase_city --out DIR\n' "$0" >&2
        exit 2
    fi
fi

case $slot in
    lesce_town) fixture=lesce-release ;;
    showcase_city) fixture=showcase-release ;;
    *) printf 'Unknown traffic screenshot slot %s\n' "$slot" >&2; exit 2 ;;
esac
slots=$(realpath -- "$slots")
if [[ ! -f $slots/$slot.zip && -d $slots/$fixture ]]; then
    slots=$slots/$fixture
fi
[[ -f $slots/$slot.meta.json && -f $slots/$slot.zip ]]
if [[ -n $out ]]; then
    root=$(realpath -m -- "$out")
fi
if [[ -n $native ]]; then
    native=$(realpath -- "$native")
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."
export TMPDIR=${TMPDIR:-/var/tmp/egr}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/home/durek/.cache/egr-target/slot1}
p7_sysroot=${SYSROOT_LIB:-/home/durek/.local/egr-sysroot/usr/lib/x86_64-linux-gnu}
export PKG_CONFIG_PATH=${PKG_CONFIG_PATH:-$p7_sysroot/pkgconfig}
export LIBRARY_PATH=${LIBRARY_PATH:-$p7_sysroot}
mkdir -p -- "$TMPDIR"
if [[ -z $native ]]; then
    cargo build --locked --release -p native_app
    native=$CARGO_TARGET_DIR/release/native_app
fi
if [[ $slot == lesce_town ]]; then
    if [[ -z $scenario_runner ]]; then
        cargo build --locked --release -p scenarios
        scenario_runner=$CARGO_TARGET_DIR/release/scenarios
    fi
    "$scenario_runner" citymap-build --verify "$slots"
fi
if [[ -z $out ]]; then
    root=$(mktemp -d "$TMPDIR/p7-traffic-shots.XXXXXX")
else
    mkdir -p -- "$root"
    if [[ -n $(find "$root" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
        printf 'Shot output directory must be empty: %s\n' "$root" >&2
        exit 2
    fi
fi
printf 'P7 traffic capture root %s\n' "$root"
script=${EGR_P7_SCRIPT:-scripts/shots/p7_traffic.json}
if [[ $slot == showcase_city && ! -v EGR_P7_SCRIPT ]]; then
    python3 - "$script" "$root/showcase.json" <<'PYTHON'
import json
import sys
from pathlib import Path

steps = json.loads(Path(sys.argv[1]).read_text())
steps[0]["LoadSlot"] = "showcase_city"
def supported(step):
    if isinstance(step, str):
        return step != "CompareTrafficCorridor"
    return ("p7-repair-denied" not in step.values()
            and "p7-matched-comparison" not in step.values()
            and not step.get("PreviewTrafficRoad", {}).get("denied", False)
            and step.get("Assert", {}).get("TrafficRepair") not in ("Denied", "Compared"))

steps = [step for step in steps if supported(step)]
for step in steps:
    if isinstance(step, dict):
        if "PreviewTrafficRoad" in step:
            step["PreviewTrafficRoad"]["anchor"] = [950, 920]
        if "Camera" in step:
            step["Camera"].update(x=950, y=920)
Path(sys.argv[2]).write_text(json.dumps(steps))
PYTHON
    script=$root/showcase.json
fi
timeout 900 xvfb-run -a -s '-screen 0 1600x900x24' env \
    WGPU_BACKEND=vulkan VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}" \
    bash scripts/shots/p7_traffic.sh --run-private "$native" "$root/shots" "$slots" "$script" "$slot"
python3 - "$root/shots" "$script" "$slot" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

output = Path(sys.argv[1])
script = json.loads(Path(sys.argv[2]).read_text())
captures = {}
for step in script:
    if isinstance(step, dict) and "Screenshot" in step:
        name = step["Screenshot"]
        path = output / (name + ".png")
        data = path.read_bytes()
        if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 64:
            sys.exit(f"Invalid capture {path}")
        captures[path.name] = hashlib.sha256(data).hexdigest()

def view(name):
    state = json.loads((output / (name + ".json")).read_text())
    if sys.argv[3] == "lesce_town" and state["water_tiles"] == 0:
        sys.exit(f"{name} did not capture wet Lesce Town")
    return state["traffic_repair"]

before = view("p7-road-quote")
applied = view("p7-repair-applied")
presentation = json.loads((output / "p7-repair-applied.json").read_text())["traffic_presentation"]
if presentation["first_revised_frame"] < presentation["submitted_frame"]:
    sys.exit("The revised road was not rendered after the input frame")
reopened = view("p7-repair-reopened")
if any(before[key] != applied[key] or applied[key] != reopened[key] for key in ("road", "lane")):
    sys.exit("Road or lane identity changed through the edit or slot reload")
if applied["debit"] != before["quote_cost"] or applied["speed_limit"] <= before["speed_limit"]:
    sys.exit("The edit receipt and driving speed do not match the quote")
if reopened["speed_limit"] != applied["speed_limit"] or reopened["outcome_generation"] is not None:
    sys.exit("The repaired speed or receipt fence did not survive the slot boundary")
if view("p7-legal-turns")["legal_turns"] == 0:
    sys.exit("The walkthrough did not capture directed turns")
if "p7-repair-denied.png" in captures and not view("p7-repair-denied")["error"]:
    sys.exit("The walkthrough did not capture the edit denial")
if "p7-matched-comparison.png" in captures and view("p7-matched-comparison")["comparison_passed"] is None:
    sys.exit("The walkthrough did not capture the matched comparison verdict")
(output / "captures.json").write_text(json.dumps(captures, indent=2) + "\n")
PY
