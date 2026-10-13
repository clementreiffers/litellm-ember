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
- **The History tab**: a GitHub-style calendar of daily spending over the last 30 days, including today, with an offline cache. It also shows daily spending, a comparison with yesterday, and an end-of-period projection.
- **The Activity tab**: requests, errors, cache hits, latency, and hourly activity, with a breakdown by model.
- **Unusual spending alerts**: a daily rolling comparison of the last completed week against the previous two weeks, with configurable percentage and dollar thresholds.
- **macOS notifications**: two configurable thresholds, set to 50% and 75% of the budget consumed by default.
- **Integrated updates**: checks at startup and every 24 hours. Install a signed update from Settings, then restart when ready.

The panel uses a dark theme with Liquid Glass on macOS 26 and native vibrancy on earlier versions. The History and Activity tabs load their data when opened and reuse it for 60 seconds. A refresh button lets you reload the data manually.

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

> 📸 Screenshot coming soon: the History tab.

<!-- ![History tab showing daily spending and the budget projection](assets/screenshots/semaine.png) -->

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
2. Download `Ember-X.Y.Z-macos-universal.dmg`.
3. Open the DMG, drag **Ember.app** to **Applications**, eject the DMG, and launch Ember from Applications.

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

Official releases use an Apple Developer ID signature and Apple notarization. macOS may display its normal first-open confirmation; removing `com.apple.quarantine` with `xattr` is not part of installation. Older unsigned releases retain their previous macOS restrictions.

### Updating Ember

The main panel displays the version of the installed GitHub release, embedded during
the release build. When no release version was supplied (for example, a local build),
Tauri falls back to the backend's Cargo version. Displaying this version works offline.

Official builds check the latest stable GitHub release independently of your LiteLLM
connection. Ember sends one notification per available version, remembering it across
restarts. Budget notification preferences do not disable update notifications.

Click the notification to open **Paramètres → Mises à jour**, then **Mettre à jour**.
Ember downloads the universal update, verifies its signature, and installs the entire
application bundle while the current process keeps running. Once **Redémarrer** appears,
everything is installed: restarting needs no network connection. You can also quit and
reopen Ember later. **Notes de version** opens GitHub without starting an installation.
Settings, cached statistics, and the Keychain entry are preserved.

Checks run at startup and every 24 hours independently of LiteLLM, without a GitHub token.
Download, verification and installation errors are shown in Settings with a retry action.
Only newer stable versions are offered. Installation from a mounted DMG or a translocated
copy is refused: move Ember to Applications and launch that copy first. If installation
requires administrator rights, macOS displays its authorization dialog; cancelling keeps
the current application. No automatic quarantine removal is performed.

**Transition from older versions:** install the first release with this updater manually
from its DMG, replacing the previous app after quitting it. Existing versions cannot
install their own updater. Subsequent updates use the integrated flow. The transition
from an unsigned app to Developer ID may require a one-time Keychain authorization;
Ember does not delete or migrate the existing key.

### Release signing setup (maintainers)

A paid Apple Developer membership is required for official releases. Configure these
GitHub Actions repository secrets before the first signed release:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | Base64-encoded `.p12` export of the Developer ID Application certificate **and its private key** |
| `APPLE_CERTIFICATE_PASSWORD` | Password protecting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full `Developer ID Application: Name (TEAMID)` identity |
| `APPLE_API_KEY` | App Store Connect team API key ID authorized for notarization |
| `APPLE_API_ISSUER` | API key issuer ID |
| `APPLE_API_PRIVATE_KEY` | Contents of the corresponding `.p8` file |
| `TAURI_SIGNING_PRIVATE_KEY` | Tauri updater private key generated below |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Nonempty password protecting that updater key |

Generate the updater key once with `cargo tauri signer generate --write-keys /secure/path/ember.key`
and choose a password. Store the generated private key securely outside the repository,
with an offline backup. Set the repository **variable** `TAURI_SIGNING_PUBLIC_KEY` to the
contents of `ember.key.pub` (already base64 encoded by Tauri). This public key is embedded
in official builds. Keep the same updater key and Apple signing identity across releases;
replacing the updater key without a transition would strand existing installations.

The Apple certificate signs the application for macOS; the separate Tauri key authenticates
updates. Neither private key is shipped in the app. Pull requests and local development
need no signing secrets. Local builds have no updater trust key and cannot install official
updates; use the signed release to exercise the full flow.

