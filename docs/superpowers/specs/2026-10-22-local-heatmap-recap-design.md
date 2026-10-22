# Local-Day Heatmap and Daily Recap

## Approved adjustment after local testing

The user reported that waiting for local-midnight corrections delayed all heatmap colors. They explicitly selected **fast aggregate colors**, with local date labels and a clear estimate label, while retaining exact local-day details only after clicking. This supersedes the exact-local heatmap calculation and progressive boundary-fetch requirements below. The heatmap now uses one UTC summary request and no detail requests. Aggregate estimates and exact selected-day costs are stored separately; the latter never borrow estimated totals. Existing UTC notification history remains independent. All other selection, cache, and recap behavior still applies.

## Intent

Make the History heatmap use the same local calendar days as Consommation. Clicking a date displays a compact daily recap below the heatmap, so users can inspect that day's costs and tokens without switching tabs.

The user selected a card below the heatmap and explicitly requested local dates for the heatmap too.

## Approaches

1. **Inline recap below the heatmap — selected.** Keeps the calendar visible, fits the menu-bar panel, and provides room for the same figures and charts as Consommation.
2. **Popup beside the selected square — rejected.** Saves vertical space but leaves insufficient room for token and model charts in the narrow panel.

For local-day history, reuse UTC spending summaries with exact local-midnight corrections and shared daily caches. Downloading all request logs for all 30 days would make opening History unnecessarily expensive. Relabeling existing UTC totals would produce incorrect dates and is not an option.

## User-visible behavior

- Show the last 30 local calendar dates, including local today. Use local weekdays, today highlighting, labels, and rollover.
- Initially show the heatmap without an expanded recap. Clicking a date opens the recap; clicking another date updates it. Provide a close button on the recap.
- Keep keyboard focus separate from activation. Enter or Space activates a date, with the same behavior as clicking.
- The recap shows the selected date, total cost, token donut, input/output token split, tokens by model, and costs by model. Reuse the main screen's chart components and formatting in a compact layout.
- Show a known local-day cost immediately. Load per-model costs and token counters independently. Never substitute costs for tokens or display missing counters as zero.
- Keep available costs visible when token loading fails. Show an explicit loading or error state for missing details.
- Preserve the selected recap across tab changes and settings visits. Connection changes clear its loaded data; a date that leaves the rolling 30-day window closes the recap.
- Loading a recap does not change the main screen's independently selected date.
- Refresh all reloads the visible recap and heatmap, bypassing their freshness caches. Late responses cannot overwrite a newer selection, refresh, or connection.

## Local-day data and loading

Each heatmap square represents local midnight through the next local midnight, using the system timezone rules for that date. Resolve both boundaries independently so daylight-saving days can contain 23 or 25 hours.

Reuse fresh full-day usage or cost-preview results when available. For uncached history, obtain the lightweight UTC summaries once for the needed range and correct the intervals between UTC and local midnights. Share repeated boundary intervals between adjacent dates and bound request concurrency. Fetch token details only for the selected date.

Show cached heatmap values immediately. On a cold cache, publish verified local-day costs progressively, prioritizing recent dates. Dates awaiting correction remain unknown rather than showing mislabeled UTC totals. If a boundary cannot be resolved or correction data is incomplete, retain a compatible previously known value as stale or show unknown with an error.

The recap and main screen share the existing per-connection daily cost and full-usage caches: up to 30 dates, 15-minute freshness for past dates, 60 seconds for today. The heatmap can retain persisted cost-only history under its existing refresh policy. When newer daily data arrives, reconcile the selected square and recap so they use the same latest cost.

First loads can still wait for server responses, particularly when boundary intervals contain many logs. Opening History must not trigger full token downloads for all 30 days.

## Cache and date correctness

Version or replace the existing persisted heatmap cache so UTC totals can never be read as local-day totals. Key local-day caches by the connection and timezone/calendar boundaries; invalidate incompatible data when the system timezone changes. Persist only aggregate historical costs and nonsecret cache metadata.

Use local today for the heatmap's range and rollover. Network failure must not prevent the calendar from advancing at local midnight. Guard incremental responses with connection generation and request identity.

## Integration boundaries

The current History cache also feeds spending-increase notifications. Preserve that feature's established UTC comparison semantics by keeping its UTC summary query separate from the new local-day heatmap path. Do not pass local-day figures into a UTC comparison.

The existing preferred analytics endpoint and paginated/legacy fallback remain available for selected-day token details. The configured gateway currently denies analytics, so the recap must remain useful with costs visible while fallback details load.

## Verification

- Local/UTC date differences, east/west offsets, month/year rollover, leap day, DST transitions, and missing-midnight behavior.
- Rejection of old UTC cache data and incompatible timezone/connection data.
- Shared boundary requests, bounded concurrency, progressive local costs, and no whole-month token fetch.
- Click and keyboard activation, close behavior, independent main-screen selection, and tab/settings persistence.
- Immediate known costs, later token charts, failed details retaining costs, cached revisits, forced refresh, and stale-response rejection.
- UTC spending-increase comparisons retain their existing behavior.
- Full Rust workspace tests, formatting, backend and WebAssembly Clippy, frontend/native builds, and browser verification of loading and selection races.

## Builder decisions

Choose the smallest internal helper extraction, incremental delivery mechanism, and cache schema that satisfy these behaviors. Reuse existing Rust, Leptos, Tauri, chart components, and request guards. Add no dependencies unless the current timezone APIs cannot establish a reliable cache identity.

Implementation follows design approval, then a concrete plan. Publish to the existing PR and relaunch the local feature build after verification.
