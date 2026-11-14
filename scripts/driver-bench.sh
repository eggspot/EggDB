#!/usr/bin/env bash
set -uo pipefail

usage="usage: scripts/driver-bench.sh <directory of the release eggdb-capi library> <work directory> <results file>

Runs each language driver's benchmark (drivers/<language>/bench) in embedded mode against SQLite's
binding in the same language, for every language whose tools are installed: the same prepared
inserts in one transaction, point lookups by primary key and a stepped scan. Writes one line per
language and workload, '<language> <workload> <eggdb ms> <sqlite ms> <ratio>', to the results file,
and fails when a ratio is above DRIVER_BENCH_SLOWEST_RATIO (default 2, as D7 sets for the engine's
own benchmarks) or a language named in DRIVER_BENCH_REQUIRED produced no lines."

[ $# -eq 3 ] || { echo "$usage" >&2; exit 2; }
library_dir=$(cd "$1" && pwd)
work=$2
results=$3
root=$(cd "$(dirname "$0")/.." && pwd)
include="$root/crates/eggdb-capi/include"
slowest="${DRIVER_BENCH_SLOWEST_RATIO:-2}"
mkdir -p "$work"
work=$(cd "$work" && pwd)
raw="$work/raw.txt"
: > "$raw"

case "$(uname -s)" in
  Darwin) library="$library_dir/libeggdb_capi.dylib" ;;
  *) library="$library_dir/libeggdb_capi.so" ;;
esac
[ -f "$library" ] || { echo "driver bench: no library at $library" >&2; exit 2; }
export EGGDB_LIBRARY="$library"

bench() {
  local language=$1
  shift
  echo "driver bench: $language" >&2
  if ! "$@" >> "$raw"; then
    echo "driver bench: $language failed" >&2
    echo "$language failed" >> "$work/failed.txt"
  fi
}

if command -v python3 >/dev/null 2>&1; then
  bench python env PYTHONPATH="$root/drivers/python" python3 "$root/drivers/python/bench/bench.py"
fi
node_include="$(dirname "$(command -v node 2>/dev/null || echo /none/node)")/../include/node"
if [ -f "$node_include/node_api.h" ] && node -e "require('node:sqlite')" 2>/dev/null; then
  case "$(uname -s)" in
    Darwin) addon_link=(-bundle -undefined dynamic_lookup) ;;
    *) addon_link=(-shared -fPIC) ;;
  esac
  "${CC:-cc}" -std=c11 -O2 "${addon_link[@]}" -I "$node_include" -I "$include" \
    "$root/drivers/node/src/addon.c" -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
    -o "$work/eggdb.node"
  bench node env EGGDB_NODE_ADDON="$work/eggdb.node" node --no-warnings "$root/drivers/node/bench/bench.js"
fi
if command -v go >/dev/null 2>&1; then
  cp -R "$root/drivers/go/bench" "$work/go-bench"
  sed -i.bak "s|=> ../|=> $root/drivers/go|" "$work/go-bench/go.mod"
  (cd "$work/go-bench" && go mod tidy >/dev/null 2>&1)
  bench go env CGO_ENABLED=1 CGO_LDFLAGS="-L$library_dir -Wl,-rpath,$library_dir" \
    go run -C "$work/go-bench" .
fi
if command -v "${CXX:-c++}" >/dev/null 2>&1 && echo '#include <sqlite3.h>' | "${CXX:-c++}" -E -x c++ - >/dev/null 2>&1; then
  "${CXX:-c++}" -std=c++17 -O2 -I "$include" -I "$root/drivers/cpp" "$root/drivers/cpp/bench/bench.cpp" \
    -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" -lsqlite3 -o "$work/cpp_bench"
  mkdir -p "$work/cpp"
  bench c++ "$work/cpp_bench" "$work/cpp"
fi
if command -v ruby >/dev/null 2>&1 && ruby -e 'require "sqlite3"' 2>/dev/null; then
  bench ruby ruby "$root/drivers/ruby/bench/bench.rb"
