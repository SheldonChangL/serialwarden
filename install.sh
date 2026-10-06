#!/usr/bin/env sh
# serialwarden install script — macOS and Linux.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/SheldonChangL/serialwarden/main/install.sh | sh
#
# What it does, in order:
#   1. Pick the prebuilt release asset for this machine:
#        macOS  arm64 / x86_64   -> aarch64-apple-darwin / x86_64-apple-darwin
#        Linux  x86_64 / aarch64 -> x86_64-unknown-linux-gnu / aarch64-unknown-linux-gnu
#      download it plus the release's SHA256SUMS, verify the checksum, and
#      install the binary. A checksum mismatch aborts the install; it never
#      falls through to anything else.
#   2. Only if there is no prebuilt asset to use (no published release yet,
#      an unsupported OS/arch, or the download itself failed), fall back to
#      building from source: clone the repo, build the web frontend, then
#      `cargo build --release`. This fallback needs `git`, `cargo`, and
#      `node`+`npm` already on PATH (see README.md, "Building from source");
#      this script never installs a Rust or Node toolchain on your behalf.
#   3. Print (not run) the Linux permission steps — dialout group membership
#      and the optional udev rule — since both need sudo, and a piped install
#      script must not escalate privileges on its own.
#
# Everything installs under $SERIALWARDEN_PREFIX (default: $HOME/.local); no
# sudo is needed for the binary itself.
#
# Environment overrides:
#   SERIALWARDEN_PREFIX         install prefix (default: $HOME/.local)
#   SERIALWARDEN_VERSION        release tag to install, e.g. v0.1.0 (default: latest)
#   SERIALWARDEN_DOWNLOAD_BASE  base URL holding <tag>/<asset> (default: this
#                             repo's GitHub release downloads; useful for a
#                             mirror, or for testing this script locally)
#   SERIALWARDEN_FROM_SOURCE=1  skip the prebuilt download and build from source
#
# Release assets are named `serialwarden-<tag>-<target-triple>.tar.gz` by
# `.github/workflows/release.yml` — if that naming changes, update both
# places together.
set -eu

REPO="SheldonChangL/serialwarden"
PREFIX="${SERIALWARDEN_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
# Not $PREFIX/share/serialwarden: with the default prefix that is
# ~/.local/share/serialwarden, the daemon's own data directory on Linux.
# Creating it here made the daemon think a legacy serialwrap data directory
# had already been migrated, and skip moving it.
SHARE_DIR="$PREFIX/share/doc/serialwarden"
VERSION="${SERIALWARDEN_VERSION:-latest}"
DOWNLOAD_BASE="${SERIALWARDEN_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"

log() { printf 'serialwarden-install: %s\n' "$*" >&2; }
die() {
    log "$*"
    exit 1
}

OS="$(uname -s)"
ARCH="$(uname -m)"
TARGET_TRIPLE=""
case "$OS" in
Darwin)
    case "$ARCH" in
    arm64 | aarch64) TARGET_TRIPLE="aarch64-apple-darwin" ;;
    x86_64) TARGET_TRIPLE="x86_64-apple-darwin" ;;
    esac
    ;;
Linux)
    case "$ARCH" in
    x86_64 | amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
    aarch64 | arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
    esac
    ;;
*)
    die "unsupported OS '$OS' — serialwarden supports macOS and Linux"
    ;;
esac

mkdir -p "$BIN_DIR"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        return 1
    fi
}

# Resolve "latest" to a concrete tag by following GitHub's
# /releases/latest redirect (no API call, so no API rate limit). With no
# published release the redirect lands on /releases rather than /tag/<x>.
resolve_tag() {
    if [ "$VERSION" != "latest" ]; then
        printf '%s\n' "$VERSION"
        return 0
    fi
    effective=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null) || return 1
    case "$effective" in
    */releases/tag/*) printf '%s\n' "${effective##*/}" ;;
    *) return 1 ;;
    esac
}

