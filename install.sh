#!/bin/sh
# Install search, the web search CLI for AI agents.
# Usage: curl -fsSL https://metasearch.sh/install | sh
#    or: curl -fsSL https://raw.githubusercontent.com/paperfoot/search-cli/master/install.sh | sh
# Prebuilt binaries: macOS and Linux (glibc), x86_64 and arm64.
set -eu

REPO="paperfoot/search-cli"
BINARY="search"

fail() {
  echo "$1" >&2
  exit 1
}

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  OS_TAG="unknown-linux-gnu" ;;
  Darwin) OS_TAG="apple-darwin" ;;
  *)      fail "Unsupported OS: $OS. Install from source: cargo install agent-search" ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_TAG="x86_64" ;;
  aarch64|arm64) ARCH_TAG="aarch64" ;;
  *)             fail "Unsupported architecture: $ARCH. Install from source: cargo install agent-search" ;;
esac

TARGET="${ARCH_TAG}-${OS_TAG}"
ASSET="${BINARY}-${TARGET}.tar.gz"
# GitHub redirects latest/download to the newest release, with no API call
# (the API allows 60 unauthenticated requests an hour per IP).
BASE="https://github.com/${REPO}/releases/latest/download"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

echo "Downloading ${ASSET}..."
curl -fsSL "${BASE}/${ASSET}" -o "${WORK}/${ASSET}" \
  || fail "Download failed: ${BASE}/${ASSET}. Install from source: cargo install agent-search"

# Verify against the release's SHA256SUMS before extracting anything.
curl -fsSL "${BASE}/SHA256SUMS" -o "${WORK}/SHA256SUMS" \
  || fail "The release has no SHA256SUMS file; refusing an unverified install. Install from source: cargo install agent-search"

EXPECTED="$(awk -v f="$ASSET" '$2 == f || $2 == "*" f { print $1 }' "${WORK}/SHA256SUMS")"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${WORK}/${ASSET}" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${WORK}/${ASSET}" | awk '{ print $1 }')"
else
  fail "Neither sha256sum nor shasum is installed, so the download cannot be verified."
fi
if [ -z "$EXPECTED" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
  fail "Checksum mismatch for ${ASSET} (expected ${EXPECTED:-none}, got ${ACTUAL})."
fi

tar -xzf "${WORK}/${ASSET}" -C "$WORK"

# Install to ~/.local/bin if it exists, else /usr/local/bin if writable,
# else create ~/.local/bin.
if [ -d "$HOME/.local/bin" ]; then
  INSTALL_DIR="$HOME/.local/bin"
elif [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

mv "${WORK}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
chmod +x "${INSTALL_DIR}/${BINARY}"

echo ""
echo "Installed ${BINARY} to ${INSTALL_DIR}/${BINARY}"
echo ""

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "Add it to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    ;;
esac

echo "Get started:"
echo "  search login              # no keys needed; new accounts get \$1 of credit"
echo "  search \"your question\""
echo "  search config check       # or use your own provider keys"
