#!/usr/bin/env bash
set -euo pipefail

# La version est calculée exclusivement par semantic-release.
version="${1:?Missing semantic-release version}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "Invalid release version: $version" >&2
  exit 1
fi

# Fail before building when the release cannot be authenticated.
: "${TAURI_SIGNING_PRIVATE_KEY:?Missing updater signing key}"
: "${EMBER_UPDATER_PUBLIC_KEY:?Missing updater public key}"

# This CLI binds the app version into the signature's authenticated comment.
cargo install tauri-cli --version '2.12.1' --locked
cargo install trunk --version 0.21.14 --locked

node scripts/updater-release.mjs config "$version"
cargo tauri build --target universal-apple-darwin --config "$PWD/release-config.json" -- --locked

mkdir -p release
bundle="target/universal-apple-darwin/release/bundle/macos"
actual_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$bundle/Ember.app/Contents/Info.plist")
[[ "$actual_version" == "$version" ]] || { echo 'Bundle version mismatch' >&2; exit 1; }
cp "$bundle/Ember.app.tar.gz" "release/Ember-${version}-macos-universal.app.tar.gz"
cp "$bundle/Ember.app.tar.gz.sig" "release/Ember-${version}-macos-universal.app.tar.gz.sig"
cargo run -p ember --example verify-update --locked -- \
  "release/Ember-${version}-macos-universal.app.tar.gz" \
  "release/Ember-${version}-macos-universal.app.tar.gz.sig" release-config.json "$version"
ditto -c -k --sequesterRsrc --keepParent \
  target/universal-apple-darwin/release/bundle/macos/Ember.app \
  "release/Ember-${version}-macos-universal.zip"
node scripts/updater-release.mjs manifest "$version"
