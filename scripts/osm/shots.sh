#!/usr/bin/env bash
set -euo pipefail

native=$1
shots=$2
source=$3

# xvfb-run owns this private display. Native creates its startup marker before
# connecting to X, so suspending the server prevents any script step from running.
display=${DISPLAY#:}
read -r xpid < "/tmp/.X${display%%.*}-lock"
[[ $(ps -p "$xpid" -o comm=) == Xvfb ]]
kill -STOP "$xpid"
trap 'kill -CONT "$xpid"' EXIT
"$native" --script scripts/shots/osm_lesce.json --exit-after-script --save-dir "$shots" &
pid=$!
trap 'kill -CONT "$xpid"; kill "$pid" 2>/dev/null || true' EXIT
while [[ ! -f $shots/.egregoria-run ]]; do
    kill -0 "$pid"
    sleep 0.05
done
cp -- "$source/lesce_osm.zip" "$source/lesce_osm.meta.json" "$source/ATTRIBUTION-OSM.txt" "$shots/"
kill -CONT "$xpid"
wait "$pid"
trap - EXIT
