<p align="center">
  <img src="src-tauri/icons/icon.png" alt="Ember logo: a flame surrounded by a budget gauge" width="128" height="128">
</p>

# Ember

I built Ember to keep an eye on my AI spending without having to open a dashboard every time. When requests pile up and you switch between models throughout the day, it helps to know where your budget stands at a glance.

Ember puts your **LiteLLM spending right in the macOS menu bar**. Leave it running all day: your spending stays visible while you work, the details are one click away, and notifications let you know when you reach your chosen budget thresholds.

## Your budget at a glance

- **Spending in the menu bar**: the amount spent during the current budget period, updated every 30 seconds by default.
- **Usage by model**: costs, today's tokens, and the split between input and output tokens.
- **A budget gauge**: spending, budget limit, and reset date.
- **The Week tab**: daily spending, a comparison with yesterday, and an end-of-period projection.
- **The Activity tab**: requests, errors, cache hits, latency, and hourly activity, with a breakdown by model.
- **macOS notifications**: two configurable thresholds, set to 50% and 75% of the budget consumed by default.
- **In-app updates**: checks GitHub at startup and every 10 minutes. Click the update notification to install and restart, or check manually in Settings.

The panel uses a dark theme with Liquid Glass on macOS 26 and native vibrancy on earlier versions. The Week and Activity tabs load their data when opened and reuse it for 60 seconds. A refresh button lets you reload the data manually.

## Preview

<!--
To add screenshots:
1. Create assets/screenshots/ and add the four PNG files referenced below.
2. Uncomment each image line and remove its corresponding "Screenshot coming soon" blockquote.
Relative paths will work directly on GitHub.
-->

### Menu bar and usage

Your spending at a glance, with the panel open to show costs and tokens by model.

> 📸 Screenshot coming soon: the menu bar with the Usage panel open.

<!-- ![Ember in the menu bar with the Usage panel open](assets/screenshots/consommation.png) -->

### Budget trends

Daily spending and a projection based on your current pace.

> 📸 Screenshot coming soon: the Week tab.

<!-- ![Week tab showing spending trends and the budget projection](assets/screenshots/semaine.png) -->

### Activity details

Request metrics and performance, grouped by model.

> 📸 Screenshot coming soon: the Activity tab.

<!-- ![Activity tab showing requests, latency, and hourly activity](assets/screenshots/activite.png) -->

### Settings and notifications

Configure your endpoint, API key, refresh interval, and alert thresholds from the panel.

> 📸 Screenshot coming soon: settings with notification options.

<!-- ![Ember settings and notification thresholds](assets/screenshots/parametres.png) -->

## Download for macOS

