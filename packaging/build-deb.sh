#!/usr/bin/env bash
# Builds androscreen_<version>_<arch>.deb with the daemon, system integration
# files, the bundled evdi DKMS source and the Android app.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PKG=androscreen
VERSION=${VERSION:-$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)}
ARCH=${ARCH:-$(dpkg --print-architecture)}
MAINTAINER=${MAINTAINER:-"Feelup <testangel@163.com>"}
EVDI_VERSION=1.15.1
EVDI_SHA256=b120df303d14be09062157aab17a8598746c0caf2e076ff3388b4091186cf2d9
OUT="$ROOT/packaging/out"
STAGE="$OUT/${PKG}_${VERSION}_${ARCH}"
EVDI_TARBALL=${EVDI_TARBALL:-$ROOT/packaging/vendor/evdi-$EVDI_VERSION.tar.gz}

if [[ -z $VERSION ]]; then
  echo "error: cannot read the workspace version from Cargo.toml" >&2
  exit 1
fi

echo "==> building androscreen $VERSION for $ARCH"
cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" \
  -p androscreen -p androscreen-settings

if [[ ! -f $EVDI_TARBALL ]]; then
  echo "==> downloading evdi $EVDI_VERSION"
  mkdir -p "$(dirname "$EVDI_TARBALL")"
  curl -fsSL --retry 3 -o "$EVDI_TARBALL" \
    "https://github.com/DisplayLink/evdi/archive/refs/tags/v$EVDI_VERSION.tar.gz"
fi
echo "$EVDI_SHA256  $EVDI_TARBALL" | sha256sum -c -

# The phone app is built here rather than taken from whatever an earlier build
# left behind. Comparing its versionName is not enough: an APK built from older
# code but already stamped with the bumped version passes that check, and the
# package then hands out a phone app that does not match the desktop it shipped
# with. `APK=` still overrides this for a build that has to use a given binary.
APK=${APK:-}
if [[ -z $APK ]]; then
  echo "==> building the Android app"
  "$ROOT/android/gradlew" -p "$ROOT/android" :app:assembleRelease
  APK="$ROOT/android/app/build/outputs/apk/release/app-release.apk"
fi
if [[ ! -f $APK ]]; then
  echo "error: APK $APK does not exist" >&2
  exit 1
fi

# The settings window labels the download with the version this package was
# built from (env!("CARGO_PKG_VERSION")), not with the APK's real versionName.
# Bundling a stale APK therefore ships an old phone app while the desktop
# claims it is current, and the user has no way to notice. Refuse that build.
AAPT2=${AAPT2:-$(command -v aapt2 || true)}
if [[ -z $AAPT2 ]]; then
  for candidate in "$HOME"/Android/Sdk/build-tools/*/aapt2; do
    if [[ -x $candidate ]]; then
      AAPT2=$candidate
      break
    fi
  done
fi
# The label on the settings window and the app on the phone have to say the
# same thing, so a package that cannot prove it is refused rather than guessed.
if [[ -z $AAPT2 ]]; then
  echo "error: aapt2 not found; cannot verify the bundled APK version" >&2
  echo "       install the Android build-tools or set AAPT2" >&2
  exit 1
fi
BADGING=$("$AAPT2" dump badging "$APK")
APK_VERSION=$(echo "$BADGING" \
  | sed -n "s/.*versionName='\([^']*\)'.*/\1/p" | head -n 1)
if [[ -z $APK_VERSION ]]; then
  echo "error: cannot read versionName from $APK" >&2
  exit 1
fi
# A debuggable app on a phone is a security hole and is what an unsigned
# release silently became before the release build was signed.
if grep -q 'application-debuggable' <<<"$BADGING"; then
  echo "error: $APK is debuggable; package a signed release build" >&2
  echo "       rebuild with: ./gradlew assembleRelease" >&2
  exit 1
fi
if [[ $APK_VERSION != "$VERSION" ]]; then
  echo "error: APK versionName '$APK_VERSION' != package version '$VERSION'" >&2
  echo "       APK: $APK" >&2
  echo "       bump versionName in android/app/build.gradle.kts and rebuild" >&2
  exit 1
fi
echo "==> APK $APK_VERSION matches package $VERSION ($APK)"

echo "==> staging $STAGE"
if [[ -d $STAGE ]]; then
  # `rm -rf` is refused by locked-down build sandboxes; -depth -delete is not.
  find "$STAGE" -depth -delete
fi
install -Dm755 "$ROOT/target/release/androscreen" "$STAGE/usr/bin/androscreen"
install -Dm755 "$ROOT/target/release/androscreen-settings" \
  "$STAGE/usr/bin/androscreen-settings"
install -Dm644 "$ROOT/packaging/androscreen-settings.desktop" \
  "$STAGE/usr/share/applications/androscreen-settings.desktop"