The release job builds an Intel/Apple Silicon universal app, signs and notarizes it, and
staples its ticket before producing the updater archive. It also notarizes and staples
the DMG. Before publication it checks the signatures, Gatekeeper assessment, stapled tickets,
universal architecture, embedded version, and updater signature against the public key.
A missing secret or failed validation stops publication. A checksum alone is never used
as proof of updater authenticity.

Each release contains the DMG, `.app.tar.gz`, `.app.tar.gz.sig`, `latest.json`, and
`SHA256SUMS.txt`. Both macOS architectures in the manifest point to the same universal
archive at a version-specific URL. The archive and its signature are technical assets;
users only download the DMG for initial installation. A temporarily missing manifest or
asset during publication is reported as an error and can be retried.

Before shipping the first signed release, follow [the macOS update validation checklist](docs/macos-updates.md).

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

The application interface is currently in French. The tab names used above correspond to **Consommation** (Usage), **Historique** (History), and **Activité** (Activity).

1. Click the amount in the menu bar, then **⚙︎**.
2. The endpoint is initially empty. Enter your LiteLLM instance URL, including any path prefix, such as `https://litellm.example.com/llm`.
3. Add your API key and choose a refresh interval between **5 and 3,600 seconds**.
4. Save your settings, then click **Envoyer une notification de test** (Send a test notification) to check notification delivery and macOS permissions.

Your key must have access to the `/key/info` and `/spend/logs` endpoints. Without a saved key, the application makes no requests to LiteLLM.

### Local storage

After upgrading from a build with the previous application identity, enter your endpoint and API key again. Existing local data and Keychain entries are left untouched and are not imported automatically. macOS may ask you to allow notifications again.

Your API key is stored in the **macOS Keychain** and is never sent back to the interface. Preferences, the latest statistics, and alert state are stored locally in the application's data directory. The cache restores your latest figures as soon as the app starts.

Alerts remember which thresholds have already been reported. They reset when a new budget period starts or when spending drops below the relevant threshold.

### Daily spending calendar

Open **Historique** for a GitHub-style calendar: weeks run left to right and Monday through Sunday run top to bottom. The 30-day rolling window includes today, which is marked **en cours**. All dates are UTC. Hover, focus with the keyboard, or select a square to read its date and cost.

Purple intensity represents spending relative to the largest known amount in the visible window: a neutral square for zero and four levels for positive spending. Hatched squares mean unknown data, not zero. The total is marked incomplete while any day is unknown. Existing projections, yesterday comparisons and spending charts remain below the calendar.

Ember loads a small, versioned `history.json` cache once and keeps at most 30 daily totals in memory. At first synchronization and on a new UTC day, it fetches all 30 days of LiteLLM summaries to incorporate corrections. Later visits update only today, at most once per minute; **Rafraîchir tout** requests the full window. No extra polling loop or detailed request logs are needed for the calendar. Network failures preserve the cached figures and show an error. Changing the endpoint or key clears the displayed history.

This history is shared with unusual-spending detection, which still compares 21 completed days and ignores today. With those alerts disabled, calendar data is fetched only on demand. Days missing from a successful server response count as zero; server retention limits can still make an apparently complete window underestimate actual spending.

### Unusual spending alerts

In **Paramètres → Hausse inhabituelle des dépenses**, enable or disable this alert independently of budget notifications. It is enabled by default, including for existing installations, with two adjustable thresholds: **50% above normal** and **at least $3 extra**. Both conditions must hold. Your refresh interval is unchanged.

Once per UTC day, Ember compares the spending of the **7 most recent completed days** with half the spending of the **14 preceding days**. The windows do not overlap and exclude today. For example, a $10 weekly reference and $16 spent in the recent week trigger an alert. When the reference is zero, only the minimum dollar increase applies.

Ember uses LiteLLM's daily summaries immediately, independently of budget resets. This requires **21 days of complete server history**: missing days in a successful response count as zero, and LiteLLM's response does not establish its retention coverage. Truncated history can therefore underestimate the reference. This detects a weekly increase, not a runaway request loop in real time.

Only one notification is sent per episode, including across restarts or disabling and re-enabling the option. A successful daily comparison below either threshold rearms the alert. Changing the thresholds triggers a new comparison without forgetting an already reported episode; changing the endpoint or key starts fresh. Network and notification failures are retried on the first refresh after 15 minutes, without rearming the alert. When the option is disabled, no background history request is made for these alerts; the calendar can still refresh on demand.

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
