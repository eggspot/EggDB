#!/bin/sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
install_dir=$(mktemp -d)
started=$(date +%s)
EGGDB_INSTALL_DIR="$install_dir" sh "$here/install.sh"
installed=$(date +%s)
"$install_dir/eggdb" version
first_start=$(date +%s)
printf '| Step | Seconds |\n|------|--------:|\n| download and install | %s |\n| first start | %s |\n' \
    "$((installed - started))" "$((first_start - installed))"
