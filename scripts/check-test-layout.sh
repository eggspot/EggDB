#!/bin/sh
set -eu

root=${1:-.}
failures=0
packages=0

fail() {
  echo "check-test-layout: $1" >&2
  failures=$((failures + 1))
}

for manifest in "$root"/Cargo.toml "$root"/crates/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  grep -qx '\[package\]' "$manifest" || continue
  package=$(dirname "$manifest")
  tests="$package/tests"
  [ -d "$tests" ] || continue
  packages=$((packages + 1))

  grep -qx 'autotests = false' "$manifest" ||
    fail "$manifest has a tests/ folder but no 'autotests = false'"
  [ "$(grep -cx '\[\[test\]\]' "$manifest")" = 1 ] ||
    fail "$manifest needs exactly one [[test]] target, named \"all\""
  grep -qx 'name = "all"' "$manifest" && grep -qx 'path = "tests/all/main.rs"' "$manifest" ||
    fail "$manifest: its [[test]] target must be name = \"all\", path = \"tests/all/main.rs\""

  for stray in "$tests"/*.rs; do
    [ -e "$stray" ] || continue
    fail "$stray is a test program of its own: move it to tests/all/ and declare it in tests/all/main.rs"
  done
  for program in "$tests"/*/main.rs; do
    [ -e "$program" ] || continue
    [ "$program" = "$tests/all/main.rs" ] && continue
    fail "$program is a test program of its own: move its modules into tests/all/"
  done

  main="$tests/all/main.rs"
  if [ ! -f "$main" ]; then
    fail "$main is missing"
    continue
  fi
  for module in "$tests"/all/*.rs "$tests"/all/*/mod.rs; do
    [ -e "$module" ] || continue
    [ "$module" = "$main" ] && continue
    case $module in
      */mod.rs) name=$(basename "$(dirname "$module")") ;;
      *) name=$(basename "$module" .rs) ;;
    esac
    grep -qx "mod $name;" "$main" ||
      fail "$module is never compiled: declare it with 'mod $name;' in $main"
  done
done

if [ "$failures" -gt 0 ]; then
  echo "check-test-layout: $failures problem(s); integration tests go in the package's single tests/all program (D335)" >&2
  exit 1
fi
echo "check-test-layout: $packages packages, one integration-test program each"
