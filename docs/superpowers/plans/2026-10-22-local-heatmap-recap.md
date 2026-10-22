# Local-Day Heatmap and Daily Recap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Continue the user's previously selected native execution with one independent final review. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a local-day heatmap with a clickable daily recap below it, sharing costs, token details, and caches with Consommation.

**Architecture:** Keep the existing UTC history path for spending-increase notifications. Add a local calendar snapshot, built progressively from one summary query and deduplicated midnight corrections. Publish revisioned snapshots through a watch channel and Tauri events; use existing daily commands for the selected recap and extract the shared daily chart rendering.

**Tech Stack:** Existing Rust, Chrono, Tokio, reqwest, Tauri v2, Leptos, and Trunk. No new dependencies planned.

**Spec:** `docs/superpowers/specs/2026-10-22-local-heatmap-recap-design.md`

## Global Constraints

- Show the last 30 local calendar dates, including local today.
- Recap appears below the heatmap; no recap until a date is activated. Focus alone does not activate a date.
- Both screens share per-connection daily caches: 30 dates, 15-minute freshness for past dates, 60 seconds for today.
- Preserve independent main-screen selection and current-budget figures.
- Never relabel UTC totals as local-day totals or display missing token counters as zero.
- Only the selected recap loads token details; opening History must not download full logs for all 30 days.
- Preserve established UTC spending-increase notification semantics.
- Persist only aggregate historical costs and nonsecret cache metadata. API keys stay in Keychain.
- Match existing French UI/comments and repository conventions. Keep the existing PR and feature branch.
- Run Rust commands with `PATH="$HOME/.cargo/bin:$PATH"`.

## Review Focus

- A timezone changes without changing today's date: reject incompatible cached values and in-flight publications. Task 1 tests bounds-based identity; Task 3 tests rebasing and stale events.
- A new recap refresh finishes before an older heatmap correction: the older result must not replace the newer cost. Tasks 2–3 test per-date publication ordering.
- A slow or failed correction occurs beside valid days: publish valid local totals progressively and retain compatible stale values with an error. Tasks 2–3 test partial completion.
- A midnight is ambiguous or missing: choose the earliest valid midnight consistently, or show unknown/error rather than guessing. Tasks 1–2 test both cases.
- Settings, tab changes, local midnight, or close/reopen occur during loading: retain valid selection, close dates outside the window, and reject old responses. Task 4 covers these in browser tests.

## File Map

- New `src-tauri/src/litellm/calendar.rs`: local-day bounds and rolling-window identity.
- Existing `litellm/{daily,costs,analytics}.rs`: reuse calendar boundaries; share timezone-safe daily caches and guarded cost publications.
- New `src-tauri/src/litellm/local_history.rs`: one range summary, deduplicated boundary corrections, incremental daily costs.
- New `src-tauri/src/local_history.rs`: persisted local calendar state and deterministic refresh/publication transitions.
- Existing `service.rs`, `tray.rs`, `main.rs`, `shared/src/lib.rs`: local calendar coordination, commands, and event snapshots.
- New `ui/src/daily_view.rs`: shared daily figures and charts, used by main usage and recap.
- Existing `ui/src/{app,history,details,tauri,usage,main}.rs`, `ui/style.css`: recap state, calendar events, local labels, and compact rendering.
- Existing `README.md`: local-day calendar, loading, caching, and recap documentation.

### Task 1: Give local days explicit bounds and cache identity

**Interfaces:** Produce `DayBounds { date: NaiveDate, start: DateTime<Utc>, end: DateTime<Utc> }` and `day_bounds(day: NaiveDate, zone: &impl TimeZone) -> Option<DayBounds>` in `litellm/calendar.rs`. Produce `CalendarWindow::new(today: NaiveDate, zone: &impl TimeZone)` with 30 dated optional bounds and a deterministic `id: String` derived from those dates/bounds. Bounds determine equivalence; two timezone names with identical day intervals need no invalidation.

- [ ] Write `local_bounds_cover_dst_and_missing_midnight` using existing `ShiftZone` fixtures. Assert spring/fall intervals are 23/25 hours, ambiguous midnight chooses earliest, and missing midnight returns `None`.
- [ ] Write `daily_caches_do_not_cross_calendar_boundaries`: the same date at UTC and UTC+02:00 cannot share a result; identical bounds can. Assert changed bounds during a request prevent that result entering the current cache.
- [ ] Run focused tests. Expected: FAIL because the new boundary/cache behavior is absent.
- [ ] Implement helpers; use exact bounds in full-usage and cost-cache lookup/publication. Reuse them in preview and analytics corrections. Preserve the existing broad-range fallback for missing-midnight full usage, but do not cache that result under guessed boundaries.
- [ ] Run `cargo test -p ember -p shared -p ui --locked`. Expected: all pass, including existing analytics/cost tests.
- [ ] Commit: `refactor: key daily caches by local calendar bounds`.

