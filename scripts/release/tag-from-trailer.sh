#!/bin/sh
set -eu

sha=${1:?usage: tag-from-trailer.sh <commit>}
remote=${RELEASE_REMOTE:-origin}

case $sha in
  *[!0-9a-f]* | '') echo "::error::not a commit id: $sha" >&2; exit 1 ;;
esac

message=$(git log -1 --format=%B "$sha" | tr -d '\r')

trailers=$(printf '%s\n' "$message" | awk '
  NF { block = block $0 "\n"; next }
  { if (block != "") { previous = last; last = block; block = "" } }
  END {
    if (block != "") { previous = last; last = block }
    printf "%s%s", previous, last
  }' | grep -E '^Release:' || true)

count=$(printf '%s' "$trailers" | grep -c . || true)
if [ "$count" -gt 1 ]; then
  echo "::error::$sha has $count Release: lines; a release commit has exactly one" >&2
  exit 1
fi

version=$(git show "$sha:Cargo.toml" | awk '
  /^\[/ { section = $0; next }
  section == "[workspace.package]" && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }')
case $version in
  '') echo "::error::$sha has no [workspace.package] version" >&2; exit 1 ;;
esac

remote_tag() {
  git ls-remote --tags "$remote" "refs/tags/$1" "refs/tags/$1^{}" | awk '
    $2 ~ /\^\{\}$/ { peeled = $1 } $2 !~ /\^\{\}$/ { plain = $1 }
    END { if (peeled != "") print peeled; else print plain }'
}

if [ "$count" -eq 0 ]; then
  if [ -n "$(remote_tag "v$version")" ]; then
    exit 0
  fi
  echo "::error::the workspace version $version at $sha has no tag v$version and $sha has no Release: trailer; merge a PR whose squash body ends with 'Release: v$version', or run: gh workflow run release.yml -f tag=v$version after tagging by hand" >&2
  exit 1
fi

value=$(printf '%s\n' "$trailers" | sed -e 's/^Release:[[:space:]]*//' -e 's/[[:space:]]*$//')
if ! printf '%s\n' "$value" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
  echo "::error::the Release: trailer of $sha is not vMAJOR.MINOR.PATCH" >&2
  exit 1
fi
if [ "$value" != "v$version" ]; then
  echo "::error::$sha says Release: $value but its workspace version is $version" >&2
  exit 1
fi
if ! git cat-file -e "$sha:docs/releases/$value.md" 2>/dev/null; then
  echo "::error::$sha has no docs/releases/$value.md" >&2
  exit 1
fi

tagged=$(remote_tag "$value")
if [ -n "$tagged" ] && [ "$tagged" != "$(git rev-parse "$sha^{commit}")" ]; then
  echo "::error::$value already tags $tagged, not $sha" >&2
  exit 1
fi

echo "tag=$value"
if [ -n "$tagged" ]; then
  echo "tagged=true"
else
  echo "tagged=false"
fi
