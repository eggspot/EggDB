#!/bin/sh
set -eu

repository="${EGGDB_REPOSITORY:-eggspot/EggDB}"
base="${EGGDB_DOWNLOAD_BASE:-https://github.com/$repository/releases/download}"
install_dir="${EGGDB_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    printf 'eggdb install: %s\n' "$1" >&2
    exit 1
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${GITHUB_TOKEN:-}" ] && [ "${1#https://api.github.com/}" != "$1" ]; then
            curl --proto '=https' --tlsv1.2 -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" "$1" -o "$2"
        else
            curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"
        fi
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "needs curl or wget to download"
    fi
}

target() {
    os=$(uname -s)
    machine=$(uname -m)
    case "$machine" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "no release build for the $machine processor yet; build from source with cargo" ;;
    esac
    case "$os" in
        Linux) printf '%s-unknown-linux-musl' "$arch" ;;
        Darwin) printf '%s-apple-darwin' "$arch" ;;
        *) fail "no release build for $os yet; on Windows run install.ps1" ;;
    esac
}

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "needs sha256sum or shasum to check the download"
    fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

version="${EGGDB_VERSION:-}"
if [ -z "$version" ]; then
    fetch "https://api.github.com/repos/$repository/releases/latest" "$work/latest.json"
    version=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$work/latest.json" | head -n 1)
    [ -n "$version" ] || fail "could not read the latest release's version"
fi
version="${version#v}"

package="eggdb-v$version-$(target)"
fetch "$base/v$version/$package.tar.gz" "$work/$package.tar.gz"
fetch "$base/v$version/SHA256SUMS" "$work/SHA256SUMS"
expected=$(grep " $package.tar.gz\$" "$work/SHA256SUMS" | cut -d ' ' -f 1)
[ -n "$expected" ] || fail "SHA256SUMS lists no $package.tar.gz"
[ "$(checksum "$work/$package.tar.gz")" = "$expected" ] || fail "the download's checksum does not match SHA256SUMS"

tar -xzf "$work/$package.tar.gz" -C "$work"
mkdir -p "$install_dir"
cp "$work/$package/eggdb" "$install_dir/eggdb.new"
chmod 755 "$install_dir/eggdb.new"
mv -f "$install_dir/eggdb.new" "$install_dir/eggdb"

printf 'eggdb %s installed in %s\n' "$version" "$install_dir"
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'add it to your PATH: export PATH="%s:$PATH"\n' "$install_dir" ;;
esac