### Task 2: Load local heatmap costs progressively without whole-month detail downloads

**Interfaces:** Consume Task 1 bounds. Produce `Client::fetch_local_history(today: NaiveDate, start: NaiveDate, force: bool, publish: impl Fn(DailyCosts) + Send + Sync) -> Result<(), String>`. Provide an internal timezone-injected equivalent for tests. Each callback represents a verified local date; return an error after publishing independent valid dates when some corrections fail.

- [ ] Write HTTP regressions for east/west boundaries, UTC fast path, DST, and missing-midnight rejection. Hand-derived fixture: UTC model cost 10, added boundary 3, removed boundary 1 gives local cost 12; reversing signs gives 8.
- [ ] Write `local_history_reuses_shared_boundaries_and_publishes_recent_days_first`: a three-day uncached window uses one summary query and at most four distinct boundary intervals, never three full-day log queries. Delay the oldest interval and assert a newer verified day is published before it finishes. Limit active boundary intervals to three; pagination retains its existing page-concurrency limit.
- [ ] Write `local_history_preserves_good_days_when_one_boundary_fails`: failed/capped/missing-spend corrections leave affected dates unpublished while independent dates arrive. Reject inconsistent summary/model totals and significant negative corrected costs. Zero and unknown remain distinct.
- [ ] Write `local_history_reuses_daily_cache_and_cannot_overwrite_newer_refresh`: fresh full usage or previews avoid network for their dates; forced requests bypass freshness. A newer selected-day result wins over an older batch result, including publication to the calendar.
- [ ] Run focused tests. Expected: FAIL for absent batch loader or ordering behavior.
- [ ] Implement one key-hash-scoped range summary query for uncached dates. Parse and normalize per-model costs once. Compute each local date from its UTC baseline plus signed boundary corrections; deduplicate exact intervals, prioritize recent dates, and share pagination/validation helpers. Publish complete dates as their dependencies finish.
- [ ] Factor the existing preview-cost cache lookup/store into internal helpers as needed. Reserve per-date ordering before network work; batch and individual loaders use the same guarded cost publication path. Recheck bounds before publishing. Do not populate the full-token cache from cost-only results.
- [ ] Run `cargo test -p ember -p shared -p ui --locked`. Expected: all pass.
- [ ] Commit: `feat: load local heatmap costs progressively`.

### Task 3: Coordinate and persist local calendar snapshots

**Interfaces:** Add shared `LocalHistorySnapshot { generation: u64, revision: u64, calendar_id: String, loading: bool, details: HistoryDetails }`. Produce `Service::local_history(mode: HistoryMode) -> Result<LocalHistorySnapshot, String>` and `Service::subscribe_local_history() -> watch::Receiver<LocalHistorySnapshot>`. Add Tauri `get_local_history(mode)` and event `local-history-updated` carrying this snapshot.

The new pure `local_history::LocalHistory` owns source/window identity, known amounts, synchronization timestamps, and stale-error state. Its operations are `load(path, source_id, window)`, `rebase(source_id, window) -> bool`, `range(now, mode) -> Option<NaiveDate>`, `merge_day(bounds, spend, fetched_at) -> bool`, `finish(now, start, error)`, and `view() -> HistoryDetails`. `CalendarWindow` supplies local today; timestamps remain UTC instants.

- [ ] Write persistence tests: reject version-1 UTC data, wrong sources, invalid values, and incompatible bounds; retain valid dates across rolling windows; persist corrected aggregate values only. Use `history-local.json`, version 2, and never interpret existing `history.json` as local data.
- [ ] Write refresh tests: cached reads do not fetch; first/daily synchronization covers the window; subsequent refreshes update local today at most once per minute; Force refreshes the whole window. Partial failures remain retryable and retain compatible known values with error state.
- [ ] Write coordinator tests for incremental snapshots, monotonic revisions, shared in-flight refreshes, forced supersession, source/timezone cancellation, local-midnight rollover during network failure, and disk-write failure retaining memory data.
- [ ] Write a race test where a completed selected-day request updates its square before an older history worker finishes. Assert the newer amount remains. Write an isolation test showing local history never enters `history::comparison` or the existing UTC notification path.
- [ ] Run focused tests. Expected: FAIL for missing local snapshot/coordinator behavior.
- [ ] Add local-calendar commands/state alongside the existing UTC History commands. A generation/run-ID guard rejects obsolete progress and completion messages. Increment revision on every visible transition; same-source refreshes can share work, Force supersedes older work.
- [ ] Start a lightweight one-second calendar check in the coordinator. Rebase and publish only when local dates/bounds change; this tick makes no network request. Abort incompatible workers. Explicit History requests perform network refreshes.
- [ ] Connect Task 2 progress to snapshots. On successful existing `day_costs`/`day_usage` commands, merge the accepted latest local cost into the calendar. Persist aggregate snapshots at completion and after selected-day reconciliation, with per-day freshness/order preventing stale overwrites.
- [ ] Wire watch updates to Tauri events. Keep the existing UTC history cache/worker and spending-increase path intact. Add test plumbing for the new watch sender without changing unrelated service behavior.
- [ ] Run `cargo test -p ember -p shared -p ui --locked`. Expected: all pass, especially UTC increase tests.
- [ ] Commit: `feat: publish timezone-safe local calendar snapshots`.

