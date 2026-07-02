#!/bin/sh
# Install search-cli - agent-friendly multi-provider search
# Usage: curl -fsSL https://raw.githubusercontent.com/paperfoot/search-cli/master/install.sh | sh
set -e

REPO="paperfoot/search-cli"
BINARY="search"

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  OS_TAG="unknown-linux-gnu" ;;
  Darwin) OS_TAG="apple-darwin" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64)  ARCH_TAG="x86_64" ;;
  aarch64|arm64) ARCH_TAG="aarch64" ;;
  *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

TARGET="${ARCH_TAG}-${OS_TAG}"

# Prebuilt binaries exist for macOS (both arches) and Linux x86_64.
if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then
  echo "No prebuilt binary for aarch64 Linux. Install from source:"
  echo "  cargo install agent-search --no-default-features"
  exit 1
fi
LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$LATEST" ]; then
  echo "No releases found. Install from source:"
  echo "  cargo install --git https://github.com/${REPO}"
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${BINARY}-${TARGET}.tar.gz"
echo "Downloading search ${LATEST} for ${TARGET}..."

TMPDIR=$(mktemp -d)
curl -fsSL "$URL" -o "${TMPDIR}/search.tar.gz"

# Verify against the release's SHA256SUMS before extracting anything.
SUMS_URL="https://github.com/${REPO}/releases/download/${LATEST}/SHA256SUMS"
if ! curl -fsSL "$SUMS_URL" -o "${TMPDIR}/SHA256SUMS"; then
  echo "Release ${LATEST} has no SHA256SUMS file; refusing unverified install."
  echo "Install from source instead: cargo install agent-search"
  rm -rf "$TMPDIR"
  exit 1
fi
EXPECTED=$(grep "${BINARY}-${TARGET}.tar.gz" "${TMPDIR}/SHA256SUMS" | awk '{print $1}')
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "${TMPDIR}/search.tar.gz" | awk '{print $1}')
else
  ACTUAL=$(shasum -a 256 "${TMPDIR}/search.tar.gz" | awk '{print $1}')
fi
if [ -z "$EXPECTED" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "Checksum mismatch for ${BINARY}-${TARGET}.tar.gz (expected ${EXPECTED:-none}, got ${ACTUAL})."
  rm -rf "$TMPDIR"
  exit 1
fi

tar -xzf "${TMPDIR}/search.tar.gz" -C "${TMPDIR}"

# Install to ~/.local/bin or /usr/local/bin
if [ -d "$HOME/.local/bin" ]; then
  INSTALL_DIR="$HOME/.local/bin"
elif [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

mv "${TMPDIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
chmod +x "${INSTALL_DIR}/${BINARY}"
rm -rf "$TMPDIR"

echo ""
echo "Installed search to ${INSTALL_DIR}/${BINARY}"
echo ""

# Check if in PATH
if ! command -v search >/dev/null 2>&1; then
  echo "Add to your PATH:"
  echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
  echo ""
fi

echo "Get started:"
echo "  search --help"
echo "  search config check"
echo ""
