# Historical Daily Usage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Browse daily usage for any retained past day while all budget-period figures remain current.

**Architecture:** Add a date-specific, generation-guarded backend query returning daily model usage separately from live Stats. Keep selection and the historical request slot at App scope; reuse existing charts and Remote request protection. Today continues to use the live Stats feed.

**Tech Stack:** Rust 2021, Tauri 2, Leptos 0.8, Chrono 0.4, js-sys, httpmock, Trunk.

**Spec:** `docs/superpowers/specs/2026-10-22-historical-day-usage-design.md`

## Global Constraints

- Budget-period figures stay current, including header, gauge, period model costs, and menu-bar amount.
- Date selection affects daily spending, total tokens, input/output token split, and both per-model breakdowns.
- Use local calendar days, French UI wording, and no History-calendar 30-day limit.
- Today is the default live mode. Preserve selection during tab and settings navigation within the running app.
- Reject invalid/future dates; never show another day's data under the selected date.
- Fetch historical usage on demand, with bounded caching and no historical background polling.
- Keep credentials in the backend and preserve serialized settings/cache compatibility.
- Reuse existing dependencies. No changes to History or Activity behavior.

## Review Focus

- Local midnight and daylight-saving changes must not lose requests or include adjacent local days: Task 1 range/aggregation tests.
- Rapid date changes and connection changes must not publish obsolete responses: Task 1 service test and Task 2 Remote tests.
- Today crossing midnight must not relabel yesterday's cached rows as today's usage: Task 2 selection tests and browser check.
- Failed or empty historical queries must not display a misleading zero or another day's figures: Task 2 state tests and browser check.
- A refresh or live event during historical viewing must preserve selection and current budget figures: Task 1 snapshot test and Task 2 integration check.

## Files and responsibilities

- `shared/src/lib.rs`: add `DailyUsage { date: String, models: Vec<ModelUsage> }`.
- `src-tauri/src/litellm.rs`: validated date queries and a single-entry aggregate cache independent of live raw rows.
- `src-tauri/src/service.rs`: connection-generation protection for historical usage.
- `src-tauri/src/tray.rs`: register the new Tauri command.
- `ui/src/tauri.rs`: typed command bridge.
- `ui/src/usage.rs` (new), `ui/src/main.rs`: date controls and testable daily display helpers.
- `ui/src/app.rs`: App-scoped selection/request state and chart integration; retain existing period chart data source.
- `ui/src/details.rs`: exercise existing Remote transitions with dated results; expose the existing status component within the crate if reused.
- `ui/style.css`: compact, keyboard-accessible date navigation styles.
- `README.md`: document daily navigation, local dates, and retention limitations.

## Task 1: Date-specific daily usage query

**Interfaces:**
- `DailyUsage` derives `Debug, Clone, Default, PartialEq, Serialize, Deserialize`.
- `Client::fetch_day(&self, date: &str, force: bool) -> Result<DailyUsage, String>`.
- `Service::day_usage(&self, date: String, force: bool) -> Result<Detail<DailyUsage>, String>`.
- Tauri command `get_day_usage(date: String, force: bool, service: State<'_, Service>)` returns `Result<Detail<DailyUsage>, String>`.

- [ ] **1. Establish the baseline.** Use the existing dedicated clone on `feat/historical-day-usage`. Rust, rustup, and Trunk were absent from PATH during planning; provision the standard stable Rust toolchain with rustfmt, Clippy, and `wasm32-unknown-unknown`, plus Trunk if still missing. Run `cargo test -p ember -p shared -p ui --locked` before product edits and record failures.
- [ ] **2. Write failing client tests.** Add `historical_day_filters_local_boundaries_and_aggregates_models`, `historical_day_rejects_invalid_and_future_dates_before_http`, and `historical_cache_is_date_specific_and_force_bypasses_it`. Use synthetic logs immediately before, at, and after local midnight; assert only the requested date's costs/tokens count. Check invalid leap dates, empty input, timestamp input, tomorrow, and a valid date older than 30 days. Repeated date hits once, another date hits again, force hits again; live-row cache remains independent.
- [ ] **3. Run the failing tests.** Run `cargo test -p ember historical_ --locked`; confirm failure is the missing date-query behavior.
- [ ] **4. Implement the query.** Parse canonical `YYYY-MM-DD` with Chrono and compare against the current local date before network access. Compute a UTC-date query envelope covering both local-day boundaries, rounding the exclusive end upward when needed. Resolve repeated midnight to its earliest instant; if midnight does not exist, use the conservative UTC range from the prior date through two dates after the requested date. Filter returned logs with existing `aggregate_today(rows, day, &Local)`. Cache one aggregated `DailyUsage` for 60 seconds, keyed by date; force bypasses that cache. Do not retain extra historical raw logs.
- [ ] **5. Test boundary range behavior.** Make the date-range helper timezone-generic for tests. Test east/west UTC offsets and a synthetic TimeZone with a spring-forward/fall-back transition, including missing/repeated midnight. Assert the queried envelope covers all relevant UTC instants; adjacent local dates are filtered out. Avoid process-global timezone mutation.
- [ ] **6. Add failing service tests, then wire the command.** `historical_usage_does_not_modify_live_stats` compares the complete Stats snapshot before/after a day query once the background cycle is idle. `historical_usage_rejects_old_connection_response` delays the historical HTTP response, changes settings, and expects an obsolete-generation error. Follow `Service::activity`'s Context/select/detail pattern and register `get_day_usage` beside `get_activity`.
- [ ] **7. Verify and commit.** Run `cargo test -p ember -p shared --locked` and `git diff --check`. Commit the shared type, client, service, and command as `feat: fetch usage for a selected local day`.

