#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DISPLAY_NUM=":2"

if [ -z "${DISPLAY:-}" ]; then
  echo "Host DISPLAY is not set. Start this from an active X11 session (for example, a terminal inside your desktop session)."
  exit 1
fi

if ! command -v Xephyr >/dev/null 2>&1; then
  echo "Xephyr not found. Install xorg-server-xephyr first."
  exit 1
fi

if ! command -v xterm >/dev/null 2>&1; then
  echo "xterm not found. Install xterm first."
  exit 1
fi

echo "Building dxdwm..."
cargo build --manifest-path "$ROOT_DIR/Cargo.toml"

echo "Starting Xephyr on $DISPLAY_NUM..."
Xephyr "$DISPLAY_NUM" -screen 1280x720 -ac &
XEPHYR_PID=$!

cleanup() {
  kill "$XEPHYR_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

sleep 1

echo "Starting dxdwm in nested display..."
DISPLAY="$DISPLAY_NUM" "$ROOT_DIR/target/debug/dxdwm" &
DWM_PID=$!

sleep 1

DISPLAY="$DISPLAY_NUM" xterm &
DISPLAY="$DISPLAY_NUM" xterm &

wait "$DWM_PID"