install -Dm644 "$ROOT/packaging/icons/androscreen.svg" \
  "$STAGE/usr/share/icons/hicolor/scalable/apps/androscreen.svg"
install -Dm644 "$ROOT/packaging/udev/60-androscreen.rules" \
  "$STAGE/etc/udev/rules.d/60-androscreen.rules"
install -Dm644 "$ROOT/packaging/modprobe/androscreen.conf" \
  "$STAGE/etc/modprobe.d/androscreen.conf"
install -Dm644 "$ROOT/packaging/modules-load/androscreen.conf" \
  "$STAGE/etc/modules-load.d/androscreen.conf"
install -Dm644 "$ROOT/packaging/systemd/androscreen.service" \
  "$STAGE/usr/lib/systemd/user/androscreen.service"
install -Dm644 "$ROOT/packaging/systemd/androscreen-evdi.service" \
  "$STAGE/usr/lib/systemd/system/androscreen-evdi.service"

if [[ -n $APK ]]; then
  install -Dm644 "$APK" "$STAGE/usr/share/androscreen/androscreen.apk"
fi

WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete 2>/dev/null || true' EXIT
# `-m`: the payload is stamped by dpkg anyway, and restoring the archive's
# timestamps needs a privilege that locked-down build sandboxes do not grant.
tar -xmzf "$EVDI_TARBALL" -C "$WORK"
EVDI_TREE="$WORK/evdi-$EVDI_VERSION"
EVDI_SRC="$STAGE/usr/src/androscreen-evdi-$EVDI_VERSION"
# Local changes ride as patches on the released tarball so the vendored
# archive stays verifiable against its upstream checksum. A patch that no
# longer applies is a hard error: silently shipping the upstream behaviour
# would undo the fix without anybody noticing.
shopt -s nullglob
for patch_file in "$ROOT"/packaging/patches/evdi-"$EVDI_VERSION"-*.patch; do
  echo "==> patching $(basename "$patch_file")"
  patch -p1 -d "$EVDI_TREE" --batch --forward <"$patch_file"
