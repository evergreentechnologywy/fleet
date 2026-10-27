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
# The binary must MATCH the platform, not merely download: an ELF on macOS or a
# Mach-O on Linux fails only at exec time, which reads as a corrupt install.
OS=$(uname -s)
ARCH=$(uname -m)

# Android/Termux reports Linux + aarch64, so it would otherwise get the glibc
# build and die on a missing loader. Termux exports $PREFIX and $ANDROID_ROOT; a
# bare /system/bin check also matches a Linux box with an Android SDK, so prefer
# the env signals. NOTE: Termux's $PREFIX is NOT this script's --prefix option,
# which is why it is captured here before anything reassigns it.
TERMUX_PREFIX_ENV="${PREFIX:-}"
case "$TERMUX_PREFIX_ENV" in
  *com.termux*) ON_ANDROID=1 ;;
  *) if [ -n "${ANDROID_ROOT:-}" ] && [ -d /system/bin ]; then ON_ANDROID=1; else ON_ANDROID=0; fi ;;
esac

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64)  TRIPLE=aarch64-apple-darwin ;;
      x86_64) TRIPLE=x86_64-apple-darwin ;;
      *) die "unsupported macOS architecture: $ARCH" ;;
    esac
    ;;
  Linux)
    if [ "$ON_ANDROID" = 1 ]; then
      # Termux/Android: a STATIC musl build, because Android's bionic loader and
      # a relocated $PREFIX mean neither the glibc nor the standard musl build
      # can be trusted to find its own interpreter.
      case "$ARCH" in
        aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl ;;
        x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl ;;
        *) die "unsupported Android architecture: $ARCH" ;;
      esac
      say "fleet: Android/Termux detected — using the static build"
    else
      case "$ARCH" in
        x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl ;;
        aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl ;;
        *) die "unsupported architecture: $ARCH" ;;
      esac
    fi
    ;;
  *) die "unsupported OS: $OS" ;;
esac

# Default the install prefix per platform. Termux cannot write /usr/local, and
# its own $PREFIX/bin is already on PATH, so install there instead.
if [ "$PREFIX" = /usr/local ] && [ "$ON_ANDROID" = 1 ] &&
   [ -n "$TERMUX_PREFIX_ENV" ] && [ -d "$TERMUX_PREFIX_ENV/bin" ]; then
  PREFIX="$TERMUX_PREFIX_ENV"
fi

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
# Termux ships no GNU `install`, and its coreutils `install` may be absent too, so
# fall back to cp+chmod rather than failing an otherwise fine install.
if command -v install >/dev/null 2>&1; then
  $SUDO install -m 0755 "$TMP/fleet" "$PREFIX/bin/fleet" || die "could not install to $PREFIX/bin"
else
  $SUDO cp "$TMP/fleet" "$PREFIX/bin/fleet" || die "could not install to $PREFIX/bin"
  $SUDO chmod 0755 "$PREFIX/bin/fleet"
fi
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
# systemd on Linux, launchd on macOS, nothing on Android. Each is guarded by what
# actually exists rather than by $OS alone, so a container without systemd or a
# Mac without a user session degrades to "installed, start it yourself".
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
elif [ "$SERVICE" = 1 ] && [ "$OS" = Darwin ]; then
  # launchd, not systemd. A LaunchAgent (not a LaunchDaemon) because the keys live
  # in the user's home and a daemon would run as root without them.
  PLIST="$HOME/Library/LaunchAgents/site.evergreen.fleetd.plist"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>site.evergreen.fleetd</string>
  <key>ProgramArguments</key>
  <array>
    <string>$PREFIX/bin/fleet</string>
    <string>serve</string>
    <string>--host</string><string>0.0.0.0</string>
    <string>--port</string><string>$PORT</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>FLEET_HOME</key><string>$HOME_DIR</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME_DIR/logs/fleetd.log</string>
  <key>StandardErrorPath</key><string>$HOME_DIR/logs/fleetd.err</string>
</dict>
</plist>
PLIST_EOF
  mkdir -p "$HOME_DIR/logs"
  # bootstrap is idempotent only if the label is absent, so bootout first.
  launchctl bootout "gui/$(id -u)/site.evergreen.fleetd" 2>/dev/null || true
  if launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null; then
    say "fleet: LaunchAgent site.evergreen.fleetd on port $PORT"
  else
    say "fleet: plist written to $PLIST; load it with: launchctl bootstrap gui/\$(id -u) '$PLIST'"
  fi
fi

say ""
say "next:"
say "  fleet onboard                 # detect harnesses and providers"
say "  fleet run \"your task\"          # or drop a harness file in $HOME_DIR/harnesses"
say "  fleet serve --port $PORT        # the HTTP API and web door"
