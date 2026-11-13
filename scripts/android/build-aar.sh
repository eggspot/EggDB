#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/android/build-aar.sh [release|debug]

Builds the C API (crates/eggdb-capi) for Android's four ABIs with the NDK's clang (arm64-v8a,
armeabi-v7a, x86_64 and x86), puts each libeggdb_capi.so where drivers/android expects it, and
assembles the Android Archive with Gradle: drivers/android/build/outputs/aar/eggdb-android-<kind>.aar,
holding the Kotlin driver, the JNI library and the C API for every ABI (D222, D1092). Needs
ANDROID_NDK_HOME (or ANDROID_NDK_LATEST_HOME), the four Rust targets and gradle."

[ $# -le 1 ] || { echo "$usage" >&2; exit 2; }
kind=${1:-release}
case "$kind" in release|debug) ;; *) echo "$usage" >&2; exit 2 ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}
[ -n "$ndk" ] || { echo "set ANDROID_NDK_HOME to the NDK" >&2; exit 1; }
host=$(ls "$ndk/toolchains/llvm/prebuilt" | head -n 1)
bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
api=23
libs="$root/drivers/android/build/rust-libs"
cargo_profile=$([ "$kind" = release ] && echo --release || true)
profile_dir=$([ "$kind" = release ] && echo release || echo debug)
cd "$root"
for pair in "aarch64-linux-android arm64-v8a aarch64-linux-android" \
            "armv7-linux-androideabi armeabi-v7a armv7a-linux-androideabi" \
            "x86_64-linux-android x86_64 x86_64-linux-android" \
            "i686-linux-android x86 i686-linux-android"; do
  read -r target abi clang_prefix <<< "$pair"
  linker="$bin/${clang_prefix}${api}-clang"
  variable="CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER"
  env "$variable=$linker" "CC_${target//-/_}=$linker" "AR_${target//-/_}=$bin/llvm-ar" \
    cargo build $cargo_profile --locked -p eggdb-capi --target "$target"
  mkdir -p "$libs/$abi"
  cp "target/$target/$profile_dir/libeggdb_capi.so" "$libs/$abi/libeggdb_capi.so"
done
cd drivers/android
gradle --no-daemon "$([ "$kind" = release ] && echo assembleRelease || echo assembleDebug)"
ls -l build/outputs/aar/
