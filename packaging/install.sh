#!/usr/bin/env bash
# Installs the newest package this repository built, and the phone app that
# goes with it when a phone is reachable over adb.
#
# The daemon restarts itself from the package's postinst, so the settings icon
# and the new policy take effect as soon as this finishes.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEB=$(ls -t "$ROOT"/packaging/out/androscreen_*.deb 2>/dev/null | head -n 1)

if [[ -z $DEB ]]; then
    echo "error: no package in $ROOT/packaging/out; run packaging/build-deb.sh first" >&2
    exit 1
fi

echo "==> installing $(basename "$DEB")"
sudo dpkg -i "$DEB"

if [ ! -e /sys/module/evdi ]; then
    echo "note: evdi 会在下次重启后生效（登录期间加载显卡驱动会让 deepin 25.3 的桌面卡死）"
fi

echo "==> restarting the service for the current user"
systemctl --user daemon-reload || true
systemctl --user restart androscreen || true

APK=/usr/share/androscreen/androscreen.apk
if ! command -v adb >/dev/null 2>&1; then
    echo "note: adb is not installed; copy $APK to the phone by hand to update the app"
    exit 0
fi
if ! adb devices | grep -qw device; then
    echo "note: no phone over adb right now."
    echo "      Unplug the cable, reconnect with USB debugging (or file transfer) and run:"
    echo "        adb install -r $APK"
    exit 0
fi

echo "==> installing the phone app"
adb install -r "$APK"
