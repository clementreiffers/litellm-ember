#!/usr/bin/env bash
set -euo pipefail

# La version est calculée exclusivement par semantic-release.
version="${1:?Missing semantic-release version}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "Invalid release version: $version" >&2
  exit 1
fi

cargo install tauri-cli --version '^2' --locked
cargo install trunk --version 0.21.14 --locked

printf '{"version":"%s"}\n' "$version" > release-config.json
cargo tauri build --target universal-apple-darwin --config "$PWD/release-config.json" -- --locked

mkdir -p release
ditto -c -k --sequesterRsrc --keepParent \
  target/universal-apple-darwin/release/bundle/macos/Ember.app \
  "release/Ember-${version}-macos-universal.zip"
cd release
shasum -a 256 "Ember-${version}-macos-universal.zip" > SHA256SUMS.txt
