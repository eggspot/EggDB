#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/c-api/check.sh <directory of the built eggdb-capi library> <work directory>

Compiles crates/eggdb-capi/tests/c/api_check.c against include/eggdb.h and the shared library
with the system C compiler (CC, default cc; warnings are errors), checks that the header also
compiles as C++ (CXX, default c++), and runs the program. Then runs
examples/python/eggdb_ctypes.py with PYTHON (default python3) against the same library.
NO_C_COMPILER=1 leaves out the C and C++ steps, for Windows runners without one."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
library_dir=$(cd "$1" && pwd)
work=$2
root=$(cd "$(dirname "$0")/../.." && pwd)
include="$root/crates/eggdb-capi/include"
mkdir -p "$work"
work=$(cd "$work" && pwd)

case "$(uname -s)" in
  Darwin) library="$library_dir/libeggdb_capi.dylib" ;;
  MINGW* | MSYS* | CYGWIN*) library="$(cd "$library_dir" && pwd -W)/eggdb_capi.dll" ;;
  *) library="$library_dir/libeggdb_capi.so" ;;
esac
[ -f "$library" ] || { echo "c-api check: no library at $library" >&2; exit 2; }

if [ "${NO_C_COMPILER:-}" != 1 ]; then
  "${CC:-cc}" -std=c11 -Wall -Wextra -Wpedantic -Werror \
    -I "$include" "$root/crates/eggdb-capi/tests/c/api_check.c" \
    -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
    -o "$work/api_check"
  printf '#include "eggdb.h"\nint main(void) { return eggdb_status_name(EGGDB_OK) == 0; }\n' \
    > "$work/header_check.cpp"
  "${CXX:-c++}" -std=c++11 -Wall -Wextra -Wpedantic -Werror -fsyntax-only \
    -I "$include" "$work/header_check.cpp"
  "$work/api_check" "$(mktemp -d "$work/run.XXXXXX")"
fi

"${PYTHON:-python3}" "$root/examples/python/eggdb_ctypes.py" --library "$library"
EGGDB_LIBRARY="$library" PYTHONPATH="$root/drivers/python" \
  "${PYTHON:-python3}" -m unittest discover -s "$root/drivers/python/tests" -t "$root/drivers/python"
echo "c-api check: passed"
