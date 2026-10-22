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
- **Prompt cache usage**: reused input tokens, their share of reported input tokens, and data coverage, with a daily breakdown by model in Activity.
- **Unusual spending alerts**: a daily rolling comparison of the last completed week against the previous two weeks, with configurable percentage and dollar thresholds.
- **macOS notifications**: two configurable thresholds, set to 50% and 75% of the budget consumed by default.
- **Integrated updates**: checks at startup and every 24 hours. Install a signed update from Settings, then restart when ready.

The panel uses a dark theme with Liquid Glass on macOS 26 and native vibrancy on earlier versions. The History and Activity tabs load their data when opened and reuse it for 60 seconds. A refresh button lets you reload the data manually.

## Local agents

The **Agents** tab appears while a local Codex, Claude Code, or OpenCode task is
running or waiting for your input. It disappears after the last task finishes,
returning to Usage if selected. Merely opening a terminal or installing a harness
does not show the tab. This feature works independently of your LiteLLM connection.

Open **Paramètres → Harnais** to inspect detection and coverage or change a data
folder. Codex and Claude Code are read passively from their local transcripts.
Ember does not install hooks or modify their configuration. Previously installed
Ember hooks remain supported.

OpenCode uses the API of an existing local instance, discovered through its process.
Both the original API and the v2 API are supported. For the local v2 service,
Ember reads OpenCode's service metadata to authenticate automatically, keeping
the password only in memory. Tabs belonging to an open OpenCode terminal remain
visible while idle; saved tabs in other project directories are not imported.
You can also specify a loopback HTTP address and store its password in the macOS
Keychain through these settings (the username is `opencode`). Ember does not
launch an OpenCode server. Remote sessions are not supported.

Cards show the current prompt, agent hierarchy, model, tools, duration, observed
skills and token usage when the harness exposes them. Reading a `SKILL.md` file
is distinguished from explicitly invoking a skill. Unsupported or ambiguous
activity is reported in Settings; it is not invented from file modification times.
Ember keeps prompt text in memory only, never in its caches, logs, or network requests.

Costs are **API equivalents**, not subscription charges or additions to LiteLLM
spending. Unambiguous `/model/info` prices from your configured LiteLLM instance
have priority over the dated bundled public catalogue. Input, output and cache
usage are counted separately and responses are deduplicated. Unknown models stay
unpriced; incomplete usage, truncated transcripts and standard rates that may have
tiered variants are marked partial. Harness transcript formats can change: the
current fixtures cover Codex 0.162, Claude Code 2.1 and OpenCode 1.x protocols.

Normal tests use synthetic records and mock HTTP servers. An optional diagnostic
reads local sessions without printing prompts:

```sh
cargo test -p ember live_local_discovery -- --ignored --nocapture
```

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

Official releases use a local **ad hoc** macOS signature and are **not notarized by Apple**.
No Apple Developer subscription is required. Gatekeeper may block the first launch:
after moving Ember to Applications and attempting to open it, go to **System Settings →
Privacy & Security → Open Anyway** for Ember and confirm. Follow this procedure only for
the release you intended to install. Ember does not automatically remove quarantine.
The separate Tauri signature authenticates update archives; it does not grant Apple approval.

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
install their own updater. Subsequent updates use the integrated flow. Replacing an ad hoc signed app may require macOS Keychain authorization again;
Ember does not delete or migrate the existing key.

### Release signing setup (maintainers)

Only the Tauri updater keys are required. No Apple certificate, account, or notarization
credentials are used. Generate the production key **once**, outside this repository:

```sh
mkdir -p "$HOME/.config/ember-signing"
chmod 700 "$HOME/.config/ember-signing"
umask 077
cargo tauri signer generate --write-keys "$HOME/.config/ember-signing/ember.key"
```

Choose a nonempty password at the prompt. Do not use `--force` to overwrite an existing
key. Keep an offline backup of the private key and its password. Never commit them or
paste them into an issue, chat, or build log. The `.pub` file already contains the
base64-encoded public key expected by Tauri; do not encode it again.

