#!/bin/sh
# Build vmagent, vmcore, and the SwiftUI manager app.
set -e
cd "$(dirname "$0")/.."
scripts/build.sh
APP=bin/VMAgent.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" \
    "$APP/Contents/Resources/bin" \
    "$APP/Contents/Resources/cloud-init" \
    "$APP/Contents/Resources/scripts"
cp gui/Info.plist "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp cloud-init/user-data cloud-init/user-data-gui cloud-init/meta-data \
    "$APP/Contents/Resources/cloud-init/"
cp scripts/fetch-debian.sh "$APP/Contents/Resources/scripts/"
# APFS is often case-insensitive. Do not put vmagent next to VMAgent.
cp bin/vmagent bin/vmcore bin/split-image.py bin/askpass.sh \
    "$APP/Contents/Resources/bin/"
chmod +x "$APP/Contents/Resources/bin/vmagent" \
    "$APP/Contents/Resources/bin/vmcore" \
    "$APP/Contents/Resources/bin/askpass.sh" \
    "$APP/Contents/Resources/bin/split-image.py" \
    "$APP/Contents/Resources/scripts/fetch-debian.sh"
swiftc -O -parse-as-library \
    -target arm64-apple-macos13 \
    -sdk "$(xcrun --sdk macosx --show-sdk-path)" \
    -framework SwiftUI -framework AppKit \
    -o "$APP/Contents/MacOS/VMAgent" \
    gui/VMAgentApp.swift gui/VMStore.swift
codesign --force --sign - --entitlements vmcore/vmcore.entitlements \
    "$APP/Contents/Resources/bin/vmcore"
codesign --force --sign - "$APP/Contents/Resources/bin/vmagent"
codesign --force --sign - "$APP/Contents/MacOS/VMAgent"
codesign --force --sign - "$APP"
echo "built $APP"
echo "open $APP"
