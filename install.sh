#!/usr/bin/env bash
# cloudcode installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
#   curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
#   curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
#
# Flags:
#   --version vX.Y.Z   Pin to specific release (default: latest)
#   --prefix DIR       Install root (default: /usr/local)
set -euo pipefail

REPO="initialz/cloudcode"
COMPONENT="${1:-}"
shift || true

VERSION="latest"
PREFIX="/usr/local"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift ;;
    --prefix)  PREFIX="$2"; shift ;;
    -h|--help)
      sed -n '2,12p' "$0" 2>/dev/null || cat <<'EOF'
Usage: install.sh {hub|agent|client} [--version vX.Y.Z] [--prefix DIR]
EOF
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
  shift
done

case "$COMPONENT" in
  hub|agent|client) ;;
  *)
    echo "Usage: install.sh {hub|agent|client} [flags]" >&2
    exit 1
    ;;
esac

# ---- detect platform ----
OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}-${ARCH}" in
  Linux-x86_64)        ASSET_OS=linux-x86_64 ;;
  Linux-aarch64|Linux-arm64) ASSET_OS=linux-aarch64 ;;
  Darwin-arm64)        ASSET_OS=macos-aarch64 ;;
  *)
    echo "unsupported platform: ${OS}-${ARCH}" >&2
    echo "supported: Linux x86_64, Linux aarch64, macOS arm64" >&2
    exit 1
    ;;
esac

# ---- resolve version ----
if [ "$VERSION" = "latest" ]; then
  echo "Resolving latest release..."
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  if [ -z "$VERSION" ]; then
    echo "could not resolve latest release tag" >&2
    exit 1
  fi
fi
echo "Installing cloudcode ${VERSION} (${ASSET_OS}) → ${PREFIX}/bin"

# ---- download ----
ASSET="cloudcode-${VERSION}-${ASSET_OS}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "$URL" -o "$TMP/asset.tar.gz" || {
  echo "download failed: $URL" >&2
  exit 1
}
tar -xzf "$TMP/asset.tar.gz" -C "$TMP"
SRC="$TMP/${ASSET}"

# ---- pick sudo if needed (check writability of closest existing ancestor) ----
BIN_DIR="${PREFIX}/bin"
SUDO=""
PROBE="$BIN_DIR"
while [ ! -d "$PROBE" ] && [ "$PROBE" != "/" ] && [ "$PROBE" != "." ]; do
  PROBE="$(dirname "$PROBE")"
done
if [ ! -w "$PROBE" ] && [ "$(id -u)" != "0" ]; then SUDO="sudo"; fi
$SUDO mkdir -p "$BIN_DIR"

install_bin() {
  local name="$1"
  echo "  → $BIN_DIR/$name"
  $SUDO install -m 755 "$SRC/$name" "$BIN_DIR/$name"
}

# ---- run ----
case "$COMPONENT" in
  hub)
    install_bin cloudcode-hub

    cat <<EOF

Hub installed.

Next steps:
  1) Generate a token for a user:
       cloudcode-hub gen-token alice

  2) Create hub.toml from the example, paste your Anthropic API key
     and the token hash from step 1:
       cp $SRC/hub.example.toml ./hub.toml
       \$EDITOR ./hub.toml

  3) Start the hub daemon (logs → ~/.local/state/cloudcode/hub.log):
       cloudcode-hub daemon start --config ./hub.toml
       cloudcode-hub daemon status
       tail -f ~/.local/state/cloudcode/hub.log

     Other lifecycle commands:
       cloudcode-hub daemon stop
       cloudcode-hub daemon restart --config ./hub.toml
EOF
    ;;

  agent)
    install_bin cloudcode-agent

    cat <<EOF

Agent installed.

Next steps:
  1) Generate a shared secret for hub<->agent auth:
       cloudcode-agent gen-secret

  2) On any workstation that can open a browser, log in to claude
     (claude code uses an OAuth PKCE flow that needs a browser):
       claude            # then run /login inside, complete OAuth
       # the credentials end up in ~/.claude/.credentials.json

  3) Copy the credentials onto this server (path is up to you, just
     point agent.toml's [claude].credentials_path at it):
       scp ~/.claude/.credentials.json THIS-SERVER:~/.claude-credentials.json
       chmod 600 ~/.claude-credentials.json

  4) Create agent.toml from the example, paste the shared_secret_hash
     from step 1 and the credentials_path from step 3:
       cp $SRC/agent.example.toml ./agent.toml
       \$EDITOR ./agent.toml

  5) Start the agent daemon (logs → ~/.local/state/cloudcode/agent.log):
       cloudcode-agent daemon start --config ./agent.toml
       cloudcode-agent daemon status
       tail -f ~/.local/state/cloudcode/agent.log

  6) On the hub host, add this agent to hub.toml:
       [[agents]]
       name = "this-agent"
       url = "http://THIS-SERVER:7100"
       shared_secret = "<plaintext secret from step 1>"
EOF
    ;;

  client)
    install_bin cloudcode

    cat <<EOF

Client installed.

Next steps:
  1) Create ~/.config/cloudcode/config.toml:
       mkdir -p ~/.config/cloudcode
       cat > ~/.config/cloudcode/config.toml <<'CFG'
       hub_url = "http://YOUR-HUB-HOST:7000"
       token   = "cc_xxx_from_admin"
       CFG

  2) Run any supported AI CLI through the hub:
       cd ~/code/myproject
       cloudcode run claude
EOF
    ;;
esac
