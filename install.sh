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

# ---- common setup shared by hub & agent ----
ensure_cloudcode_user() {
  if ! id cloudcode >/dev/null 2>&1; then
    echo "Creating system user 'cloudcode'..."
    $SUDO useradd --system --no-create-home --shell /usr/sbin/nologin cloudcode
  fi
  $SUDO mkdir -p /etc/cloudcode /var/log/cloudcode
}

systemd_preflight() {
  if [ "$OS" != "Linux" ]; then
    echo "systemd unit only supported on Linux; skipping" >&2
    return 1
  fi
  if ! command -v systemctl >/dev/null 2>&1; then
    echo "systemctl not found; skipping service install" >&2
    return 1
  fi
  return 0
}

# ---- hub-specific systemd install ----
install_hub_systemd_unit() {
  systemd_preflight || return 0
  ensure_cloudcode_user
  $SUDO mkdir -p /var/lib/cloudcode
  $SUDO chown cloudcode:cloudcode /var/log/cloudcode /var/lib/cloudcode

  if [ ! -f /etc/cloudcode/hub.example.toml ]; then
    $SUDO install -m 644 "$SRC/hub.example.toml" /etc/cloudcode/hub.example.toml
  fi

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

# ---- agent-specific systemd install ----
install_agent_systemd_unit() {
  systemd_preflight || return 0
  ensure_cloudcode_user
  $SUDO mkdir -p /var/lib/cloudcode-agent
  $SUDO chown cloudcode:cloudcode /var/lib/cloudcode-agent
  $SUDO chmod 700 /var/lib/cloudcode-agent

  if [ ! -f /etc/cloudcode/agent.example.toml ]; then
    $SUDO install -m 644 "$SRC/agent.example.toml" /etc/cloudcode/agent.example.toml
  fi

  UNIT=/etc/systemd/system/cloudcode-agent.service
  echo "Writing $UNIT"
  $SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=Cloudcode Agent (claude subscription forwarder)
Documentation=https://github.com/${REPO}
After=network.target

[Service]
Type=simple
User=cloudcode
Group=cloudcode
WorkingDirectory=/var/lib/cloudcode-agent
ExecStart=${BIN_DIR}/cloudcode-agent serve --config /etc/cloudcode/agent.toml
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/log/cloudcode /var/lib/cloudcode-agent
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

    if [ "$DO_SERVICE" = "1" ]; then install_hub_systemd_unit; fi

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

  agent)
    install_bin cloudcode-agent

    DO_SERVICE=0
    if [ "$INSTALL_SERVICE" = "auto" ] && [ "$OS" = "Linux" ]; then DO_SERVICE=1; fi
    if [ "$INSTALL_SERVICE" = "1" ]; then DO_SERVICE=1; fi
    if [ "$INSTALL_SERVICE" = "0" ]; then DO_SERVICE=0; fi

    if [ "$DO_SERVICE" = "1" ]; then install_agent_systemd_unit; fi

    cat <<EOF

Agent installed.

Next steps:
  1) Generate a shared secret for hub<->agent auth:
       cloudcode-agent gen-secret

  2) On any workstation that can open a browser, log in to claude
     (claude code uses an OAuth PKCE flow that needs a browser):
       claude            # then run /login inside, complete OAuth
       # the credentials end up in ~/.claude/.credentials.json

  3) Copy the credentials onto this server:
       scp ~/.claude/.credentials.json THIS-SERVER:/tmp/cc-credentials.json
       sudo install -o cloudcode -g cloudcode -m 600 \\
            /tmp/cc-credentials.json /var/lib/cloudcode-agent/credentials.json
       sudo rm /tmp/cc-credentials.json

  4) Create /etc/cloudcode/agent.toml from the example, paste the
     shared_secret_hash from step 1, leave credentials_path at the
     default /var/lib/cloudcode-agent/credentials.json:
       sudo cp /etc/cloudcode/agent.example.toml /etc/cloudcode/agent.toml
       sudo \$EDITOR /etc/cloudcode/agent.toml
       sudo chown cloudcode:cloudcode /etc/cloudcode/agent.toml
       sudo chmod 640 /etc/cloudcode/agent.toml

  5) Start the service:
       sudo systemctl enable --now cloudcode-agent
       journalctl -u cloudcode-agent -f

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
