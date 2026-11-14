#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/runner-description.sh <output file>"
[ $# -eq 1 ] || { echo "$usage" >&2; exit 2; }
out=$1

system=$(uname -s)
kernel=$(uname -r)
machine=$(uname -m)
case "$system" in
  Linux)
    cpu=$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -n 1)
    cores=$(nproc)
    memory_kib=$(sed -n 's/^MemTotal:[[:space:]]*\([0-9]*\) kB/\1/p' /proc/meminfo)
    memory=$((memory_kib / 1024 / 1024))
    distribution=$(sed -n 's/^PRETTY_NAME="\(.*\)"/\1/p' /etc/os-release 2> /dev/null || echo "$system")
    ;;
  Darwin)
    cpu=$(sysctl -n machdep.cpu.brand_string)
    cores=$(sysctl -n hw.ncpu)
    memory=$(( $(sysctl -n hw.memsize) / 1024 / 1024 / 1024 ))
    distribution="macOS $(sw_vers -productVersion)"
    ;;
  *)
    cpu=unknown
    cores=unknown
    memory=unknown
    distribution=$system
    ;;
esac
rust=$(rustc --version 2> /dev/null || echo unknown)

{
  echo "| Runner | Value |"
  echo "|--------|-------|"
  echo "| Image | ${ImageOS:-local} ${ImageVersion:-} |"
  echo "| System | $distribution, kernel $kernel, $machine |"
  echo "| Processor | ${cpu:-unknown} |"
  echo "| Logical processors | $cores |"
  echo "| Memory | $memory GiB |"
  echo "| Compiler | $rust |"
  echo "| Run | ${GITHUB_REPOSITORY:-local}@${GITHUB_SHA:-working tree} |"
} > "$out"
cat "$out"