## Task 2: Daily navigation and integration

**Interfaces:**
- `tauri::get_day_usage(date: String, force: bool) -> Result<Detail<DailyUsage>, String>`.
- App-scoped `selected_day: RwSignal<Option<String>>` (`None` means live Today), plus `day_usage: RwSignal<Remote<DailyUsage>>`.
- `usage::visible_usage(selected: Option<&str>, today: &str, stats: &Stats, historical: Option<&DailyUsage>) -> Option<Vec<ModelUsage>>` returns only rows belonging to the displayed day.
- `usage::normalize_selection(date: &str, today: &str) -> Option<String>` maps today to live mode; caller validates input before calling.
- `usage::DayNavigation` receives the selection and current local date signals and a selection callback. Keep date stepping inside this focused component.

- [ ] **1. Write failing UI state tests.** Test `visible_usage`: historical selection reads the matching historical response, never Stats.today; mismatch returns None; live mode requires `Stats.today_date == today`. Test normalization returns None for today and retains a past date. Extend Remote tests with DailyUsage for A→B→A out-of-order responses, reset during a pending request, a connection-generation change, and failed/empty successful responses. Assert failed first load has no data while successful empty data is present.
- [ ] **2. Run the failing tests.** Run `cargo test -p ui --locked`; confirm failures come from the missing helpers or changed daily behavior.
- [ ] **3. Implement state and bridge.** Store selection and request slot in App, above settings/tab mounts. Reset historical Remote on selection or connection changes, then use `details::load` with a date-capturing closure and the existing generation signal. Never subscribe historical fetching to every live Stats revision. Use the visible-data helper to prevent stale-day display. Reset invalidated historical data when settings close while retaining selection.
- [ ] **4. Add navigation and daily rendering.** Add a native `input type="date"`, previous/next buttons, and Aujourd'hui. Set max to the current local date and disable next while viewing today. Use existing js-sys local calendar getters/setters for navigation, not UTC ISO slicing or 24-hour timestamp arithmetic. Reject empty/invalid/future input without changing selection. Recompute the local date on mount and Stats updates; keep live mode advancing without changing a fixed past selection. Render historical loading/error/empty states, date label, and local-time hint. Reuse existing donut, token split, and bars with selected-day data; period bars continue reading `Stats.models_total`.
- [ ] **5. Integrate manual refresh and document behavior.** Reload the currently selected past day with `force: true` through the existing header action, independently of completion of the live refresh. Capture the correct selection and reset request identity so an older response cannot win. Keep controls usable during loads. Add French accessible labels, visible focus, disabled styles, and compact layout. Document server retention and the distinction between local daily usage and UTC History.
- [ ] **6. Run checks.** Run `cargo fmt --all -- --check`, `cargo test -p ember -p shared -p ui --locked`, `cargo clippy -p ember -p shared --all-targets --all-features --locked -- -D warnings`, and `cargo clippy -p ui --target wasm32-unknown-unknown --all-targets --all-features --locked -- -D warnings`. Run `trunk build --release --locked` from `ui/`. Investigate baseline-only failures separately from this diff.
- [ ] **7. Exercise the built UI.** Serve the built frontend with a temporary mocked Tauri bridge, without real credentials. Verify yesterday, a date older than 30 days, month/year/leap-day navigation, future-date prevention, Today, loading, empty and error states, rapid changes, tab/settings return, forced refresh, live updates while viewing history, and midnight rollover. Assert the header, gauge, and period-model figures remain current. Capture a screenshot of historical usage. Native date-picker behavior can receive an additional macOS app check if available.
- [ ] **8. Review and commit.** Review the entire feature diff against the spec, record actual verification results, and commit as `feat: browse daily usage with date navigation`. Keep the local branch ready for the user's chosen publication path.

## Execution recommendation

Use native execution in this session: the two tasks share a small, tightly coupled interface. Perform one independent whole-branch review after implementation. Obtain the user's plan/execution confirmation before starting product edits.

## Execution results

Both tasks were implemented with native execution after approval. An independent whole-branch review found three issues: historical requests serialized behind obsolete requests, local midnight waited for the next Stats event, and refresh from another tab skipped the selected historical day. All three were reproduced and fixed with regression checks.

The midnight fix adds a one-second local UI clock and rechecks the date during navigation; this replaces the plan's Stats-only clock updates without adding network requests. Historical HTTP requests run outside the cache lock, and only the latest request may publish its cached result. Manual refresh reloads the retained historical selection from any tab.

Final verification: 193 tests passed, 3 optional live tests ignored; rustfmt, backend/shared Clippy, WebAssembly UI Clippy, and the release Trunk build passed. Built-WebAssembly browser checks covered navigation, leap/year boundaries, invalid/future dates, loading/error/empty states, date and connection races, current budget isolation, tab/settings persistence, cross-tab refresh, and midnight without a Stats event. These browser checks used synthetic Tauri responses, not a live LiteLLM server or the native macOS webview.
