#!/usr/bin/env bash
set -euo pipefail

# La version est calculée exclusivement par semantic-release.
version="${1:?Missing semantic-release version}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "Invalid release version: $version" >&2
  exit 1
fi

# La signature Tauri reste obligatoire, indépendamment de la signature ad hoc macOS.
for secret in TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD TAURI_SIGNING_PUBLIC_KEY; do
  [[ -n "${!secret:-}" ]] || { echo "Missing release configuration: $secret" >&2; exit 1; }
done

# Valider la clé publique avant toute installation ou compilation.
node scripts/updater-release.mjs config "$version"

# Cette publication ne doit ni importer de certificat ni contacter le service Apple.
unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY
unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH APPLE_API_PRIVATE_KEY

cargo install tauri-cli --version '2.12.1' --locked
cargo install trunk --version 0.21.14 --locked

verification_dir=$(mktemp -d)
trap 'rm -rf "$verification_dir"' EXIT

# Ne pas utiliser --no-sign : cela désactiverait aussi les signatures Tauri.
cargo tauri build --target universal-apple-darwin --config "$PWD/release-config.json" -- --locked

verify_universal() {
  local architectures
  architectures=$(lipo "$1" -archs)
  [[ " $architectures " == *" arm64 "* && " $architectures " == *" x86_64 "* ]] || {
    echo 'Expected both arm64 and x86_64 in the application binary' >&2
    exit 1
  }
}

mkdir -p release
bundle="target/universal-apple-darwin/release/bundle/macos"
app="$bundle/Ember.app"
actual_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")
[[ "$actual_version" == "$version" ]] || { echo 'Bundle version mismatch' >&2; exit 1; }
codesign --verify --deep --strict --verbose=2 "$app"
verify_universal "$app/Contents/MacOS/ember"

base="release/Ember-${version}-macos-universal"
dmgs=(target/universal-apple-darwin/release/bundle/dmg/*.dmg)
[[ ${#dmgs[@]} -eq 1 && -f "${dmgs[0]}" ]] || { echo 'Expected exactly one DMG' >&2; exit 1; }
cp "${dmgs[0]}" "$base.dmg"
hdiutil verify "$base.dmg"
cp "$bundle/Ember.app.tar.gz" "$base.app.tar.gz"
cp "$bundle/Ember.app.tar.gz.sig" "$base.app.tar.gz.sig"
# Contrôler aussi la signature ad hoc du bundle réellement transporté.
mkdir "$verification_dir/extracted"
tar -xzf "$base.app.tar.gz" -C "$verification_dir/extracted"
codesign --verify --deep --strict --verbose=2 "$verification_dir/extracted/Ember.app"
verify_universal "$verification_dir/extracted/Ember.app/Contents/MacOS/ember"
node scripts/updater-release.mjs artifacts "$version"
