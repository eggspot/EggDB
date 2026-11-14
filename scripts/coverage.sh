#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/coverage.sh [--update]"
update=false
case "${1:-}" in
  "") ;;
  --update) update=true ;;
  *) echo "$usage" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
targets=benchmarks/coverage.txt
crates=(eggdb-format eggdb-storage eggdb)

rustup component add llvm-tools > /dev/null
sysroot=$(rustc --print sysroot)
host=$(rustc -vV | sed -n 's/^host: //p')
tools="$sysroot/lib/rustlib/$host/bin"
[ -x "$tools/llvm-cov" ] || { echo "coverage: llvm-cov is not in $tools" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export RUSTC_BOOTSTRAP=1
export RUSTFLAGS="-C instrument-coverage -Z coverage-options=branch"
export LLVM_PROFILE_FILE="$work/profiles/%p-%m.profraw"
export CARGO_TARGET_DIR="$root/target/coverage"

objects=()
for crate in "${crates[@]}"; do
  cargo test -p "$crate" --lib --no-run --message-format=json 2> /dev/null |
    sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' > "$work/$crate.executables"
  cargo test -p "$crate" --lib --quiet
  while IFS= read -r executable; do
    objects+=("--object" "$executable")
  done < "$work/$crate.executables"
done

"$tools/llvm-profdata" merge -sparse "$work"/profiles/*.profraw -o "$work/merged.profdata"
"$tools/llvm-cov" report "${objects[@]}" \
  --instr-profile="$work/merged.profdata" \
  --ignore-filename-regex='(/\.cargo/|/rustc/|_tests\.rs$|/tests/)' \
  --show-branch-summary > "$work/report.txt"

total=$(grep '^TOTAL' "$work/report.txt")
branch=$(echo "$total" | awk '{print $(NF)}' | tr -d '%')
line=$(echo "$total" | awk '{print $(NF-3)}' | tr -d '%')
echo "core branch coverage: $branch% (lines: $line%)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "| Measure | Covered |"
    echo "|---------|--------:|"
    echo "| core branches | $branch% |"
    echo "| core lines | $line% |"
  } >> "$GITHUB_STEP_SUMMARY"
fi
cp "$work/report.txt" "$root/target/coverage/report.txt"

if $update; then
  printf 'core branch_coverage_percent %s\n' "$branch" > "$targets"
  echo "coverage: wrote $targets"
  exit 0
fi
[ -f "$targets" ] || { echo "coverage: $targets is missing; run with --update once" >&2; exit 1; }
target=$(awk '$1 == "core" && $2 == "branch_coverage_percent" {print $3}' "$targets")
if awk -v got="$branch" -v want="$target" 'BEGIN { exit !(got + 0 < want + 0) }'; then
  echo "coverage FAILED: core branch coverage $branch% is below the published target $target%" >&2
  exit 1
fi
echo "coverage: at or above the published target $target%"
