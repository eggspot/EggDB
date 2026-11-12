#!/usr/bin/env bash
set -euo pipefail

: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to the Android NDK}"
out="${1:-drivers/dart/flutter/android/src/main/jniLibs}"
api="${ANDROID_API:-21}"
case "$(uname -s)" in
  Darwin) host="darwin-x86_64" ;;
  *) host="linux-x86_64" ;;
esac
toolchain="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin"
targets=(aarch64-linux-android armv7-linux-androideabi x86_64-linux-android)
abis=(arm64-v8a armeabi-v7a x86_64)
clangs=(aarch64-linux-android armv7a-linux-androideabi x86_64-linux-android)
rustup target add "${targets[@]}"
for index in "${!targets[@]}"; do
  target="${targets[$index]}"
  variable="CARGO_TARGET_$(echo "$target" | tr '[:lower:]-' '[:upper:]_')_LINKER"
  export "$variable=$toolchain/${clangs[$index]}$api-clang"
  cargo build --release -p eggdb-capi --target "$target"
  mkdir -p "$out/${abis[$index]}"
  cp "target/$target/release/libeggdb_capi.so" "$out/${abis[$index]}/"
done
echo "copied libeggdb_capi.so for ${abis[*]} into $out"
