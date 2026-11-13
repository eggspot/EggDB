#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/qemu-test.sh target <cpu>
       scripts/qemu-test.sh packages <cpu>
       scripts/qemu-test.sh test <cpu> <engine|sql|rest>

Runs the workspace's tests for another CPU, under QEMU's user-mode emulation where the machine
cannot run the code itself (D8, D1095). <cpu> is one of i686, armv7, s390x, riscv64, ppc64le,
loongarch64. 'target' prints its Rust target, and 'packages' the Debian packages its cross
linker and QEMU need. 'test' runs one
shard: engine (the file format, storage, cryptography, columnar encodings, the platform layer and
the eggdb crate), sql (eggdb-sql), or rest (every other crate but the GUI and the harnesses
that need outside services or data)."

target_of() {
  case "$1" in
    i686) echo "i686-unknown-linux-gnu|gcc-multilib||" ;;
    armv7) echo "armv7-unknown-linux-gnueabihf|gcc-arm-linux-gnueabihf qemu-user|arm-linux-gnueabihf-gcc|qemu-arm -L /usr/arm-linux-gnueabihf" ;;
    s390x) echo "s390x-unknown-linux-gnu|gcc-s390x-linux-gnu qemu-user|s390x-linux-gnu-gcc|qemu-s390x -L /usr/s390x-linux-gnu" ;;
    riscv64) echo "riscv64gc-unknown-linux-gnu|gcc-riscv64-linux-gnu qemu-user|riscv64-linux-gnu-gcc|qemu-riscv64 -L /usr/riscv64-linux-gnu" ;;
    ppc64le) echo "powerpc64le-unknown-linux-gnu|gcc-powerpc64le-linux-gnu qemu-user|powerpc64le-linux-gnu-gcc|qemu-ppc64le -L /usr/powerpc64le-linux-gnu" ;;
    loongarch64) echo "loongarch64-unknown-linux-gnu|gcc-14-loongarch64-linux-gnu qemu-user|loongarch64-linux-gnu-gcc-14|qemu-loongarch64 -L /usr/loongarch64-linux-gnu" ;;
    *) return 1 ;;
  esac
}

engine=(eggdb-format eggdb-storage eggdb-crypto eggdb-columnar eggdb-platform eggdb-platform-core eggdb)
not_emulated=(eggdb-gui eggdb-ui eggdb-fuzz eggdb-jepsen eggdb-sparql-suite eggdb-sqllogictest eggdb-bench eggdb-cluster-bench)

[ $# -ge 2 ] || { echo "$usage" >&2; exit 2; }
command=$1
row=$(target_of "$2") || { echo "unknown cpu '$2'" >&2; echo "$usage" >&2; exit 2; }
IFS='|' read -r target packages linker runner <<< "$row"
case "$command" in
  target)
    echo "$target"
    exit 0
    ;;
  packages)
    echo "$packages"
    exit 0
    ;;
  test)
    [ $# -eq 3 ] || { echo "$usage" >&2; exit 2; }
    ;;
  *)
    echo "$usage" >&2
    exit 2
    ;;
esac
upper=$(echo "$target" | tr 'a-z-' 'A-Z_')
[ -z "$linker" ] || export "CARGO_TARGET_${upper}_LINKER=$linker"
[ -z "$runner" ] || export "CARGO_TARGET_${upper}_RUNNER=$runner"
args=()
case "$3" in
  engine) for package in "${engine[@]}"; do args+=(-p "$package"); done ;;
  sql) args+=(-p eggdb-sql) ;;
  rest)
    args+=(--workspace)
    for package in "${engine[@]}" eggdb-sql "${not_emulated[@]}"; do args+=(--exclude "$package"); done
    ;;
  *) echo "unknown shard '$3'" >&2; echo "$usage" >&2; exit 2 ;;
esac
echo "testing ${args[*]} on $target"
cargo test --target "$target" "${args[@]}"
