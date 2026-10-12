# Updater fixtures

These public keys, payloads and signatures were generated with Tauri CLI 2.12.1
using a disposable test key. No private key is stored in this repository.
Both signatures bind version `1.1.0` into the authenticated trusted comment.

`archive.txt` exercises exact-byte and signed-version validation.
`Ember.app.tar.gz` contains a minimal test bundle: its `ember` file is plain text,
never an executable application. Tests install it only into a temporary directory,
without launching it or touching the running Ember application.
