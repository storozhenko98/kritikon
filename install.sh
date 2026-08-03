#!/usr/bin/env sh
set -eu

repo="storozhenko98/kritikon"
version="${KRITIKON_VERSION:-latest}"
install_dir="${KRITIKON_INSTALL_DIR:-${HOME}/.local/bin}"

fail() {
    printf 'kritikon: %s\n' "$1" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

fetch() {
    source_url="$1"
    destination="$2"
    if has curl; then
        curl --proto '=https' --tlsv1.2 -fsSL "$source_url" -o "$destination"
    elif has wget; then
        wget -q "$source_url" -O "$destination"
    else
        fail "curl or wget is required"
    fi
}

os="$(uname -s)"
arch="$(uname -m)"

case "$os:$arch" in
    Darwin:arm64|Darwin:aarch64)
        archive="kritikon-macos-aarch64.tar.gz"
        ;;
    Darwin:*)
        fail "macOS releases require Apple Silicon (arm64); Intel Macs are not supported"
        ;;
    Linux:x86_64|Linux:amd64)
        archive="kritikon-linux-x86_64.tar.gz"
        ;;
    Linux:aarch64|Linux:arm64)
        archive="kritikon-linux-aarch64.tar.gz"
        ;;
    Linux:*)
        fail "unsupported Linux architecture: $arch"
        ;;
    *)
        fail "unsupported operating system: $os"
        ;;
esac

if [ "$version" = "latest" ]; then
    release_url="https://github.com/${repo}/releases/latest/download"
else
    case "$version" in
        v*) tag="$version" ;;
        *) tag="v$version" ;;
    esac
    release_url="https://github.com/${repo}/releases/download/${tag}"
fi

temp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t kritikon)"
trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

printf 'Downloading Kritikon for %s/%s…\n' "$os" "$arch"
fetch "$release_url/$archive" "$temp_dir/$archive"
fetch "$release_url/SHA256SUMS" "$temp_dir/SHA256SUMS"

expected="$(awk -v file="$archive" '$2 == file || $2 == "*" file { print $1; exit }' "$temp_dir/SHA256SUMS")"
[ -n "$expected" ] || fail "release checksum is missing for $archive"

if has sha256sum; then
    actual="$(sha256sum "$temp_dir/$archive" | awk '{print $1}')"
elif has shasum; then
    actual="$(shasum -a 256 "$temp_dir/$archive" | awk '{print $1}')"
else
    fail "sha256sum or shasum is required to verify the download"
fi

[ "$actual" = "$expected" ] || fail "download checksum verification failed"

LC_ALL=C tar -xzf "$temp_dir/$archive" -C "$temp_dir"
[ -f "$temp_dir/kritikon" ] || fail "release archive does not contain the kritikon binary"

mkdir -p "$install_dir"
install -m 0755 "$temp_dir/kritikon" "$install_dir/kritikon"

printf 'Installed Kritikon to %s/kritikon\n' "$install_dir"
case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
        printf 'Add %s to PATH, then run: kritikon\n' "$install_dir"
        ;;
esac

if ! has gh; then
    printf 'GitHub CLI is also required: https://cli.github.com/\n'
fi