# Returns 0 on success, 1 when no prebuilt binary could be fetched (the
# caller then builds from source). A checksum mismatch exits the script.
try_prebuilt_release() {
    if [ -z "$TARGET_TRIPLE" ]; then
        log "no prebuilt release for $OS/$ARCH"
        return 1
    fi
    command -v curl >/dev/null 2>&1 || {
        log "curl not found, skipping the prebuilt download"
        return 1
    }
    tag=$(resolve_tag) || {
        log "no published GitHub release found (or GitHub unreachable)"
        return 1
    }
    asset="serialwarden-${tag}-${TARGET_TRIPLE}.tar.gz"
    tmp=$(mktemp -d)
    log "downloading ${asset} (${tag})"
    if ! curl -fsSL "$DOWNLOAD_BASE/$tag/$asset" -o "$tmp/$asset"; then
        log "download failed (this release may not have a $TARGET_TRIPLE asset)"
        rm -rf "$tmp"
        return 1
    fi
    if curl -fsSL "$DOWNLOAD_BASE/$tag/SHA256SUMS" -o "$tmp/SHA256SUMS"; then
        # Match on the file's basename, so a SHA256SUMS line written as
        # `<hash>  <name>`, `<hash> *<name>`, or `<hash>  dist/<name>` all work.
        expected=$(awk -v a="$asset" '{f=$2; sub(/^\*/, "", f); n=split(f, p, "/"); if (p[n] == a) {print $1; exit}}' "$tmp/SHA256SUMS")
        [ -n "$expected" ] || die "SHA256SUMS for $tag has no entry for $asset — refusing to install an unverified binary"
        actual=$(sha256_of "$tmp/$asset") || die "neither sha256sum nor shasum is available to verify the download"
        [ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual) — aborting"
        log "checksum verified"
    else
        die "could not download SHA256SUMS for $tag — refusing to install an unverified binary"
    fi
    # This function runs as an `if` condition, where `set -e` is suspended,
    # so every step past the checksum check fails loudly on its own.
    tar -xzf "$tmp/$asset" -C "$tmp" || die "failed to extract $asset"
    dir="$tmp/serialwarden-${tag}-${TARGET_TRIPLE}"
    install -m 0755 "$dir/serialwarden" "$BIN_DIR/serialwarden" || die "failed to install to $BIN_DIR/serialwarden"
    if [ -f "$dir/60-serialwarden.rules" ]; then
        mkdir -p "$SHARE_DIR" || die "failed to create $SHARE_DIR"
        install -m 0644 "$dir/60-serialwarden.rules" "$SHARE_DIR/60-serialwarden.rules" || die "failed to install the udev rule template"
    fi
    rm -rf "$tmp"
    log "installed serialwarden $tag to $BIN_DIR/serialwarden"
    return 0
}

build_from_source() {
    for tool in git cargo node npm; do
        command -v "$tool" >/dev/null 2>&1 || die "'$tool' not found on PATH — building from source needs Rust (rustup) and Node.js 22+; see README.md"
    done
    src=$(mktemp -d)
    log "building from source in $src (the slow path: expect a few minutes for the first cargo build)"
    git clone --depth 1 "https://github.com/$REPO.git" "$src/serialwarden"
    (
        cd "$src/serialwarden/webui"
        npm ci
        npm run build
    )
    (
        cd "$src/serialwarden"
        cargo build --release -p serialwarden
    )
    install -m 0755 "$src/serialwarden/target/release/serialwarden" "$BIN_DIR/serialwarden"
    if [ "$OS" = "Linux" ]; then
        mkdir -p "$SHARE_DIR"
        install -m 0644 "$src/serialwarden/packaging/linux/60-serialwarden.rules" "$SHARE_DIR/60-serialwarden.rules"
    fi
    rm -rf "$src"
    log "built and installed to $BIN_DIR/serialwarden"
}

if [ "${SERIALWARDEN_FROM_SOURCE:-0}" = "1" ] || ! try_prebuilt_release; then
    build_from_source
fi

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) log "note: $BIN_DIR is not on your PATH — add 'export PATH=\"$BIN_DIR:\$PATH\"' to your shell rc file" ;;
esac

if [ "$OS" = "Linux" ]; then
    cat >&2 <<EOF
serialwarden-install: binary installed. Two permission steps this script does
NOT run for you (both need sudo, and a piped install script should not
silently escalate privileges):

  1. Add yourself to the 'dialout' group, then log out and back in:
       sudo usermod -aG dialout "\$USER"

  2. (optional, only if 'serialwarden devices' can't see your adapter after
     step 1) install the udev rule template:
       sudo cp $SHARE_DIR/60-serialwarden.rules /etc/udev/rules.d/
       sudo udevadm control --reload-rules && sudo udevadm trigger

EOF
fi

cat >&2 <<EOF
Next:
  serialwarden daemon            # run in the foreground to watch it start, or
  serialwarden service install   # start it at login (launchd / systemd --user)
Then open http://127.0.0.1:5590 for the web UI (served by the daemon itself).
EOF
