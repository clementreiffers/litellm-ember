# Shared Current-Day Cache and Yesterday Preload

## User requirements

- Selecting today in the heatmap must reuse validated data already loaded by Consommation.
- Preload yesterday at startup, like today's consumption.
- Check whether the preferred analytics endpoint is accessible.

## Root cause and approach

The current-day cycle used `fetch_today_snapshot`, backed by the activity raw-row cache, while heatmap details used the separate validated daily cache. Replace the cycle's daily fetch with the same strict, key-scoped daily path as selected dates. Carry its original validation timestamp and exact local boundaries through `Event::Today`, then persist it in the existing daily store.

After the first finished current-day cycle, preload yesterday through the normal service method in a background task. Reuse fresh persisted data. Track the connection generation and local date so ordinary periodic refreshes do not repeatedly preload yesterday. Cancel on settings changes. Preserve explicit manual-refresh bypass.

## Verification

- [x] Reproduce absent shared today results with a startup/first-click regression.
- [x] Share strict current-day aggregates and preserve original validation time/bounds.
- [x] Add background yesterday preload, fresh-disk reuse and source isolation.
- [x] Reproduce and preserve manual current-day refresh bypass after cache unification.
- [x] Verify delayed yesterday loading does not delay current budget publication.
- [x] Workspace tests: 268 passed, 6 optional live tests ignored. Three startup tests also pass under UTC.
- [x] Backend/shared Clippy and native build pass.
- [ ] Receive targeted review, address material findings, and publish to the existing PR.
- [ ] Verify CI and final local runtime.

The configured `/key/info` endpoint returned HTTP 200 in 125 ms. `/user/daily/activity/aggregated`, scoped by key hash and user ID, returned HTTP 403 in 94 ms. The preferred endpoint is reachable but forbidden for this key; fallback remains required.

## Publication status

The user explicitly requested pushing before the combative review completes. The earlier workspace run passed 268 tests, but the pre-push rerun failed `service::tests::daily_json_cache_survives_restart_and_failed_refresh_without_crossing_sources` at the final stale-cache lookup (`Option::unwrap()` on `None`). Formatting and diff checks passed. This failure and the pending adversarial review remain open follow-up work; publication is not a claim that the branch is ready to merge.