Ready-to-use macOS builds are available on [GitHub Releases](https://github.com/clementreiffers/litellm-ember/releases). Choose the version you want, or go directly to the [latest release](https://github.com/clementreiffers/litellm-ember/releases/latest).

1. Open your chosen release and expand **Assets**.
2. Download `Ember-X.Y.Z-macos-universal.zip`.
3. Unzip it, move **Ember.app** to **Applications**, and launch it.

The same universal build works on both **Apple Silicon and Intel Macs**. No Rust installation or local build is required.

After tests and Clippy pass on a push to `main`, semantic-release analyzes the commits since the last release and publishes a new version when needed. Tags and release names use `X.Y.Z`, without a `v` prefix. The version is injected into the universal macOS build without committing generated changes. Each release includes generated release notes and a SHA-256 checksum file.

Use [Conventional Commits](https://www.conventionalcommits.org/) for commits merged into `main` (or the squash commit title and body):

| Commit | Release |
|---|---|
| `fix: correct the budget total` | Patch (`1.2.3` → `1.2.4`) |
| `feat: add weekly spending` | Minor (`1.2.3` → `1.3.0`) |
| `feat!: change the settings format` or a `BREAKING CHANGE:` footer | Major (`1.2.3` → `2.0.0`) |
| `docs:`, `test:`, `ci:`, `chore:`, or other changes without a breaking change | No release |

The highest required bump wins when multiple commits are pushed. Existing reachable `X.Y.Z` tags are used as the release baseline; without a previous release, the first releasable change creates `1.0.0`. Node.js 24 and `npm ci` install the release tooling; `npm test` verifies the versioning rules without publishing.

These builds are not signed with an Apple Developer ID or notarized, so macOS may require approval in **System Settings → Privacy & Security** on first launch.

### Updating Ember

The main panel displays the version of the installed GitHub release, embedded during
the release build. When no release version was supplied (for example, a local build),
Tauri falls back to the backend's Cargo version. Displaying this version works offline.

Official builds check the latest stable GitHub release independently of your LiteLLM
connection. Ember sends one notification per available version, remembering it across
restarts. Budget notification preferences do not disable update notifications.

Click the notification to download, verify the update signature, replace the running
`.app` at its current location, and restart. The settings panel displays progress;
macOS asks for administrator authorization if required. You can also open **Paramètres
→ Mises à jour → Vérifier les mises à jour**, then choose **Installer et redémarrer**.
This manual path also works when macOS notifications are disabled.

The updater preserves your settings, cached statistics, and Keychain entry. Network or
signature failures do not trigger installation or restart. If authorization is refused,
retry from Settings. Read-only volumes and App Translocation cannot be updated: move
Ember to a permanent writable location and relaunch it. An installation error is shown
in the panel; if the updater could not restore a previous bundle, its error identifies
the recovery copy. Automatic checks retry on the next 10-minute interval.

The first release with this feature must be installed manually. Development builds and
local builds without an updater public key disable updating. The updater signature is
separate from Apple Developer ID signing and notarization.

### Release signing setup

Before publishing the first updater-enabled release, create and securely back up a
dedicated key pair with `cargo tauri signer generate -w /secure/path/ember-updater.key`.
Do not put the private key in the repository. Configure the GitHub repository with:

| GitHub configuration | Value |
|---|---|
| Secret `TAURI_SIGNING_PRIVATE_KEY` | Contents of the generated private-key file |
| Secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Its password (empty for an unencrypted key) |
| Variable `EMBER_UPDATER_PUBLIC_KEY` | Contents of the generated `.pub` file |

The release build embeds the public key and emits a universal `.app.tar.gz`, its `.sig`,
and `latest.json` with entries for both Mac architectures. The existing ZIP remains
available for manual installation. The pinned Tauri CLI binds the release version into
the signature. Before publication, `verify-update` verifies both the archive signature
and its authenticated version against the embedded public key; missing keys, artifacts,
or mismatches fail the release. Checksums cover all published update artifacts.

Keep the private key and password: future releases must use the same signing identity
for installed clients to accept them. A key rotation requires a migration release
signed with the previously trusted key; simply replacing the GitHub key breaks updates
for existing installations.

For a release acceptance test, use two isolated test bundles and a disposable signing
key. Verify the native notification click, download progress, restart/version change,
and settings/Keychain continuity on Apple Silicon and Intel. Test a custom path with
spaces, administrator authorization accepted/refused, and a read-only or translocated
bundle. Automated tests exercise signed downloads and replacement of a minimal fixture
bundle in a temporary directory; they never update or launch your installed Ember.

## Build from source

You will need a **Mac**, the Xcode Command Line Tools, [Rust](https://www.rust-lang.org/tools/install), and access to a LiteLLM instance.

Install the required tools:

```bash
xcode-select --install # If the Xcode Command Line Tools are not already installed
rustup target add wasm32-unknown-unknown
cargo install tauri-cli --version "^2" --locked
cargo install trunk --locked
```

From the root of your cloned repository, build the application:

```bash
cargo tauri build
```

The application is generated at `target/release/bundle/macos/Ember.app`. Copy it to your **Applications** folder and launch it. Ember will appear in the menu bar.

## Getting started

The application interface is currently in French. The tab names used above correspond to **Consommation** (Usage), **Semaine** (Week), and **Activité** (Activity).

1. Click the amount in the menu bar, then **⚙︎**.
2. The endpoint is initially empty. Enter your LiteLLM instance URL, including any path prefix, such as `https://litellm.example.com/llm`.
3. Add your API key and choose a refresh interval between **5 and 3,600 seconds**.
4. Save your settings, then click **Envoyer une notification de test** (Send a test notification) to check notification delivery and macOS permissions.

Your key must have access to the `/key/info` and `/spend/logs` endpoints. Without a saved key, the application makes no requests to LiteLLM.

### Local storage

After upgrading from a build with the previous application identity, enter your endpoint and API key again. Existing local data and Keychain entries are left untouched and are not imported automatically. macOS may ask you to allow notifications again.

Your API key is stored in the **macOS Keychain** and is never sent back to the interface. Preferences, the latest statistics, and alert state are stored locally in the application's data directory. The cache restores your latest figures as soon as the app starts.

Alerts remember which thresholds have already been reported. They reset when a new budget period starts or when spending drops below the relevant threshold.

### Understanding the numbers

The displayed amount covers the **key's current budget period**, rather than its lifetime spending. Figures depend on the data and permissions provided by LiteLLM: a model without configured pricing may report a cost of $0. The projection is an estimate based on your current spending pace.

## Development

Ember is built with **Rust**, **Tauri v2**, and **Leptos**, with a frontend compiled to WebAssembly. Charts are rendered using HTML/CSS and SVG.

| Directory | Purpose |
|---|---|
| `src-tauri/` | LiteLLM client, menu bar integration, settings, and notifications |
| `ui/` | Leptos interface, charts, and styles |
| `shared/` | Types shared between the backend and frontend |
| `assets/` | SVG logo source and future screenshots |

```bash
cargo tauri dev                                      # Run in development mode
cargo test -p ember -p shared --locked                # Backend and shared-type tests
cargo clippy -p ember -p shared --all-targets --locked # Static analysis
```

An optional test can query a real LiteLLM instance. It requires both `OPENAI_API_KEY` and `LITELLM_BASE_URL` to be set in the environment; there is no default endpoint. These variables are used only by the test; the application uses its settings and the Keychain.

```bash
cargo test -p ember live -- --ignored --nocapture
```

See [AGENTS.md](AGENTS.md) for contribution guidelines.
