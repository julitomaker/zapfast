#!/bin/bash
# Build "Whats Pro.app" for this computer.
#
#   packaging/macos/bundle-julito.sh [output.app]
#
# Runs bundle.sh, then, when WhatsApp is installed here, uses that copy's app
# icon (with a PRO badge) and message sound for the bundle (the Dock, notifications and their
# sound). Those files belong to WhatsApp: they are read from the local
# installation at build time and never committed to this repository.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
app="${1:-$HOME/Applications/Whats Pro.app}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
whatsapp="${WHATSAPP_APP:-/Applications/WhatsApp.app}/Contents/Resources"

"$here/bundle.sh" "$root/target/release/zapfast" "$app" "$version" >/dev/null

# The branded icon and sound are kept beside the app's data, so a later
# build still has them after WhatsApp itself is removed from this Mac.
brand="${WHATSPRO_BRANDING:-$HOME/Library/Application Support/me.paolino.zapfast-julito/branding}"
mkdir -p "$brand"
if [ -d "$whatsapp" ]; then
    # WhatsApp's icon with a "PRO" badge (pro-icon.swift).
    swift "$here/pro-icon.swift" "$(dirname "$(dirname "$whatsapp")")" "$brand/whatspro.icns"
fi
if [ -f "$whatsapp/incoming_message_notification.m4a" ]; then
    afconvert -f WAVE -d LEI16@44100 "$whatsapp/incoming_message_notification.m4a" \
        "$brand/receive.wav"
fi
if [ -f "$brand/whatspro.icns" ]; then
    cp "$brand/whatspro.icns" "$app/Contents/Resources/zapfast.icns"
    echo "icon: Whats Pro (WhatsApp with the PRO badge)"
else
    echo "icon: ZapFast's (no WhatsApp installed and no saved Whats Pro icon)" >&2
fi
if [ -f "$brand/receive.wav" ]; then
    # Notifications play the bundle's sound by name (src/notify.rs).
    cp "$brand/receive.wav" "$app/Contents/Resources/zapfast-receive.wav"
    echo "message sound: WhatsApp's"
else
    echo "message sound: ZapFast's (install WhatsApp once to take its sound)" >&2
fi

codesign --force --entitlements "$here/entitlements.plist" --sign - "$app"
codesign --verify --strict "$app"
# Make Finder and the Dock pick up the new icon.
touch "$app"
echo "$app"