fi
if command -v php >/dev/null 2>&1 && php -m | grep -qx FFI && php -m | grep -qx sqlite3; then
  bench php php -d ffi.enable=1 "$root/drivers/php/bench/bench.php"
fi
if command -v dotnet >/dev/null 2>&1; then
  if env -u VERSION dotnet build -c Release "$root/drivers/dotnet/EggDB.Data.Bench" -o "$work/dotnet" > "$work/dotnet-build.log" 2>&1; then
    cp "$library" "$work/dotnet/"
    bench dotnet dotnet "$work/dotnet/EggDB.Data.Bench.dll"
  else
    cat "$work/dotnet-build.log" >&2
    echo "dotnet failed" >> "$work/failed.txt"
  fi
fi
sqlite_jdbc="${SQLITE_JDBC_JAR:-}"
if command -v javac >/dev/null 2>&1 && javac --release 22 -version >/dev/null 2>&1 && [ -f "$sqlite_jdbc" ]; then
  mkdir -p "$work/java"
  find "$root/drivers/java/src" -name '*.java' -exec javac --release 22 -d "$work/java" {} +
  javac --release 22 -cp "$work/java" -d "$work/java" "$root/drivers/java/bench/Bench.java"
  cp -R "$root/drivers/java/src/META-INF" "$work/java/"
  bench java java --enable-native-access=ALL-UNNAMED -Deggdb.library="$library" -cp "$work/java:$sqlite_jdbc" Bench
fi
if command -v kotlinc >/dev/null 2>&1 && [ -f "$sqlite_jdbc" ] && [ "$(uname -s)" = Linux ]; then
  java_home="${JAVA_HOME:-$(dirname "$(dirname "$(readlink -f "$(command -v javac)")")")}"
  mkdir -p "$work/kotlin"
  "${CC:-cc}" -shared -fPIC -O2 -I"$java_home/include" -I"$java_home/include/linux" -I "$include" \
    "$root/drivers/kotlin/jni/eggdb_jni.c" -L "$library_dir" -leggdb_capi -Wl,-rpath,"$library_dir" \
    -o "$work/kotlin/libeggdb_jni.so"
  kotlinc "$root/drivers/kotlin/src" "$root/drivers/kotlin/bench/Bench.kt" -cp "$sqlite_jdbc" -include-runtime \
    -d "$work/kotlin/bench.jar" 2> "$work/kotlin-build.log"
  bench kotlin java -Djava.library.path="$work/kotlin" -cp "$work/kotlin/bench.jar:$sqlite_jdbc" BenchKt
fi
if command -v dart >/dev/null 2>&1; then
  (cd "$root/drivers/dart/bench" && dart pub get >/dev/null 2>&1)
  bench dart dart run "$root/drivers/dart/bench/bin/bench.dart"
fi
if command -v swift >/dev/null 2>&1 && [ "$(uname -s)" = Darwin ]; then
  bench swift swift run -c release --package-path "$root/drivers/swift" --scratch-path "$work/swift" \
    -Xlinker -L"$library_dir" -Xlinker -rpath -Xlinker "$library_dir" EggDBBench
fi

: > "$results"
failed=0
while read -r language workload mine theirs; do
  case "$mine$theirs" in *[!0-9.]*) continue ;; esac
  ratio=$(awk -v mine="$mine" -v theirs="$theirs" 'BEGIN { printf "%.2f", theirs > 0 ? mine / theirs : 0 }')
  echo "$language $workload $mine $theirs $ratio" >> "$results"
  if awk -v ratio="$ratio" -v slowest="$slowest" 'BEGIN { exit !(ratio > slowest) }'; then
    echo "driver bench: $language $workload is ${ratio}x SQLite's binding, above ${slowest}x" >&2
    failed=1
  fi
done < "$raw"
for language in ${DRIVER_BENCH_REQUIRED:-}; do
  if ! grep -q "^$language " "$results"; then
    echo "driver bench: $language produced no results" >&2
    failed=1
  fi
done
if [ -f "$work/failed.txt" ]; then
  failed=1
fi
column -t "$results" 2>/dev/null || cat "$results"
exit "$failed"
