#!/usr/bin/env sh
# Cross-platform installer for Pockets (Linux & macOS).
# Installs Podman automatically via the OS package manager when missing,
# then downloads the matching prebuilt `pockets` binary from GitHub Releases.
set -eu

REPO="${POCKETS_REPO:-runpockets/pockets}"
INSTALL_DIR="${POCKETS_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "Unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    if command -v brew >/dev/null 2>&1; then
      echo "Homebrew detected — installing Pockets and Podman via Homebrew..."
      brew install "${POCKETS_BREW_TAP:-runpockets/tap}/pockets"
      exit 0
    fi
    case "$ARCH" in
      arm64) TARGET="aarch64-apple-darwin" ;;
      x86_64) TARGET="x86_64-apple-darwin" ;;
      *) echo "Unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (use Windows installer or cargo install)" >&2
    exit 1
    ;;
esac

if ! command -v podman >/dev/null 2>&1; then
  echo "Podman not found — installing Podman..."
  if [ "$OS" = "Linux" ]; then
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update && sudo apt-get install -y podman
    elif command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y podman
    elif command -v pacman >/dev/null 2>&1; then
      sudo pacman -Sy --noconfirm podman
    else
      echo "Please install Podman via your distro package manager, then re-run this script." >&2
      exit 1
    fi
  else
    echo "Install Homebrew (https://brew.sh) and run: brew install ${POCKETS_BREW_TAP:-runpockets/tap}/pockets" >&2
    exit 1
  fi
fi

VERSION="${POCKETS_VERSION:-$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)}"
if [ -z "$VERSION" ]; then
  echo "Could not determine latest Pockets release version." >&2
  exit 1
fi

URL="https://github.com/$REPO/releases/download/$VERSION/pockets-$VERSION-$TARGET.tar.gz"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading Pockets $VERSION ($TARGET)..."
curl -fsSL "$URL" -o "$TMP_DIR/pockets.tar.gz"
tar -xzf "$TMP_DIR/pockets.tar.gz" -C "$TMP_DIR"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$TMP_DIR/pockets" "$INSTALL_DIR/pockets"

echo "Installed pockets to $INSTALL_DIR/pockets"
if [ "$OS" = "Linux" ] && [ "${POCKETS_NO_SERVICE:-0}" != "1" ] && command -v systemctl >/dev/null 2>&1; then
  echo "Enabling Pockets user background service (set POCKETS_NO_SERVICE=1 to skip)..."
  "$INSTALL_DIR/pockets" service install || true
fi
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add $INSTALL_DIR to your PATH, then run: pockets" ;;
esac