With `gh` authenticated to the repository, configure GitHub Actions:

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo clementreiffers/litellm-ember < "$HOME/.config/ember-signing/ember.key"
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo clementreiffers/litellm-ember
gh variable set TAURI_SIGNING_PUBLIC_KEY --repo clementreiffers/litellm-ember < "$HOME/.config/ember-signing/ember.key.pub"
```

The second command prompts for the same password; do not put it in a command argument.
The private key and password are **secrets**; the public key is a repository **variable**
embedded in official builds. Keep the same pair across releases: replacing it without a
migration prevents existing installations from verifying future updates. Never use the
repository's test fixtures or temporary build-validation keys for production.

Pull requests and local development need no signing secrets. Local builds have no updater
trust key and cannot install official updates. The first release with your production
public key must be installed manually from its DMG.

The release job builds a universal Intel/Apple Silicon app with `signingIdentity: "-"`
and no hardened runtime. It verifies the app's ad hoc signature, embedded version,
universal architecture, DMG integrity, and the extracted updater app. It authenticates
the update archive and its signed version using the Tauri public key before publishing
`latest.json` and checksums. No Gatekeeper acceptance or notarization is claimed.
A missing Tauri setting or failed validation stops publication. Do not pass `--no-sign`
to a release build: Tauri 2.12.1 also skips updater signatures with that option.
A checksum alone is never used as proof of updater authenticity.

Each release contains the DMG, `.app.tar.gz`, `.app.tar.gz.sig`, `latest.json`, and
`SHA256SUMS.txt`. Both macOS architectures in the manifest point to the same universal
archive at a version-specific URL. The archive and its signature are technical assets;
users only download the DMG for initial installation. A temporarily missing manifest or
asset during publication is reported as an error and can be retried.

Before shipping the first release with integrated updates, follow [the macOS update validation checklist](docs/macos-updates.md).

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

### Daily usage navigation

In **Consommation**, use the date picker or previous/next-day buttons to inspect daily costs, tokens, and model breakdowns. **Aujourd'hui** returns to today's live usage. Dates follow your Mac's local calendar; future dates are unavailable. The selected date stays selected while you switch tabs or open Settings, until you choose another date or restart Ember.

Past days load on demand, and **Rafraîchir tout** reloads the selected day's figures. You can select dates older than 30 days, provided your LiteLLM server still retains their logs. An empty response means no usage was returned; Ember cannot distinguish an unused day from logs removed by server retention. Loading and network errors do not display another day's figures.

Costs and model costs appear first from a lightweight spending summary while token details load separately. Because summaries use UTC days, Ember adds or subtracts the boundary intervals needed to match your local day. Token charts stay hidden until their counters arrive, rather than showing zero. If the summary or its boundary correction is unavailable, the full usage request still supplies the figures. Once complete details arrive, they take precedence over the preview.

Historical loading uses `/spend/logs/v2` with exact local-day boundaries converted to UTC, 1,000 records per page, and up to three concurrent page requests. This endpoint omits prompt and response bodies. Ember checks that all pages are complete before showing totals. Servers that deny or lack this endpoint, or cap the result count, use the original `/spend/logs` query instead.

If the legacy endpoint also reports truncation, Ember shows an error instead of presenting incomplete totals as a full day's usage.

Up to 30 recently visited daily aggregates stay in memory for 15 minutes, making return visits fast. This is a cache limit, not a limit on selectable dates. Today's data expires after 60 seconds. **Rafraîchir tout** bypasses the selected day's cache; changing the endpoint or API key clears connection-specific data. No historical request logs are written to disk.

Cost previews are cached with the same 30-day capacity and 15-minute expiry, even while token details are still loading. Revisiting a day reuses available values during the running app session. Restarting Ember clears these in-memory caches.

For complete usage, Ember first tries `/user/daily/activity/aggregated`, scoped to the key's hash and its user ID from `/key/info`. This endpoint can return model costs and token counters without downloading the whole day's request logs. Its historical buckets are UTC, so Ember corrects the local-day boundaries for costs and tokens. Forbidden, missing, slow, malformed, or incomplete analytics responses fall back to the paginated endpoint described above. An unavailable analytics source is retried after 15 minutes, or immediately through **Rafraîchir tout**. API keys without permission continue to work through the existing endpoints.

The header, budget gauge, period model costs, and menu-bar amount always describe the **current budget period**. The History calendar below uses UTC dates, which may differ from the local days used in Consommation.

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

The **Cache de prompts** card in Activity tracks input tokens reused by providers,
separately from LiteLLM's response cache. It uses normalized
`metadata.usage_object.prompt_tokens` and
`metadata.usage_object.prompt_tokens_details.cached_tokens` from the existing spend
logs. Availability depends on your LiteLLM version and logging configuration.

The percentage is the sum of reused tokens divided by the sum of input tokens
for requests with valid counters, rather than an average of request percentages.
The card shows how many eligible requests have these counters. Missing data is
unavailable, not zero; an explicitly reported zero is included. Failed requests
and explicit LiteLLM response-cache hits are excluded. Figures cover the current
local day and reuse Activity's existing refresh behavior. Cache writes and dollar
savings are not included.

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
