#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/apple/test-simulator.sh

Builds the C API as drivers/swift/CEggDB.xcframework (scripts/apple/xcframework.sh) and runs the
Swift package's tests against it twice: with swift test on this Mac, and with xcodebuild on the
first available iPhone simulator (D222, D1093). Needs Xcode and its iOS simulator runtime."

[ $# -eq 0 ] || { echo "$usage" >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
framework=drivers/swift/CEggDB.xcframework
[ ! -e "$framework" ] || { echo "$framework exists from an earlier run: remove it first" >&2; exit 1; }
scripts/apple/xcframework.sh "$framework"
cd drivers/swift
export EGGDB_XCFRAMEWORK=CEggDB.xcframework
swift test
device=$(xcrun simctl list devices available | sed -n 's/^ *\(iPhone[^(]*\) (.*/\1/p' | sed 's/ *$//' | head -n 1)
[ -n "$device" ] || { echo "no iPhone simulator is available" >&2; exit 1; }
scheme=$(xcodebuild -list | sed -n 's/^ *\(EggDB-Package\)$/\1/p' | head -n 1)
xcodebuild test -scheme "${scheme:-EggDB}" -destination "platform=iOS Simulator,name=$device" -skipPackagePluginValidation
