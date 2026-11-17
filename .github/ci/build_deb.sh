#!/bin/bash
# build_deb.sh BUNDLE VERSION OUT.deb: RigDesk's Linux bundle (the Flutter build: rustdesk, lib/, data/) as the rigdesk
# package. It installs beside nothing: it replaces a stock rustdesk package, and never touches a RustDesk Flatpak, which
# lives in its own namespace with its own config.
set -euo pipefail
BUNDLE=$(realpath "$1"); VERSION=$2; OUT=$(realpath -m "$3")
RES=$(cd "$(dirname "$0")/../../res" && pwd)
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
install -d "$ROOT/usr/share/rigdesk" "$ROOT/usr/bin" "$ROOT/usr/lib/systemd/system" "$ROOT/usr/share/applications" \
    "$ROOT/usr/share/icons/hicolor/256x256/apps" "$ROOT/usr/share/icons/hicolor/scalable/apps" "$ROOT/DEBIAN"
cp -a "$BUNDLE/lib" "$BUNDLE/data" "$ROOT/usr/share/rigdesk/"
install -m 755 "$BUNDLE/rustdesk" "$ROOT/usr/share/rigdesk/rigdesk"
ln -s /usr/share/rigdesk/rigdesk "$ROOT/usr/bin/rigdesk"
install -m 644 "$RES/deb/rigdesk.service" "$ROOT/usr/lib/systemd/system/rigdesk.service"
install -m 644 "$RES/deb/rigdesk.desktop" "$ROOT/usr/share/applications/rigdesk.desktop"
install -m 644 "$RES/128x128@2x.png" "$ROOT/usr/share/icons/hicolor/256x256/apps/rigdesk.png"
install -m 644 "$RES/scalable.svg" "$ROOT/usr/share/icons/hicolor/scalable/apps/rigdesk.svg"
install -m 755 "$RES/deb/postinst" "$RES/deb/prerm" "$RES/deb/postrm" "$ROOT/DEBIAN/"
SIZE=$(du -sk --exclude=DEBIAN "$ROOT" | cut -f1)
cat > "$ROOT/DEBIAN/control" <<CONTROL
Package: rigdesk
Section: net
Priority: optional
Version: $VERSION
Architecture: amd64
Maintainer: deussalo <noreply@github.com>
Installed-Size: $SIZE
Depends: libgtk-3-0t64 | libgtk-3-0, libxcb-randr0, libxdo3 | libxdo4, libxfixes3, libxcb-shape0, libxcb-xfixes0, libasound2t64 | libasound2, libsystemd0, curl, libva2, libva-drm2, libva-x11-2, libgstreamer-plugins-base1.0-0, gstreamer1.0-pipewire, systemd (>= 248), dbus-user-session
Recommends: libayatana-appindicator3-1
Conflicts: rustdesk
Replaces: rustdesk
Description: RigDesk: remote desktop and agent API for Rig
 A RustDesk build that registers nowhere, takes viewers from the tailnet alone, and serves Rig's agents: a shared
 desktop, and a management lane that runs commands as root or the signed-in user.
CONTROL
dpkg-deb --root-owner-group --build "$ROOT" "$OUT"
dpkg-deb --info "$OUT" | sed -n '1,30p'
