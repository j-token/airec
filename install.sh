#!/bin/sh
# Installs the airec screen recorder CLI on Linux x86_64.
#
#   curl -fsSL https://raw.githubusercontent.com/j-token/airec/main/install.sh | sh
#
# Downloads a prebuilt airec from the GitHub releases of j-token/airec, verifies its SHA256,
# and places it in ~/.local/bin. No root and no Rust toolchain are needed.
#
#   AIREC_VERSION=v0.3.0        install that release instead of the latest one
#   AIREC_INSTALL_DIR=/opt/bin  install somewhere else
#   sh -s -- --uninstall        remove a previous installation
set -eu

repo="j-token/airec"
target="x86_64-unknown-linux-gnu"
version="${AIREC_VERSION:-latest}"
install_dir="${AIREC_INSTALL_DIR:-$HOME/.local/bin}"
exe="$install_dir/airec"

step() { echo "==> $*"; }
die() {
    echo "error: $*" >&2
    exit 1
}

if [ "${1:-}" = "--uninstall" ]; then
    if [ -e "$exe" ]; then
        rm -f "$exe"
        step "removed $exe"
    fi
    echo "airec uninstalled. Recordings and airec.toml were left untouched."
    exit 0
fi

[ "$(uname -s)" = "Linux" ] || die "install.sh is for Linux; on Windows use install.ps1."
[ "$(uname -m)" = "x86_64" ] || die "airec ships only an x86_64 Linux build; this machine reports $(uname -m)."
command -v curl >/dev/null || die "curl is required."
command -v sha256sum >/dev/null || die "sha256sum is required."

if [ "$version" = "latest" ]; then
    step "resolving latest"
    # The latest release page redirects to .../releases/tag/<tag>.
    latest_url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
        die "could not resolve the latest release of $repo."
    tag=${latest_url##*/}
else
    tag=$version
fi

asset="airec-$tag-$target.tar.gz"
base_url="https://github.com/$repo/releases/download/$tag"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

step "downloading $tag"
curl -fsSL -o "$work/$asset" "$base_url/$asset" ||
    die "release $tag has no asset named $asset. See https://github.com/$repo/releases."

step "verifying checksum"
curl -fsSL -o "$work/$asset.sha256" "$base_url/$asset.sha256" ||
    die "release $tag publishes no $asset.sha256. The download was not installed."
expected=$(cut -d ' ' -f 1 "$work/$asset.sha256")
actual=$(sha256sum "$work/$asset" | cut -d ' ' -f 1)
[ "$expected" = "$actual" ] ||
    die "checksum mismatch for $asset (expected $expected, got $actual). The download was not installed."

tar -xzf "$work/$asset" -C "$work"
extracted="$work/airec-$tag-$target/airec"
[ -f "$extracted" ] || die "the archive did not contain airec."

mkdir -p "$install_dir"
install -m 755 "$extracted" "$exe"
step "installed $exe"

"$exe" --version
case ":$PATH:" in
*":$install_dir:"*) ;;
*) echo "$install_dir is not on PATH. Add it in your shell profile, for example: export PATH=\"$install_dir:\$PATH\"" ;;
esac
echo "Run 'airec doctor --json' in an X11 session to confirm capture and encoding work on this machine."