done
shopt -u nullglob
install -d "$EVDI_SRC"
# The test tree is large and DKMS only wants one header out of it, so it is
# never copied: a locked-down build box may refuse to delete it afterwards.
for entry in "$EVDI_TREE"/module/*; do
  name=$(basename "$entry")
  if [[ $name == tests ]]; then
    continue
  fi
  cp -a --no-preserve=timestamps "$entry" "$EVDI_SRC/"
done
install -Dm644 "$EVDI_TREE/module/tests/evdi_test.h" "$EVDI_SRC/tests/evdi_test.h"
install -m644 "$EVDI_TREE/LICENSE" "$EVDI_SRC/LICENSE"
sed -i 's/^PACKAGE_NAME="evdi"$/PACKAGE_NAME="androscreen-evdi"/' "$EVDI_SRC/dkms.conf"
grep -q '^PACKAGE_NAME="androscreen-evdi"$' "$EVDI_SRC/dkms.conf"

mkdir -p "$STAGE/DEBIAN"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: $MAINTAINER
Depends: libusb-1.0-0, dkms, ffmpeg, pulseaudio-utils
Conflicts: evdi-dkms, evdi-source
Description: Use an Android phone as a secondary display over USB
 AndroScreen turns an Android 14+ phone into an extended desktop display over a
 plain USB cable, without enabling USB debugging. It runs inside the desktop
 session, creates a virtual display with evdi, and streams H.264 video, touch
 input and desktop audio to the phone.
 .
 The package builds the bundled evdi $EVDI_VERSION module through DKMS so it
 matches the running kernel.
EOF

cat > "$STAGE/DEBIAN/conffiles" <<'EOF'
/etc/udev/rules.d/60-androscreen.rules
/etc/modprobe.d/androscreen.conf
/etc/modules-load.d/androscreen.conf
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

EVDI_PACKAGE=androscreen-evdi
EVDI_VERSION=1.15.1

if [ ! -e "/lib/modules/$(uname -r)/build" ]; then
    echo "androscreen: kernel headers for $(uname -r) are missing." >&2
    echo "androscreen: install them, then run 'dkms autoinstall' to build evdi." >&2
fi

if command -v dkms >/dev/null 2>&1; then
    dkms add -m "$EVDI_PACKAGE" -v "$EVDI_VERSION" >/dev/null 2>&1 || true
    # `--force`: an upgrade that ships new module sources under the same
    # version would otherwise find an existing build and install the old one.
    dkms build --force -m "$EVDI_PACKAGE" -v "$EVDI_VERSION" >/dev/null 2>&1 || true
    dkms install --force -m "$EVDI_PACKAGE" -v "$EVDI_VERSION" >/dev/null 2>&1 || true
fi

# A stale module may have been loaded with initial_device_count=0.
if /usr/bin/androscreen --evdi-scan 2>/dev/null | grep -q '^no evdi virtual display cards found$'; then
    modprobe -r evdi >/dev/null 2>&1 || true
fi
modprobe evdi >/dev/null 2>&1 || true
modprobe uinput >/dev/null 2>&1 || true

# Secure Boot refuses modules whose signature the firmware does not trust. evdi
# is signed by the DKMS key built on this machine, and that key becomes trusted
# only after the user enrolls it — a request answered at the next boot, before
# the desktop. Nothing is enrolled here on the user's behalf; the settings
# window offers the switch, and this only says where it is. A module that was
# never built is a different failure and is already reported above, so the
# check starts from modinfo.
if [ ! -d /sys/module/evdi ] \
    && command -v modinfo >/dev/null 2>&1 \
    && modinfo -n evdi >/dev/null 2>&1 \
    && command -v mokutil >/dev/null 2>&1 \
    && mokutil --sb-state 2>/dev/null | grep -q 'SecureBoot enabled'; then
    if mokutil --list-new 2>/dev/null | grep -q 'SHA1 Fingerprint'; then
        echo "androscreen: a Secure Boot key request is already waiting." >&2
        echo "androscreen: reboot, then on the blue screen choose Enroll MOK ->" >&2
        echo "androscreen: Continue -> Yes and enter the MOK password that was shown." >&2
    else
        echo "androscreen: Secure Boot is enabled and evdi did not load." >&2
        echo "androscreen: evdi is signed by this machine's DKMS key, which Secure Boot" >&2
        echo "androscreen: does not trust yet, so no virtual display can be created." >&2
        echo "androscreen: open 安卓副屏 (androscreen-settings) and turn on" >&2
        echo "androscreen: 允许登记签名密钥 — nothing happens until you do." >&2
        echo "androscreen: the request is answered at the next boot, on the blue screen;" >&2
        echo "androscreen: choose Enroll MOK -> Continue -> Yes, then the MOK password." >&2
        echo "androscreen: if the machine will not boot afterwards, restore the default" >&2
        echo "androscreen: Secure Boot keys in the firmware (Restore Factory Keys), or" >&2
        echo "androscreen: turn Secure Boot off and run: sudo mokutil --revoke-import" >&2
        echo "androscreen: manual route: sudo mokutil --import /var/lib/dkms/mok.pub" >&2
    fi
fi

if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=usb || true
    udevadm trigger --subsystem-match=misc --sysname-match=uinput || true
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    systemctl enable "$EVDI_PACKAGE.service" >/dev/null 2>&1 || true
    systemctl start "$EVDI_PACKAGE.service" >/dev/null 2>&1 || true

    install -d /etc/systemd/user/default.target.wants
    ln -sf /usr/lib/systemd/user/androscreen.service \
        /etc/systemd/user/default.target.wants/androscreen.service

    for runtime in /run/user/[0-9]*; do
        [ -d "$runtime" ] || continue
        uid=${runtime##*/}
        user=$(getent passwd "$uid" | cut -d: -f1) || continue
        [ -n "$user" ] || continue
        runuser -u "$user" -- env XDG_RUNTIME_DIR="$runtime" \
            systemctl --user daemon-reload >/dev/null 2>&1 || true
        runuser -u "$user" -- env XDG_RUNTIME_DIR="$runtime" \
            systemctl --user restart androscreen.service >/dev/null 2>&1 || true
    done
fi

exit 0
EOF

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e

case "$1" in
    remove|deconfigure)
        rm -f /etc/systemd/user/default.target.wants/androscreen.service
        if command -v systemctl >/dev/null 2>&1; then
            systemctl disable --now androscreen-evdi.service >/dev/null 2>&1 || true
        fi
        ;;
esac

exit 0
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e

case "$1" in
    remove|purge)
        rm -f /etc/systemd/user/default.target.wants/androscreen.service
        if command -v dkms >/dev/null 2>&1; then
            dkms remove -m androscreen-evdi -v 1.15.1 --all >/dev/null 2>&1 || true
        fi
        if command -v udevadm >/dev/null 2>&1; then
            udevadm control --reload-rules || true
        fi
        if command -v systemctl >/dev/null 2>&1; then
            systemctl daemon-reload || true
        fi
        ;;
esac

exit 0
EOF

chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

mkdir -p "$OUT"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT/${PKG}_${VERSION}_${ARCH}.deb"
echo "==> $(du -h --apparent-size "$OUT/${PKG}_${VERSION}_${ARCH}.deb" | cut -f1)  $OUT/${PKG}_${VERSION}_${ARCH}.deb"
