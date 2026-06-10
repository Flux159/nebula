#!/bin/bash
# Developer ID sign + notarize + staple the built Nebula.app, emitting a
# distributable DMG. Run after scripts/bundle-app.sh (locally or in CI).
#
# Required env (CI: repo secrets; locally: source ~/.applekeys + files):
#   APPLE_TEAM_ID            e.g. Y4STC4VM22
#   APPLE_CERT_P12_BASE64    base64 of the Developer ID Application .p12
#   APPLE_CERT_PASSWORD      .p12 export password
#   APPLE_API_KEY_ID / APPLE_API_ISSUER_ID / APPLE_API_KEY_P8   (notarytool)
set -euo pipefail
cd "$(dirname "$0")/.."

# Stray whitespace in pasted IDs is the classic notarytool failure — trim.
APPLE_TEAM_ID="$(printf '%s' "${APPLE_TEAM_ID}" | tr -d '[:space:]')"
APPLE_API_KEY_ID="$(printf '%s' "${APPLE_API_KEY_ID}" | tr -d '[:space:]')"
APPLE_API_ISSUER_ID="$(printf '%s' "${APPLE_API_ISSUER_ID}" | tr -d '[:space:]')"

APP=ui/src-tauri/target/release/bundle/macos/Nebula.app
test -d "$APP" || { echo "ERROR: build the app first (scripts/bundle-app.sh)" >&2; exit 1; }
ENT=scripts/entitlements/dev.entitlements
VERSION=$(python3 -c 'import json;print(json.load(open("ui/src-tauri/tauri.conf.json"))["version"])')
DMG="dist/Nebula_${VERSION}_aarch64.dmg"
IDENTITY=""

WORK="$(mktemp -d)"
KC="$WORK/sign.keychain-db"
KCPW="$(uuidgen)"
cleanup() {
    security delete-keychain "$KC" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "==> importing signing identity into a throwaway keychain"
printf '%s' "$APPLE_CERT_P12_BASE64" | base64 -d > "$WORK/cert.p12"
security create-keychain -p "$KCPW" "$KC"
security set-keychain-settings -lut 21600 "$KC"
security unlock-keychain -p "$KCPW" "$KC"
security import "$WORK/cert.p12" -k "$KC" -P "$APPLE_CERT_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KCPW" "$KC" >/dev/null
# Make the keychain visible to codesign without touching the default.
security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
IDENTITY=$(security find-identity -v -p codesigning "$KC" | awk -F'"' '/Developer ID Application/ {print $2; exit}')
test -n "$IDENTITY" || { echo "ERROR: no Developer ID Application identity in the p12" >&2; exit 1; }
echo "    identity: $IDENTITY"

sign() { codesign --force --options runtime --timestamp --keychain "$KC" -s "$IDENTITY" "$@"; }

echo "==> signing (inside-out: dylibs -> CLIs -> sidecars -> app)"
for lib in "$APP/Contents/Frameworks/"*.dylib; do sign "$lib"; done
for bin in "$APP/Contents/Resources/resources/bin/"*; do sign "$bin"; done
# The sidecars are the processes that talk to Virtualization/Hypervisor.
sign --entitlements "$ENT" "$APP/Contents/MacOS/nebula"
sign --entitlements "$ENT" "$APP/Contents/MacOS/nebulad"
sign "$APP"
codesign --verify --strict --deep "$APP"
echo "    codesign verify: OK"

echo "==> packaging $DMG"
mkdir -p dist
rm -f "$DMG"
hdiutil create -volname "Nebula" -srcfolder "$APP" -ov -format UDZO "$DMG" >/dev/null
sign "$DMG"

echo "==> notarizing (Apple service round-trip; usually 1-10 min)"
printf '%s' "$APPLE_API_KEY_P8" > "$WORK/AuthKey.p8"
xcrun notarytool submit "$DMG" \
    --key "$WORK/AuthKey.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" \
    --wait --timeout 30m
xcrun stapler staple "$DMG"

echo "==> Gatekeeper assessment"
spctl -a -t open --context context:primary-signature -v "$DMG"
ls -lh "$DMG"
echo "==> signed + notarized + stapled: $DMG"
