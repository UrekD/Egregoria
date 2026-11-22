#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 ]]; then
    echo "Usage: $0 <cache-file>" >&2
    exit 2
fi

query=$(dirname -- "$0")/lesce.overpassql
cache=$1
validate() {
    [[ $(wc -c < "$1") -le 16777216 ]] &&
        jq -e -s 'length == 1 and (.[0] |
            type == "object" and (has("remark") | not)
            and (.elements | type == "array" and length > 0))' "$1" > /dev/null
}

if [[ -e $cache ]]; then
    validate "$cache" || { echo "Invalid cached Overpass response at $cache" >&2; exit 1; }
    exit 0
fi
mkdir -p -- "$(dirname -- "$cache")"
pending=$(mktemp -- "${cache}.XXXXXX")
trap 'rm -f -- "$pending"' EXIT
curl --fail-with-body --silent --show-error --max-time 135 --max-filesize 16777216 \
    --user-agent 'Egregoria Lesce import/1.0 (local map generation)' \
    --data-urlencode "data@$query" --output "$pending" \
    https://overpass-api.de/api/interpreter
validate "$pending" || { echo "Overpass response is invalid or incomplete" >&2; exit 1; }
mv -f -- "$pending" "$cache"
