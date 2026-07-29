#!/bin/sh
# Cram installer: fetch the latest Linux or macOS release binary and drop it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/lukr54/cram/main/install.sh | sh
#
# POSIX sh (not bash) on purpose, so it runs the same on a minimal container as on a full desktop. It
# installs ONLY the `cram` binary (no daemon, no root, no system files touched) into ~/.local/bin
# (or $CRAM_INSTALL_DIR if you set one). Re-running it upgrades in place. To uninstall: delete the
# binary it prints at the end. Set CRAM_VERSION=vX.Y.Z to pin a specific release.
set -eu

REPO="lukr54/cram"
BIN="cram"
INSTALL_DIR="${CRAM_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*"; }
err()  { printf 'cram-install: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- platform check -------------------------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "no prebuilt Linux binary for '$arch' yet. Build from source: https://github.com/$REPO#build-from-source" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) err "no prebuilt macOS binary for '$arch' yet (Apple Silicon only). Build from source: https://github.com/$REPO#build-from-source" ;;
    esac ;;
  *) err "this installer is for Linux and macOS; on Windows use the .zip from the Releases page." ;;
esac

# --- pick a downloader ----------------------------------------------------------------------------
if   have curl; then dl() { curl -fsSL "$1"; }
elif have wget; then dl() { wget -qO- "$1"; }
else err "need curl or wget installed."
fi

# --- resolve the release tag ----------------------------------------------------------------------
tag="${CRAM_VERSION:-}"
if [ -z "$tag" ]; then
  say "Finding the latest Cram release..."
  # Read the tag from the GitHub API without needing jq: pull the first "tag_name" field.
  tag="$(dl "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$tag" ] || err "could not determine the latest release. Set CRAM_VERSION=vX.Y.Z to pin one."
fi

asset="cram-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${tag}/${asset}"

# --- download + verify + install ------------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ..."
dl "$url" > "$tmp/pkg.tar.gz" || err "download failed: $url"

# Best-effort checksum check when the release ships a SHA256SUMS file (never fatal if absent).
if sums="$(dl "https://github.com/$REPO/releases/download/${tag}/SHA256SUMS" 2>/dev/null)" && [ -n "$sums" ]; then
  want="$(printf '%s\n' "$sums" | sed -n "s/ .*\\/${asset}\$//p;s/  *${asset}\$//p" | head -n1)"
  if [ -n "$want" ] && have sha256sum; then
    got="$(sha256sum "$tmp/pkg.tar.gz" | cut -d' ' -f1)"
    [ "$got" = "$want" ] || err "checksum mismatch for $asset (expected $want, got $got)"
    say "Checksum verified."
  fi
fi

tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
src="$(find "$tmp" -type f -name "$BIN" | head -n1)"
[ -n "$src" ] || err "the archive did not contain a '$BIN' binary."

mkdir -p "$INSTALL_DIR"
install -m 0755 "$src" "$INSTALL_DIR/$BIN" 2>/dev/null || { cp "$src" "$INSTALL_DIR/$BIN" && chmod 0755 "$INSTALL_DIR/$BIN"; }

say ""
say "Installed $BIN $tag -> $INSTALL_DIR/$BIN"

# macOS quarantines anything downloaded, and Gatekeeper then refuses to run an unsigned binary. The
# attribute is only a download marker, so clearing it on the file we just fetched is exactly what the
# user would otherwise be told to do by hand. Nothing else on the system is touched.
if [ "$os" = "Darwin" ] && have xattr; then
  xattr -d com.apple.quarantine "$INSTALL_DIR/$BIN" 2>/dev/null || true
fi

"$INSTALL_DIR/$BIN" --version 2>/dev/null || true

# --- PATH hint ------------------------------------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *)
    say ""
    say "NOTE: $INSTALL_DIR is not on your PATH. Add it, e.g.:"
    say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile && . ~/.profile"
    ;;
esac
