#!/bin/sh
# Install the witnessed binary from a GitHub release.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Druidder-Labs/witnessed/main/install.sh | sh
#   curl -fsSL ... | WITNESSED_VERSION=v0.4.0 WITNESSED_INSTALL_DIR=/usr/local/bin sh
#
# Picks the newest release (prereleases included) unless WITNESSED_VERSION is set,
# downloads the tarball for this OS and CPU, checks it against the .sha256 file attached
# to the same release, and copies the binary to WITNESSED_INSTALL_DIR
# (default: ~/.local/bin). Needs curl or wget, tar, and shasum or sha256sum.
set -eu

repo="Druidder-Labs/witnessed"
dir="${WITNESSED_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$1"
  else
    die "curl or wget is required"
  fi
}

os=$(uname -s)
cpu=$(uname -m)
case "$os" in
  Linux) libc=unknown-linux-musl ;;
  Darwin) libc=apple-darwin ;;
  *) die "unsupported operating system: $os (Linux and macOS only)" ;;
esac
case "$cpu" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported CPU: $cpu (x86_64 and aarch64 only)" ;;
esac
target="${arch}-${libc}"

version="${WITNESSED_VERSION:-}"
if [ -z "$version" ]; then
  version=$(fetch "https://api.github.com/repos/${repo}/releases" | grep -o '"tag_name": *"[^"]*"' | head -1 | sed 's/.*"\(v[^"]*\)"/\1/')
  [ -n "$version" ] || die "could not find a release of ${repo}"
fi

name="witnessed-${version}-${target}"
base="https://github.com/${repo}/releases/download/${version}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "downloading ${name}.tar.gz"
fetch "${base}/${name}.tar.gz" > "${tmp}/${name}.tar.gz" || die "no binary for ${target} in release ${version}"
fetch "${base}/${name}.tar.gz.sha256" > "${tmp}/expected.sha256"

expected=$(awk '{print $1}' "${tmp}/expected.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${tmp}/${name}.tar.gz" | awk '{print $1}')
else
  actual=$(shasum -a 256 "${tmp}/${name}.tar.gz" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for ${name}.tar.gz (expected ${expected}, got ${actual})"

tar -xzf "${tmp}/${name}.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "${tmp}/${name}/witnessed" "${dir}/witnessed"
say "installed $("${dir}/witnessed" --version) to ${dir}/witnessed"
case ":$PATH:" in
  *":${dir}:"*) ;;
  *) say "" ; say "add it to your PATH:" ; say "  export PATH=\"${dir}:\$PATH\"" ;;
esac
say ""
say "next, record an agent session:"
say "  witnessed run -- claude"
