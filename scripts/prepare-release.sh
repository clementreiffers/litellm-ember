#!/usr/bin/env bash
set -euo pipefail

# La version est calculée exclusivement par semantic-release.
version="${1:?Missing semantic-release version}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "Invalid release version: $version" >&2
  exit 1
fi

# Ne jamais publier silencieusement une application non signée/non notarisée.
for secret in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY \
  APPLE_API_KEY APPLE_API_ISSUER APPLE_API_PRIVATE_KEY \
  TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD TAURI_SIGNING_PUBLIC_KEY; do
  [[ -n "${!secret:-}" ]] || { echo "Missing release configuration: $secret" >&2; exit 1; }
done

cargo install tauri-cli --version '2.12.1' --locked
cargo install trunk --version 0.21.14 --locked

# Fichier éphémère hors du dépôt ; son contenu ne doit jamais apparaître dans les logs.
notary_dir=$(mktemp -d)
trap 'rm -rf "$notary_dir"' EXIT
export APPLE_API_KEY_PATH="$notary_dir/AuthKey_${APPLE_API_KEY}.p8"
(umask 077; printf '%s' "$APPLE_API_PRIVATE_KEY" > "$APPLE_API_KEY_PATH")

node scripts/updater-release.mjs config "$version"
cargo tauri build --target universal-apple-darwin --config "$PWD/release-config.json" -- --locked

mkdir -p release
bundle="target/universal-apple-darwin/release/bundle/macos"
app="$bundle/Ember.app"
actual_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")
[[ "$actual_version" == "$version" ]] || { echo 'Bundle version mismatch' >&2; exit 1; }
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"
lipo -verify_arch arm64 x86_64 "$app/Contents/MacOS/ember"

base="release/Ember-${version}-macos-universal"
dmgs=(target/universal-apple-darwin/release/bundle/dmg/*.dmg)
[[ ${#dmgs[@]} -eq 1 && -f "${dmgs[0]}" ]] || { echo 'Expected exactly one DMG' >&2; exit 1; }
cp "${dmgs[0]}" "$base.dmg"
# Tauri signe le DMG ; notariser et agrafer également ce conteneur final.
codesign --verify --strict --verbose=2 "$base.dmg"
xcrun notarytool submit "$base.dmg" --key "$APPLE_API_KEY_PATH" \
  --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER" --wait
xcrun stapler staple "$base.dmg"
xcrun stapler validate "$base.dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$base.dmg"
cp "$bundle/Ember.app.tar.gz" "$base.app.tar.gz"
cp "$bundle/Ember.app.tar.gz.sig" "$base.app.tar.gz.sig"
# Contrôler le bundle réellement transporté, notamment la présence du ticket agrafé.
mkdir "$notary_dir/extracted"
tar -xzf "$base.app.tar.gz" -C "$notary_dir/extracted"
codesign --verify --deep --strict --verbose=2 "$notary_dir/extracted/Ember.app"
xcrun stapler validate "$notary_dir/extracted/Ember.app"
node scripts/updater-release.mjs artifacts "$version"
