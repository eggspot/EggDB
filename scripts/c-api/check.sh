#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/c-api/check.sh <directory of the built eggdb-capi library> <work directory>

Compiles crates/eggdb-capi/tests/c/api_check.c against include/eggdb.h and the shared library
with the system C compiler (CC, default cc; warnings are errors), checks that the header also
compiles as C++ (CXX, default c++), and runs the program. Then runs
examples/python/eggdb_ctypes.py and the Python driver's tests with PYTHON (default python3)
against the same library, once through ctypes and once through its C extension (eggdb._native), the Node.js, Go, C++, Ruby, PHP and Dart drivers' checks where their tools are installed, and
the .NET and Java drivers' check programs where dotnet and a Java 22 or newer javac are installed. The
JDBC driver is compiled for Java 17 but for its foreign-function classes, and its check runs twice:
through Java's foreign function API and through its JNI binding (drivers/java/jni).
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
EGGDB_LIBRARY="$library" PYTHONPATH="$root/drivers/python" EGGDB_PYTHON_NATIVE=0 \
  "${PYTHON:-python3}" -m unittest discover -s "$root/drivers/python/tests"
python_include=$("${PYTHON:-python3}" -c 'import sysconfig; print(sysconfig.get_paths()["include"])')
python_suffix=$("${PYTHON:-python3}" -c 'import sysconfig; print(sysconfig.get_config_var("EXT_SUFFIX"))')
if [ "${NO_C_COMPILER:-}" != 1 ] && [ -f "$python_include/Python.h" ]; then
  mkdir -p "$work/python"
  cp -R "$root/drivers/python/eggdb" "$work/python/"
  case "$(uname -s)" in
    Darwin) python_link=(-bundle -undefined dynamic_lookup) ;;
    *) python_link=(-shared -fPIC -ldl) ;;
  esac
  "${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror "${python_link[@]}" -I "$python_include" -I "$include" \
    "$root/drivers/python/eggdb/_native.c" -o "$work/python/eggdb/_native$python_suffix"
  EGGDB_LIBRARY="$library" PYTHONPATH="$work/python" EGGDB_PYTHON_NATIVE=1 \
    "${PYTHON:-python3}" -m unittest discover -s "$root/drivers/python/tests"
fi
if command -v dotnet >/dev/null 2>&1; then
  env -u VERSION dotnet build "$root/drivers/dotnet/EggDB.Data.Check" -o "$work/dotnet" > "$work/dotnet-build.log" 2>&1 \
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
if [ "${NO_C_COMPILER:-}" != 1 ]; then
  "${CXX:-c++}" -std=c++17 -Wall -Wextra -Werror -I "$include" -I "$root/drivers/cpp" \
    "$root/drivers/cpp/check.cpp" -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
    -o "$work/cpp_check"
  "$work/cpp_check"
fi
if command -v ruby >/dev/null 2>&1; then
  EGGDB_LIBRARY="$library" ruby "$root/drivers/ruby/test/eggdb_test.rb"
fi
if command -v php >/dev/null 2>&1 && php -m | grep -qx FFI; then
  EGGDB_LIBRARY="$library" php -d ffi.enable=1 "$root/drivers/php/tests/check.php"
fi
if command -v swift >/dev/null 2>&1 && [ "$(uname -s)" = Darwin ]; then
  (cd "$root/drivers/swift" && swift test --scratch-path "$work/swift" \
    -Xlinker -L"$library_dir" -Xlinker -rpath -Xlinker "$library_dir")
fi
if command -v dart >/dev/null 2>&1; then
  EGGDB_LIBRARY="$library" dart run "$root/drivers/dart/test/check.dart"
fi
if command -v javac >/dev/null 2>&1 && javac --release 22 -version >/dev/null 2>&1; then
  java_sources="$root/drivers/java/src/app/eggspot/eggdb"
  foreign_sources=(FfmCalls NativeLibrary KeyValueApi EggDbTransaction EggDbKeyValue)
  mkdir -p "$work/java"
  find "$java_sources" -name '*.java' $(printf "! -name %s.java " "${foreign_sources[@]}") \
    -exec javac --release 17 -d "$work/java" {} +
  javac --release 22 -cp "$work/java" -d "$work/java" $(printf "$java_sources/%s.java " "${foreign_sources[@]}")
  javac --release 22 -cp "$work/java" -d "$work/java" "$root/drivers/java/check/Check.java"
  cp -R "$root/drivers/java/src/META-INF" "$work/java/"
  java --enable-native-access=ALL-UNNAMED -Deggdb.library="$library" -cp "$work/java" Check
  java_home="${JAVA_HOME:-$(dirname "$(dirname "$(readlink -f "$(command -v javac)")")")}"
  case "$(uname -s)" in
    Darwin) jni_platform=darwin; jni_library="$work/libeggdb_jdbc.dylib"; jni_link=(-dynamiclib) ;;
    *) jni_platform=linux; jni_library="$work/libeggdb_jdbc.so"; jni_link=(-shared -fPIC) ;;
  esac
  if [ "${NO_C_COMPILER:-}" != 1 ] && [ -f "$java_home/include/jni.h" ]; then
    "${CC:-cc}" -std=c11 -Wall -Wextra -Werror "${jni_link[@]}" -I "$java_home/include" -I "$java_home/include/$jni_platform" \
      -I "$include" "$root/drivers/java/jni/eggdb_jdbc.c" -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
      -o "$jni_library"
    java --enable-native-access=ALL-UNNAMED -Deggdb.library="$library" -Deggdb.binding=jni -Deggdb.jni="$jni_library" \
      -cp "$work/java" Check
  fi
fi
echo "c-api check: passed"
