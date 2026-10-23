# Adversarial Review Fixes Implementation Plan

> **For agentic workers:** Use `executing-plans` to implement and verify each task inline.

**Goal:** Fix all eight reproduced review findings before publishing the historical-usage branch.

**Architecture:** Validate daily responses at the transport boundary. Carry validation timestamps across cache layers, use a shared refresh identity for cost and usage requests, and preserve compatible values until replacements succeed. Check selected-day timezone boundaries independently of the heatmap window.

**Tech Stack:** Rust, Tokio, Tauri 2, Leptos 0.8, existing JSON persistence and HTTP mocks.

**Spec:** The eight findings in the full-branch review, accepted by the user with “can we fix all of that”; existing historical-day and local-heatmap specifications remain applicable.

## Global Constraints

- Continue on `feat/historical-day-usage` with existing uncommitted cache changes.
- Keep API keys in Keychain; only their hash may appear in endpoint filters.
- Keep current budget state separate from selected historical dates.
- Preserve French UI wording and comments; add no dependencies.
- Keep 30 daily cache entries, 900-second past TTL and 60-second today TTL.
- Never persist raw logs or treat absent counters as zero.

## Review Focus

- Both legacy and paginated responses must preserve key scope and reject missing relevant counters.
- A new forced refresh must supersede old requests while its two consumers share boundaries.
- Costs and complete usage must persist in either completion order without renewing old freshness.
- Failed refreshes must preserve displayed figures and healthy memory/disk values.
- An older selected date must reload when its timezone bounds change, even if recent heatmap bounds do not.

### Task 1: Validate daily inputs (findings 1–3)

Files: `src-tauri/src/litellm/{daily,costs}.rs`, existing neighboring tests.

- [x] Add regressions for legacy key filtering, missing/null/invalid spend and counters on both endpoints, and unreconciled summary/model totals.
- [x] Run each regression before changing its production path; expect failure.
- [x] Add key hash to legacy daily query; validate only relevant day rows before aggregation; reject invalid model summaries.
- [x] Run daily/analytics/cost tests; expect all pass.

### Task 2: Correct refresh identity and cache metadata (findings 4, 5, 8, backend part of 6)

Files: `litellm/{daily,costs,ranges,analytics}.rs`, `litellm.rs`, `daily_store.rs`, `service.rs`, `tray.rs`, `ui/src/{tauri,app,history,details}.rs`.

- [x] Add deterministic regressions for pre-force in-flight boundaries, reverse cost/usage completion, original validation timestamps, and failed-refresh cache retention.
- [x] Give paired frontend force requests one refresh token. Resolve that token to a connection/day attempt shared by both consumers; reject obsolete persistence.
- [x] Carry each result's original validation timestamp through memory hits. Track persisted cost and usage validation independently.
- [x] Preserve successful memory values while registering newer requests; update only on validated success.
- [x] Run cache/service/concurrency tests in default timezone and UTC; expect all pass.

### Task 3: Retain UI data and validate selected timezone (findings 6–7)

Files: `ui/src/{details,usage,app,history}.rs`.

- [x] Add a Remote transition regression and built-WASM failed-force/old-date-timezone checks; observe failures first.
- [x] Add a refresh transition that supersedes request identity while retaining compatible data, marked stale until validation.
- [x] Compute selected local midnight bounds and check them in the existing local clock and async response guards.
- [x] Verify offline retention, rapid dates, source changes, timezone changes, and force supersession in the built UI.

### Task 4: Verify, review, publish

- [x] Run `cargo test -p ember -p shared -p ui --locked`, both Clippy targets with `-D warnings`, formatting, diff checks, release UI and native builds.
- [x] Run a fresh adversarial review across the entire branch and address material findings with regressions.
- [ ] Update README/PR evidence and review-thread replies; commit and push to the existing feature branch under the prior publication request.
- [ ] Verify macOS CI and relaunch the native feature build.

## Execution decisions

The user approved the enumerated fixes; this is corrective work within the existing feature design. Continue inline. The canonical task ledger is `litellm-ember-github-review` in work state. Keep the current feature checkout to preserve the reviewed uncommitted work; consolidate commits after integrated validation.

## Verification record

- Initial scope/validation regressions failed, then passed with 210 backend tests.
- Cache completion-order, failed-force memory retention and older in-flight boundary regressions failed before fixes.
- The initial forced-refresh Remote test lost its value; the new refresh transition preserves it and rejects old completions.
- Integrated workspace after the final fix pass: see the final verification below.
- Both Clippy targets, formatting, diff checks, release WASM and native development builds passed.
- Built-WASM scripts verify stale JSON previews, failed-refresh retention on both screens, shared force tokens, late replies after date changes, January Paris/Johannesburg timezone invalidation, and heatmap loading without detail requests.
- Fresh adversarial review found four Important issues. All six scratch reproductions failed as reported. The fix pass adds model validation to daily and both boundary paths, checked per-model and global aggregation, and cross-layer stale-usage revalidation. Its Rust regressions failed before the fixes and now pass.
- Same-date selections on both screens initially created two refresh attempts per click. The built-WASM regression failed on that assertion; grouping targets by date now shares one request pair and passes the browser check.
- The incompatible-usage regression also covers dates outside the persisted window. Both memory usage readers now compare newer cached costs before reusing complete usage; this test failed before the compatibility check.
- No review findings were deferred. Native/browser checks remain parent-owned and were run. Live analytics permission and production server compatibility remain environment-dependent; the configured gateway denies analytics, while mock success and fallback tests cover the contract.

### Final verification

259 workspace tests passed; 6 optional live tests ignored. The UTC daily subset passed 34 tests with 1 ignored. Backend/shared and WebAssembly Clippy with warnings denied, formatting, diff checks, release frontend build and native development build passed. Both built-WASM regression scripts passed after the final changes.

### Live compatibility correction

After publication, the user reported model-validation and summary-reconciliation errors. Sanitized gateway inspection found seven failed requests on October 22 with empty model names and explicitly zero spending and token counters. The summary contained an empty model bucket with zero cost; its totals reconciled exactly. Equivalent summary buckets also occurred on October 8, 13 and 20.

The daily and boundary paths now exclude only explicitly failed, unnamed rows whose spending and all three counters are present and zero. Summary validation accepts unnamed buckets only at zero cost. Nonzero usage, absent counters and unknown/success status remain rejected. Three regressions failed before the correction and pass afterward, including both legacy and paginated sources. Workspace verification: 262 passed, 6 ignored; backend/shared Clippy passed. Real Rust cost-preview/full-usage comparisons matched per-model totals for October 20–23. The native build was rebuilt and relaunched.
