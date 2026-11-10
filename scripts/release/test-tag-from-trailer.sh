#!/bin/sh
set -eu

script=$(cd "$(dirname "$0")" && pwd)/tag-from-trailer.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

git_in() {
  git -C "$work/repo" -c user.name=test -c user.email=test@example.com "$@"
}

commit() {
  version=$1
  notes=$2
  message=$3
  printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\n' "$version" > "$work/repo/Cargo.toml"
  mkdir -p "$work/repo/docs/releases"
  if [ -n "$notes" ]; then
    printf 'notes\n' > "$work/repo/docs/releases/$notes.md"
  fi
  git_in add -A
  printf '%s\n' "$message" > "$work/message"
  git_in commit -q --allow-empty -F "$work/message"
  git_in rev-parse HEAD
}

run() {
  (cd "$work/repo" && RELEASE_REMOTE="$work/remote.git" "$script" "$1") 2>"$work/error" \
    && echo ok >"$work/status" || echo failed >"$work/status"
}

expect() {
  name=$1
  reason=${5:-}
  wanted_status=$2
  wanted_output=$3
  output=$4
  status=$(cat "$work/status")
  if [ -n "$reason" ] && ! grep -qF -- "$reason" "$work/error"; then
    status="failed for another reason"
  fi
  if [ "$status" != "$wanted_status" ] || [ "$output" != "$wanted_output" ]; then
    echo "FAIL $name: status $status (wanted $wanted_status), output [$output] (wanted [$wanted_output]): $(cat "$work/error")"
    failures=$((failures + 1))
  else
    echo "ok   $name"
  fi
}

git init -q --bare "$work/remote.git"
git init -q "$work/repo"
git_in remote add origin "$work/remote.git"

released=$(commit 0.4.0 v0.4.0 'Release v0.4.0')
git_in tag -a v0.4.0 "$released" -m 'EggDB v0.4.0'
git_in push -q origin v0.4.0

sha=$(commit 0.4.0 '' 'A fix with no trailer')
expect 'no trailer, version tagged: nothing to do' ok '' "$(run "$sha")"

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nWhat it does.\n\nRelease: v0.5.0')")
expect 'trailer in the last paragraph' ok "$(printf 'tag=v0.5.0\ntagged=false')" "$(run "$sha")"

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: v0.5.0\n\nBuilt-by: worker\nCo-authored-by: A <a@example.com>')")
expect 'trailer beside a separate trailer paragraph' ok "$(printf 'tag=v0.5.0\ntagged=false')" "$(run "$sha")"

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\r\n\r\nRelease: v0.5.0\r\n\r\n')")
expect 'carriage returns and trailing blank lines' ok "$(printf 'tag=v0.5.0\ntagged=false')" "$(run "$sha")"

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: v0.5.0 is in the middle\n\nMore text.\n\nAnd more.')")
expect 'Release: in an earlier paragraph is ignored, version untagged fails loudly' failed '' "$(run "$sha")" 'has no tag v0.5.0'

sha=$(commit 0.5.0 v0.5.0 'A fix with no trailer on an untagged version')
expect 'untagged version without a trailer fails loudly' failed '' "$(run "$sha")" 'has no tag v0.5.0'

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: v0.5.0\nRelease: v0.5.0')")
expect 'two trailers' failed '' "$(run "$sha")" 'has 2 Release: lines'

for bad in v0.5 v05.0.0 v0.5.0-rc1 'v0.5.0;rm -rf x' '$(touch pwned)' '`touch pwned`' 0.5.0; do
  sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: %s' "$bad")")
  expect "malformed trailer $bad" failed '' "$(run "$sha")" 'is not vMAJOR.MINOR.PATCH'
done
if [ -e "$work/repo/pwned" ] || [ -e pwned ]; then
  echo "FAIL a trailer ran a command"
  failures=$((failures + 1))
fi

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: v0.6.0')")
expect 'trailer and version differ' failed '' "$(run "$sha")" 'but its workspace version is 0.5.0'

git_in rm -q docs/releases/v0.5.0.md
sha=$(commit 0.5.0 '' "$(printf 'Batch 7\n\nRelease: v0.5.0')")
expect 'missing notes file' failed '' "$(run "$sha")" 'has no docs/releases/v0.5.0.md'

sha=$(commit 0.5.0 v0.5.0 "$(printf 'Batch 7\n\nRelease: v0.5.0')")
git_in tag -a v0.5.0 "$sha" -m 'EggDB v0.5.0'
git_in push -q origin v0.5.0
expect 'tag already at this commit' ok "$(printf 'tag=v0.5.0\ntagged=true')" "$(run "$sha")"

other=$(commit 0.5.0 v0.5.0 "$(printf 'Fix\n\nRelease: v0.5.0')")
expect 'tag already at another commit' failed '' "$(run "$other")" 'already tags'

later=$(commit 0.5.0 v0.5.0 'After the release, no trailer')
expect 'tagged version without a trailer: nothing to do' ok '' "$(run "$later")"

if [ "$failures" -ne 0 ]; then
  echo "test-tag-from-trailer: $failures failed"
  exit 1
fi
echo "test-tag-from-trailer: every case passed"
