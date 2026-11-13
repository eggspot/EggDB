#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/footprint-image.sh <image:tag> [<previous image:tag>] [results file]

Measures a pushed container image of EggDB (D14, D224, D1096): for each platform, amd64 and arm64,
the bytes a pull downloads (its layers and config, compressed, from the registry's manifests),
and on this machine's platform how long a cold pull takes. Prints the lines in
benchmarks/footprint.txt's layout (container <platform> image_compressed_bytes, container
<platform> pull_ms), the change from the previous image when one is named, and fails when a line
is over its budget. Needs docker with buildx and python3."

[ $# -ge 1 ] && [ $# -le 3 ] || { echo "$usage" >&2; exit 2; }
image=$1
previous=${2:-}
results=${3:-}
root=$(cd "$(dirname "$0")/.." && pwd)
budgets="$root/benchmarks/footprint.txt"

compressed_bytes() {
  local reference=$1 platform=$2 digest
  digest=$(docker buildx imagetools inspect --raw "$reference" | python3 -c '
import json, sys
index = json.load(sys.stdin)
wanted = sys.argv[1]
for manifest in index.get("manifests", []):
    if manifest.get("platform", {}).get("architecture") == wanted:
        print(manifest["digest"])
        break
' "$platform")
  [ -n "$digest" ] || { echo "$reference has no $platform image" >&2; return 1; }
  docker buildx imagetools inspect --raw "${reference%%:*}@$digest" | python3 -c '
import json, sys
manifest = json.load(sys.stdin)
print(manifest["config"]["size"] + sum(layer["size"] for layer in manifest["layers"]))
'
}

here=$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')
lines=()
for platform in amd64 arm64; do
  bytes=$(compressed_bytes "$image" "$platform")
  lines+=("container $platform image_compressed_bytes $bytes")
  if [ -n "$previous" ] && before=$(compressed_bytes "$previous" "$platform" 2>/dev/null); then
    echo "$platform: $bytes bytes compressed, $((bytes - before)) since $previous" >&2
  fi
done
docker image rm -f "$image" >/dev/null 2>&1 || true
started=$(python3 -c 'import time; print(int(time.monotonic() * 1000))')
docker pull --quiet --platform "linux/$here" "$image" >/dev/null
finished=$(python3 -c 'import time; print(int(time.monotonic() * 1000))')
lines+=("container $here pull_ms $((finished - started))")
over=0
for line in "${lines[@]}"; do
  echo "$line"
  read -r system profile metric value <<< "$line"
  budget=$(awk -v s="$system" -v p="$profile" -v m="$metric" '$1 == s && $2 == p && $3 == m { print $4 }' "$budgets")
  if [ -n "$budget" ] && [ "$value" -gt "$budget" ]; then
    echo "over budget: $profile $metric is $value, the budget is $budget" >&2
    over=1
  fi
done
[ -z "$results" ] || printf '%s\n' "${lines[@]}" > "$results"
exit "$over"
