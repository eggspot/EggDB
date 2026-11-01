#!/usr/bin/env bash
set -euo pipefail

out="${1:-target/EggDB.xcframework}"
targets=(aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin)
rustup target add "${targets[@]}"
for target in "${targets[@]}"; do
  cargo build --release -p eggdb-capi --target "$target"
done
headers="$(mktemp -d)"
cp crates/eggdb-capi/include/eggdb.h "$headers/"
printf 'module CEggDB {\n    header "eggdb.h"\n    link "eggdb_capi"\n    export *\n}\n' > "$headers/module.modulemap"
if [ -e "$out" ]; then
  echo "$out already exists: remove it, or name another output" >&2
  exit 1
fi
args=()
for target in "${targets[@]}"; do
  args+=(-library "target/$target/release/libeggdb_capi.a" -headers "$headers")
done
xcodebuild -create-xcframework "${args[@]}" -output "$out"
echo "built $out"
