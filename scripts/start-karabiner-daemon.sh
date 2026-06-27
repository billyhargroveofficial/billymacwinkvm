#!/usr/bin/env bash
set -euo pipefail

DAEMON="/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app/Contents/MacOS/Karabiner-VirtualHIDDevice-Daemon"
SOCKET="/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock"
LOG="/tmp/karabiner-vhid-daemon.log"

if [[ ! -x "$DAEMON" ]]; then
  echo "missing daemon: $DAEMON" >&2
  exit 1
fi

if pgrep -f 'Karabiner-VirtualHIDDevice-Daemon' >/dev/null; then
  echo "daemon already running"
else
  echo "starting daemon with sudo..."
  sudo nohup "$DAEMON" >"$LOG" 2>&1 &
fi

for _ in {1..30}; do
  if [[ -S "$SOCKET" ]]; then
    echo "socket ready: $SOCKET"
    exit 0
  fi
  sleep 0.5
done

echo "socket did not appear: $SOCKET" >&2
echo "daemon log: $LOG" >&2
tail -80 "$LOG" 2>/dev/null || true
exit 1
