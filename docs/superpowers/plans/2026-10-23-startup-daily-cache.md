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
- [x] Receive targeted review and address material findings with regressions.
- [ ] Publish the review-fix follow-up to the existing PR.
- [ ] Verify CI and final local runtime.

The configured `/key/info` endpoint returned HTTP 200 in 125 ms. `/user/daily/activity/aggregated`, scoped by key hash and user ID, returned HTTP 403 in 94 ms. The preferred endpoint is reachable but forbidden for this key; fallback remains required.

## Publication status

The user explicitly requested pushing before the combative review completed. The earlier workspace run passed 268 tests, but the pre-push rerun failed `service::tests::daily_json_cache_survives_restart_and_failed_refresh_without_crossing_sources` at the final stale-cache lookup (`Option::unwrap()` on `None`). That version was published as `fcd8f27`; its CI passed. The review and fixture failure were subsequently addressed in the fix pass below.

## Combative review corrections

The review reproduced three Important issues: current-day cycles bypassed selected-day attempt ordering, accepted current-day results did not reconcile exact calendar values, and a click during yesterday's preload duplicated the full download.

- Route cycle results through the same service method and guarded `StoreDaily` publication as selected dates. Accept current Stats, persistent usage and calendar exact costs together, using the original validation timestamp. Suppress superseded cycle errors instead of publishing them as fresh failures.
- Share one refresh token between today's recap and the global refresh. Global refreshes without an existing token supersede prior selected-day work.
- Share in-flight full-day futures by connection and exact bounds. Same-attempt forced consumers can also reuse the completed result. New force attempts supersede older work; failures are removed and can retry. Futures use an independent request-cache owner to avoid reference cycles.
- Isolate the restart fixture's explicit persistence from automatic startup writes when aborting and recreating coordinators inside one test process.

The new race and duplication regressions failed before these fixes. A queued forced-refresh token also failed a source-change regression; settings saves now clear that queued token. Workspace verification after the fix pass: 274 passed, 6 optional live tests ignored; 32 service tests passed under UTC. Both Clippy targets, release frontend and native builds, formatting and diff checks passed. Built-WASM checks cover a shared today token across the budget cycle and recap, same-date screens, offline preservation, timezone changes and late selected-date replies. No review findings were deferred.