### Task 4: Add the inline recap and shared daily charts

**Interfaces:** Produce `DailyBreakdown(usage: Signal<Option<Vec<ModelUsage>>>, costs: Signal<Option<Vec<ModelCost>>>, known_total: Signal<Option<f64>>)`. It renders full usage first, otherwise model-cost preview, otherwise a known heatmap total. Consume Task 3 snapshots through `tauri::get_local_history(mode)` and existing `on_event("local-history-updated", ...)`.

App owns `history_selection: RwSignal<Option<String>>`, separate recap usage/cost `Remote` slots, and the latest local snapshot. `HistoryCalendar` consumes controlled selection and an activation callback; it no longer owns a focus-driven selection. The recap's close callback clears selection and invalidates pending requests.

- [ ] Write pure UI tests: full usage beats preview, known zero cost is visible without invented tokens, mismatched dates never render, newer snapshot revision wins over older command/event responses, changed generation/calendar invalidates loaded recap values, and a selection outside the new 30-day range closes.
- [ ] Run focused UI tests. Expected: FAIL for missing state/selection helpers.
- [ ] Extract the main screen's daily chart body into `daily_view.rs`, preserving its current appearance and model ordering. Use the same component inside a compact recap card with date, local-day label, close button, pending details, and error states. Render input/output totals alongside the reused token split; leave token graphics absent until counters arrive.
- [ ] Store selection and remote slots at App scope for tab/settings persistence. Load both daily commands when History is visible and a date is selected, including today. Reuse backend caches; do not change the main screen's selected date. Refresh all keeps the existing main-day refresh behavior and also forces the visible recap plus local calendar.
- [ ] Subscribe before fetching the initial local snapshot. Gate command responses and events by generation and revision. Calendar changes reset incompatible daily remote data; requests reset on date change/close. Current-generation events may reconcile values while settings are open. Dispose event listeners with App cleanup.
- [ ] Update heatmap labels, today state, and weekdays to the local snapshot. Buttons use native click activation for mouse/Enter/Space; focus shows focus styling only. Keep selection when compatible snapshots update; close it when the date leaves the window. Known local heatmap totals appear in the recap before either daily request completes.
- [ ] Replace the obsolete frontend UTC-history loader with the local snapshot path; keep the backend UTC path for notifications. Update README and scoped CSS.
- [ ] Run `cargo test -p ember -p shared -p ui --locked` and `trunk build --release --locked` from `ui/` for the latter. Expected: both pass.
- [ ] Exercise built WASM with synthetic Tauri responses: no initial recap; focus alone; click/keyboard activation; costs before tokens; token failure; A→B→A; close during load; late response rejection; independent main selection; tab/settings persistence; forced refresh; source/timezone change; local midnight without a Stats event. Capture a synthetic-data screenshot.
- [ ] Commit: `feat: show daily recap below the local heatmap`.

### Task 5: Verify, review, publish, and relaunch

- [ ] Run `cargo fmt --all -- --check`, `git diff --check`, `cargo test -p ember -p shared -p ui --locked`, backend/shared Clippy with `--all-targets --all-features --locked -- -D warnings`, and UI Clippy with `--target wasm32-unknown-unknown --all-targets --all-features --locked -- -D warnings`. Expected: all pass.
- [ ] Build release frontend and native backend (`trunk build --release --locked` in `ui/`; `cargo build -p ember --locked` in repository root). Expected: both succeed.
- [ ] Run one bounded read-only live comparison for a selected local day if credentials remain available. Compare heatmap total, recap cost, and full usage; record timing and parity only. Do not print credentials or raw request data. Analytics permission failures exercise fallback, not analytics success.
- [ ] Request one independent review of this feature's complete diff against `a2c9518`, including new files and Review Focus. Reproduce material findings before fixing; use one regression-driven fix pass and record deferred minors.
- [ ] Update plan results and existing PR description, preserving screenshot URLs. Push feature commits, attach the new synthetic screenshot, and check macOS CI.
- [ ] Gracefully quit the currently running debug Ember, launch the rebuilt binary, and verify its process/startup log. Keep the Trunk server available on port 1420.

## Plan self-review

All spec sections map to Tasks 1–4; Task 5 covers validation/publication. The existing native execution method is preserved. Local history has a new command/event contract so existing UTC notification tests remain meaningful. Calendar identity uses resolved day intervals rather than today's offset alone; no new timezone dependency is required for equivalent day membership.
