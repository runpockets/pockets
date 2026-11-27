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
    echo "Unsupported OS: $OS (Pockets supports macOS and Linux; on Windows, run this installer inside WSL2)" >&2
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

ARCHIVE_NAME="pockets-$VERSION-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$ARCHIVE_NAME"
SUMS_URL="https://github.com/$REPO/releases/download/$VERSION/SHA256SUMS.txt"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading Pockets $VERSION ($TARGET)..."
curl -fsSL "$URL" -o "$TMP_DIR/$ARCHIVE_NAME"
curl -fsSL "$SUMS_URL" -o "$TMP_DIR/SHA256SUMS.txt"

EXPECTED_SHA="$(awk -v name="$ARCHIVE_NAME" '
  NF >= 2 {
    file = $2
    sub(/^\*/, "", file)
    if (file == name && length($1) == 64 && $1 ~ /^[0-9A-Fa-f]+$/) {
      print tolower($1)
      exit
    }
  }
' "$TMP_DIR/SHA256SUMS.txt")"

if [ -z "$EXPECTED_SHA" ]; then
  echo "Could not find a valid SHA-256 checksum for $ARCHIVE_NAME in SHA256SUMS.txt." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA="$(sha256sum "$TMP_DIR/$ARCHIVE_NAME" | awk '{print tolower($1)}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 "$TMP_DIR/$ARCHIVE_NAME" | awk '{print tolower($1)}')"
else
  echo "Neither sha256sum nor shasum is available to verify $ARCHIVE_NAME." >&2
  exit 1
fi

if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
  echo "Checksum mismatch for $ARCHIVE_NAME!" >&2
  echo "  expected: $EXPECTED_SHA" >&2
  echo "  actual:   $ACTUAL_SHA" >&2
  exit 1
fi

echo "Verified SHA-256 checksum for $ARCHIVE_NAME."
tar -xzf "$TMP_DIR/$ARCHIVE_NAME" -C "$TMP_DIR"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$TMP_DIR/pockets" "$INSTALL_DIR/pockets"

echo "Installed pockets to $INSTALL_DIR/pockets"
if [ "$OS" = "Linux" ]; then
  CURRENT_USER="${USER:-$(id -un)}"

  # Allow non-root Pockets to manage private `tailscale serve` HTTPS routes on Linux.
  if [ "$(id -u)" -ne 0 ] && command -v tailscale >/dev/null 2>&1; then
    echo "Granting $CURRENT_USER permission to manage Tailscale Serve routes..."
    if command -v sudo >/dev/null 2>&1; then
      sudo tailscale set --operator="$CURRENT_USER" || \
        echo "Note: Run 'sudo tailscale set --operator=$CURRENT_USER' so Pockets can share apps over Tailscale."
    else
      echo "Note: Run 'sudo tailscale set --operator=$CURRENT_USER' so Pockets can share apps over Tailscale."
    fi
  fi

  if [ "${POCKETS_NO_SERVICE:-0}" != "1" ] && command -v systemctl >/dev/null 2>&1; then
    # Keep systemd --user (daemon, Tailscale portal, and rootless Podman) alive after SSH logout.
    if command -v loginctl >/dev/null 2>&1; then
      LINGER_STATE="$(loginctl show-user "$CURRENT_USER" -p Linger --value 2>/dev/null || true)"
      if [ "$LINGER_STATE" != "yes" ]; then
        echo "Enabling systemd user lingering for $CURRENT_USER..."
        if ! loginctl enable-linger "$CURRENT_USER" 2>/dev/null; then
          if command -v sudo >/dev/null 2>&1; then
            sudo loginctl enable-linger "$CURRENT_USER" || \
              echo "Note: Run 'sudo loginctl enable-linger $CURRENT_USER' so Pockets stays running after SSH disconnect."
          else
            echo "Note: Run 'sudo loginctl enable-linger $CURRENT_USER' so Pockets stays running after SSH disconnect."
          fi
        fi
      fi
    fi

    echo "Enabling Pockets user background service (set POCKETS_NO_SERVICE=1 to skip)..."
    "$INSTALL_DIR/pockets" service install || true
  fi
fi
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add $INSTALL_DIR to your PATH, then run: pockets" ;;
esac
