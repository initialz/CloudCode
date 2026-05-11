#!/usr/bin/env bash
# cloudcode installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
#   curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
#
# Flags:
#   --version vX.Y.Z   Pin to specific release (default: latest)
#   --prefix DIR       Install root (default: /usr/local)
#   --no-service       Hub mode: skip systemd unit
#   --service          Hub mode: install systemd unit even if already present
set -euo pipefail

REPO="initialz/cloudcode"
COMPONENT="${1:-}"
shift || true

VERSION="latest"
PREFIX="/usr/local"
INSTALL_SERVICE="auto"   # auto: yes for hub on linux, no otherwise

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift ;;
    --prefix)  PREFIX="$2"; shift ;;
    --service) INSTALL_SERVICE=1 ;;
    --no-service) INSTALL_SERVICE=0 ;;
    -h|--help)
      sed -n '2,12p' "$0" 2>/dev/null || cat <<'EOF'
Usage: install.sh {hub|client} [--version vX.Y.Z] [--prefix DIR] [--service|--no-service]
EOF
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
  shift
done

case "$COMPONENT" in
  hub|client) ;;
  *)
    echo "Usage: install.sh {hub|client} [flags]" >&2
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

# ---- pick sudo if needed ----
BIN_DIR="${PREFIX}/bin"
SUDO=""
if [ ! -d "$BIN_DIR" ] || [ ! -w "$BIN_DIR" ]; then
  if [ "$(id -u)" != "0" ]; then SUDO="sudo"; fi
fi
$SUDO mkdir -p "$BIN_DIR"

install_bin() {
  local name="$1"
  echo "  → $BIN_DIR/$name"
  $SUDO install -m 755 "$SRC/$name" "$BIN_DIR/$name"
}

# ---- hub-specific systemd install ----
install_systemd_unit() {
  if [ "$OS" != "Linux" ]; then
    echo "systemd unit only supported on Linux; skipping" >&2
    return
  fi
  if ! command -v systemctl >/dev/null 2>&1; then
    echo "systemctl not found; skipping service install" >&2
    return
  fi

  # create user / dirs
  if ! id cloudcode >/dev/null 2>&1; then
    echo "Creating system user 'cloudcode'..."
    $SUDO useradd --system --no-create-home --shell /usr/sbin/nologin cloudcode
  fi
  $SUDO mkdir -p /etc/cloudcode /var/log/cloudcode /var/lib/cloudcode
  $SUDO chown cloudcode:cloudcode /var/log/cloudcode /var/lib/cloudcode

  # place example config (without overwriting real one)
  if [ ! -f /etc/cloudcode/hub.example.toml ]; then
    $SUDO install -m 644 "$SRC/hub.example.toml" /etc/cloudcode/hub.example.toml
  fi

  # write unit
  UNIT=/etc/systemd/system/cloudcode-hub.service
  echo "Writing $UNIT"
  $SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=Cloudcode Hub (LLM API gateway)
Documentation=https://github.com/${REPO}
After=network.target

[Service]
Type=simple
User=cloudcode
Group=cloudcode
WorkingDirectory=/var/lib/cloudcode
ExecStart=${BIN_DIR}/cloudcode-hub serve --config /etc/cloudcode/hub.toml
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/log/cloudcode /var/lib/cloudcode
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
  $SUDO systemctl daemon-reload
}

# ---- run ----
case "$COMPONENT" in
  hub)
    install_bin cloudcode-hub
    install_bin cloudcode    # bundled, harmless

    DO_SERVICE=0
    if [ "$INSTALL_SERVICE" = "auto" ] && [ "$OS" = "Linux" ]; then DO_SERVICE=1; fi
    if [ "$INSTALL_SERVICE" = "1" ]; then DO_SERVICE=1; fi
    if [ "$INSTALL_SERVICE" = "0" ]; then DO_SERVICE=0; fi

    if [ "$DO_SERVICE" = "1" ]; then install_systemd_unit; fi

    cat <<EOF

Hub installed.

Next steps:
  1) Generate a token for a user:
       cloudcode-hub gen-token alice

  2) Create /etc/cloudcode/hub.toml from the example, paste your
     Anthropic API key and the token hash from step 1:
       sudo cp /etc/cloudcode/hub.example.toml /etc/cloudcode/hub.toml
       sudo \$EDITOR /etc/cloudcode/hub.toml
       sudo chown cloudcode:cloudcode /etc/cloudcode/hub.toml
       sudo chmod 640 /etc/cloudcode/hub.toml

  3) Start the service:
       sudo systemctl enable --now cloudcode-hub
       systemctl status cloudcode-hub
       journalctl -u cloudcode-hub -f
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
