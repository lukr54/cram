#!/bin/sh
# Cram installer: fetch the latest Linux or macOS release binary and drop it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/lukr54/cram/main/install.sh | sh
#
# POSIX sh (not bash) on purpose, so it runs the same on a minimal container as on a full desktop. It
# installs the `cram` and `cram-extract` binaries (no daemon, no root, no system files touched) into
# ~/.local/bin (or $CRAM_INSTALL_DIR if you set one). Re-running it upgrades in place. To uninstall:
# delete the binaries it prints at the end. Set CRAM_VERSION=vX.Y.Z to pin a specific release.
set -eu

REPO="lukr54/cram"
BIN="cram"
# `cram make-sfx` builds a self-extractor by prepending this stub, and it looks for it beside the
# `cram` binary. Installing one without the other leaves that verb dead on the documented path.
STUB="cram-extract"
INSTALL_DIR="${CRAM_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*"; }
err()  { printf 'cram-install: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- platform check -------------------------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
# Each platform's release job publishes its checksums under its own name, so there is no one
# `SHA256SUMS` to ask for; asking for the wrong one gets a file that does not list this asset.
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu"; sums_name="SHA256SUMS" ;;
      *) err "no prebuilt Linux binary for '$arch' yet. Build from source: https://github.com/$REPO#building-from-source" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin"; sums_name="SHA256SUMS.macos" ;;
      *) err "no prebuilt macOS binary for '$arch' yet (Apple Silicon only). Build from source: https://github.com/$REPO#building-from-source" ;;
    esac ;;
  *) err "this installer is for Linux and macOS; on Windows use the .zip from the Releases page." ;;
esac

# --- pick a downloader ----------------------------------------------------------------------------
# --proto/--proto-redir keep the whole exchange on https, so a redirect can never move the download
# to a plain-http hop. wget has no non-recursive equivalent; the URLs are literal https either way.
if   have curl; then dl() { curl -fsSL --proto '=https' --proto-redir '=https' "$1"; }
elif have wget; then dl() { wget -qO- "$1"; }
else err "need curl or wget installed."
fi

# --- pick a hasher --------------------------------------------------------------------------------
# `sha256sum` is GNU coreutils; stock macOS ships `shasum` instead and nothing else.
if   have sha256sum; then sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif have shasum;    then sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else err "need sha256sum or shasum to verify the download."
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

# The checksum is served from the same origin as the payload, so it proves transport integrity, not
# authenticity. That is still worth stopping for: a truncated tarball that unpacks far enough to
# install half a binary is worse than no install at all. Fatal, therefore, rather than best-effort.
# `cram update` refuses on the same grounds, so the two install paths agree.
sums="$(dl "https://github.com/$REPO/releases/download/${tag}/${sums_name}")" && [ -n "$sums" ] \
  || err "release $tag publishes no readable ${sums_name}; refusing to install an unverified binary."
# The dots in the asset name are regex metacharacters to sed; escape them so a near-miss name in the
# file cannot hand over another artefact's hash.
asset_re="$(printf '%s' "$asset" | sed 's/\./\\./g')"
want="$(printf '%s\n' "$sums" | sed -n "s/^\([0-9a-fA-F]\{64\}\)[ *][ *]*${asset_re}\$/\1/p" | head -n1)"
[ -n "$want" ] || err "${sums_name} does not list ${asset}; refusing to install an unverified binary."
got="$(sha256 "$tmp/pkg.tar.gz")"
[ "$got" = "$want" ] || err "checksum mismatch for $asset (expected $want, got $got)"
say "Checksum verified."

tar -xzf "$tmp/pkg.tar.gz" -C "$tmp" || err "could not unpack $asset"
mkdir -p "$INSTALL_DIR" || err "could not create $INSTALL_DIR"

for b in "$BIN" "$STUB"; do
  src="$(find "$tmp" -type f -name "$b" | head -n1)"
  [ -n "$src" ] || err "the archive did not contain a '$b' binary."
  install -m 0755 "$src" "$INSTALL_DIR/$b" 2>/dev/null \
    || { cp "$src" "$INSTALL_DIR/$b" && chmod 0755 "$INSTALL_DIR/$b"; } \
    || err "could not install '$b' into $INSTALL_DIR"
done

say ""
say "Installed $BIN $tag -> $INSTALL_DIR/$BIN"
say "          $STUB -> $INSTALL_DIR/$STUB"

# macOS quarantines anything downloaded, and Gatekeeper then refuses to run an unsigned binary. The
# attribute is only a download marker, so clearing it on the file we just fetched is exactly what the
# user would otherwise be told to do by hand. Nothing else on the system is touched.
if [ "$os" = "Darwin" ] && have xattr; then
  for b in "$BIN" "$STUB"; do
    xattr -d com.apple.quarantine "$INSTALL_DIR/$b" 2>/dev/null || true
  done
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
