#!/bin/sh
# fleet installer.
#
#   curl -fsSL https://raw.githubusercontent.com/evergreentechnologywy/fleet/main/install.sh | sh
#
# Options:
#   --prefix DIR   where the binary goes (default /usr/local)
#   --port N       API port for the systemd unit (default 8787)
#   --no-service   install the binary only, do not write a unit
#   --version V    release tag to fetch (default: latest)
#
# Idempotent: re-running upgrades the binary in place.

set -eu

PREFIX=/usr/local
PORT=8787
SERVICE=1
VERSION=latest
REPO=evergreentechnologywy/fleet

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'fleet-install: %s\n' "$*" >&2; exit 1; }

# --- platform ---------------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Linux) ;;
  Darwin) say "macOS: use 'cargo install --git https://github.com/$REPO' instead"; exit 1 ;;
  *) die "unsupported OS: $OS" ;;
esac
case "$ARCH" in
  x86_64|amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
  *) die "unsupported architecture: $ARCH" ;;
esac

if [ "$VERSION" = latest ]; then
  URL="https://github.com/$REPO/releases/latest/download/fleet-$TRIPLE.tar.gz"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/fleet-$TRIPLE.tar.gz"
fi

# --- fetch ------------------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "fleet: downloading $URL"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$TMP/fleet.tar.gz" || die "download failed"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$TMP/fleet.tar.gz" "$URL" || die "download failed"
else
  die "neither curl nor wget is available"
fi

tar xzf "$TMP/fleet.tar.gz" -C "$TMP" || die "the archive did not unpack"
[ -f "$TMP/fleet" ] || die "the archive contains no fleet binary"
chmod +x "$TMP/fleet"

# --- install ----------------------------------------------------------------
SUDO=""
if [ ! -w "$PREFIX/bin" ] && [ "$(id -u)" != 0 ]; then
  command -v sudo >/dev/null 2>&1 && SUDO=sudo
fi
$SUDO mkdir -p "$PREFIX/bin"
$SUDO install -m 0755 "$TMP/fleet" "$PREFIX/bin/fleet" || die "could not install to $PREFIX/bin"
say "fleet: installed $PREFIX/bin/fleet"

# The engine keeps its state in $HOME/.fleet and creates what it needs; this
# only makes the first run obvious.
HOME_DIR="${HOME:-/root}/.fleet"
$SUDO mkdir -p "$HOME_DIR/harnesses"
if [ ! -f "$HOME_DIR/env" ]; then
  $SUDO sh -c ": > '$HOME_DIR/env' && chmod 600 '$HOME_DIR/env'"
fi
if [ ! -f "$HOME_DIR/README.md" ]; then
  "$PREFIX/bin/fleet" paths >/dev/null 2>&1 || true
fi

# --- service ----------------------------------------------------------------
if [ "$SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1 && [ -d /etc/systemd/system ]; then
  USER_NAME=$(id -un)
  $SUDO tee /etc/systemd/system/fleetd.service >/dev/null <<UNIT
[Unit]
Description=fleet — a harness for harnesses
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER_NAME
Environment=FLEET_HOME=$HOME_DIR
# Provider keys are never part of an install. They are named in the descriptors
# and read from here; the file is created empty with mode 600 so there is one
# obvious place to put them.
EnvironmentFile=-$HOME_DIR/env
ExecStart=$PREFIX/bin/fleet serve --host 0.0.0.0 --port $PORT
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable --now fleetd || say "fleet: unit written; start it with: systemctl start fleetd"
  say "fleet: service fleetd on port $PORT"
fi

say ""
say "next:"
say "  fleet onboard                 # detect harnesses and providers"
say "  fleet run \"your task\"          # or drop a harness file in $HOME_DIR/harnesses"
say "  fleet serve --port $PORT        # the HTTP API and web door"
