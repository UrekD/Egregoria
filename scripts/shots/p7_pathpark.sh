#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --private ]]; then
    native=$2
    out=$3
    display=${DISPLAY#:}
    read -r xpid < "/tmp/.X${display%%.*}-lock"
    [[ $(ps -p "$xpid" -o comm=) == Xvfb ]]
    kill -STOP "$xpid"
    trap 'kill -CONT "$xpid" 2>/dev/null || true' EXIT
    "$native" --script scripts/shots/p7_pathpark.json --exit-after-script --save-dir "$out/shots" &
    app=$!
    trap 'kill -CONT "$xpid" 2>/dev/null || true; kill "$app" 2>/dev/null || true' EXIT
    while [[ ! -f $out/shots/.egregoria-run ]]; do
        kill -0 "$app"
        sleep 0.05
    done
    cp -- "$out/slots/pathpark_growables.zip" "$out/slots/pathpark_growables.meta.json" "$out/shots/"
    kill -CONT "$xpid"
    wait "$app"
    trap - EXIT
    exit
fi

if [[ $# != 3 ]]; then
    echo "Usage: $0 SCENARIOS NATIVE_APP NEW_OUTPUT_DIRECTORY" >&2
    exit 2
fi
runner=$(realpath "$1")
native=$(realpath "$2")
out=$(realpath -m "$3")
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."
mkdir -- "$out"
"$runner" pathpark-fixture --save-root "$out/slots"
timeout 900 xvfb-run -a -s '-screen 0 1600x900x24' env \
    WGPU_BACKEND=vulkan \
    VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}" \
    bash scripts/shots/p7_pathpark.sh --private "$native" "$out"
for name in driveways garage-inspector public-garages garage-entrance; do
    test -s "$out/shots/p7-pathpark-$name.png"
done
