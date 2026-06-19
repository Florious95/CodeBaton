#!/usr/bin/env bash
# Remove the stray .VolumeIcon.icns from the built DMG so it can never appear in
# the Finder install window (BUG 331 / ISS-014).
#
# Tauri's bundle_dmg.sh drops .VolumeIcon.icns at the volume root for a custom
# disk icon. Setting the macOS hidden flag is NOT reliable — if the user has
# "show hidden files" on, Finder shows it anyway. So we DELETE the file outright
# (the DMG simply uses the default disk icon) and clear the volume's
# custom-icon FinderInfo bit so Finder doesn't try to render a now-missing icon.
# .DS_Store is kept so the window layout (CodeBaton.app + Applications
# positions) survives.
#
# Re-masters the read-only UDZO image: UDRW -> delete/clear -> UDZO.
#
# Usage: hide-dmg-volicon.sh /path/to/Foo_x.y.z_arch.dmg
set -euo pipefail

DMG="${1:?usage: hide-dmg-volicon.sh <dmg>}"
[ -f "$DMG" ] || { echo "[hide-dmg] no such dmg: $DMG" >&2; exit 1; }

rw="$(dirname "$DMG")/.rw-$(basename "$DMG")"
rm -f "$rw"

# UDZO (read-only/compressed) -> UDRW (read-write) so we can delete files.
hdiutil convert "$DMG" -format UDRW -o "$rw" -quiet

mnt="$(mktemp -d /tmp/cb-dmg-XXXX)"
hdiutil attach "$rw" -nobrowse -mountpoint "$mnt" -quiet

# 1) Delete the stray volume icon entirely.
rm -f "$mnt/.VolumeIcon.icns" 2>/dev/null || true
# 2) Delete .DS_Store too. Tauri's bundle_dmg baked an icon-layout entry for
#    .VolumeIcon.icns into it, so even after deleting the file, Finder reads the
#    layout and renders a stale top-left icon (BUG 331/ISS-014, screenshot 上传).
#    Removing .DS_Store drops the custom positions (Finder uses default layout:
#    just CodeBaton.app + Applications) and guarantees no phantom icon.
rm -f "$mnt/.DS_Store" 2>/dev/null || true
# 3) Clear the volume's "has custom icon" FinderInfo bit (lowercase c clears).
SetFile -a c "$mnt" 2>/dev/null || true
# 4) Defensively remove other stray system dotfiles if present.
for f in "$mnt/.fseventsd" "$mnt/.background" "$mnt/.Trashes" "$mnt/.Spotlight-V100"; do
  rm -rf "$f" 2>/dev/null || true
done

hdiutil detach "$mnt" -quiet
rmdir "$mnt" 2>/dev/null || true

# Back to UDZO, overwriting the shipped DMG.
rm -f "$DMG"
hdiutil convert "$rw" -format UDZO -imagekey zlib-level=9 -o "$DMG" -quiet
rm -f "$rw"
echo "[hide-dmg] removed .VolumeIcon.icns from $(basename "$DMG")"
