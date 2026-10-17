#!/bin/bash
# Build "ZapFast Julito.app" for this computer.
#
#   packaging/macos/bundle-julito.sh [output.app]
#
# Runs bundle.sh, then, when WhatsApp is installed here, uses that copy's app
# icon and message sound for the bundle (the Dock, notifications and their
# sound). Those files belong to WhatsApp: they are read from the local
# installation at build time and never committed to this repository.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
app="${1:-$HOME/Applications/ZapFast Julito.app}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
whatsapp="${WHATSAPP_APP:-/Applications/WhatsApp.app}/Contents/Resources"

"$here/bundle.sh" "$root/target/release/zapfast" "$app" "$version" >/dev/null

if [ -f "$whatsapp/AppIcon.icns" ]; then
    cp "$whatsapp/AppIcon.icns" "$app/Contents/Resources/zapfast.icns"
    echo "icon: WhatsApp's"
fi
if [ -f "$whatsapp/incoming_message_notification.m4a" ]; then
    # Notifications play the bundle's sound by name (src/notify.rs).
    afconvert -f WAVE -d LEI16@44100 "$whatsapp/incoming_message_notification.m4a" \
        "$app/Contents/Resources/zapfast-receive.wav"
    echo "message sound: WhatsApp's"
fi

codesign --force --entitlements "$here/entitlements.plist" --sign - "$app"
codesign --verify --strict "$app"
# Make Finder and the Dock pick up the new icon.
touch "$app"
echo "$app"
