#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/c-api/check.sh <directory of the built eggdb-capi library> <work directory>

Compiles crates/eggdb-capi/tests/c/api_check.c against include/eggdb.h and the shared library
with the system C compiler (CC, default cc; warnings are errors), checks that the header also
compiles as C++ (CXX, default c++), and runs the program. Then runs
examples/python/eggdb_ctypes.py and the Python driver's tests with PYTHON (default python3)
against the same library, the Node.js and Go drivers' tests where Node's headers and Go are installed, and
the .NET and Java drivers' check programs where dotnet and a Java 22 or newer javac are installed.
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
if command -v dotnet >/dev/null 2>&1; then
  dotnet build "$root/drivers/dotnet/EggDB.Data.Check" -o "$work/dotnet" > "$work/dotnet-build.log" 2>&1 \
    || { cat "$work/dotnet-build.log" >&2; exit 1; }
  cp "$library" "$work/dotnet/"
  dotnet "$work/dotnet/EggDB.Data.Check.dll"
fi
node_include="$(dirname "$(command -v node 2>/dev/null || echo /none/node)")/../include/node"
if [ "${NO_C_COMPILER:-}" != 1 ] && [ -f "$node_include/node_api.h" ]; then
  case "$(uname -s)" in
    Darwin) addon_link=(-bundle -undefined dynamic_lookup) ;;
    *) addon_link=(-shared -fPIC) ;;
  esac
  "${CC:-cc}" -std=c11 -Wall -Wextra -Werror "${addon_link[@]}" -I "$node_include" -I "$include" \
    "$root/drivers/node/src/addon.c" -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
    -o "$work/eggdb.node"
  EGGDB_NODE_ADDON="$work/eggdb.node" node --test "$root/drivers/node/test/database.test.js"
fi
if [ "${NO_C_COMPILER:-}" != 1 ] && command -v go >/dev/null 2>&1; then
  (cd "$root/drivers/go" && CGO_ENABLED=1 CGO_LDFLAGS="-L$library_dir -Wl,-rpath,$library_dir" go test ./...)
fi
if command -v javac >/dev/null 2>&1 && javac --release 22 -version >/dev/null 2>&1; then
  javac --release 22 -d "$work/java" $(find "$root/drivers/java/src" "$root/drivers/java/check" -name '*.java')
  cp -R "$root/drivers/java/src/META-INF" "$work/java/"
  java --enable-native-access=ALL-UNNAMED -Deggdb.library="$library" -cp "$work/java" Check
fi
echo "c-api check: passed"
