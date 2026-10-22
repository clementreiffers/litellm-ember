# Cached Daily Views and Preferred Analytics Source

**Goal:** Reuse previously loaded daily values and prefer `/user/daily/activity/aggregated`, falling back to the current implementation.

**Scope:** Continue the user's approved native workflow and update PR #1. Keep current budget sources and local-day semantics. Existing 30-day/15-minute in-memory full-detail cache remains; add the same bounded cache for cost previews so returning before token completion also reuses costs. Manual refresh bypasses both.

## Implementation

- [x] Add tests showing repeated costs and A→B→A views reuse cached values; force reload and connection changes do not reuse stale values.
- [x] Add a preferred analytics reader with the key's own user ID from `/key/info`, SHA-256 API-key filter, and strict response validation. Remember unavailable/forbidden capability for 15 minutes per connection; manual refresh retries it.
- [x] Treat the analytics response as UTC daily buckets. Correct local start/end intervals for costs, requests, and input/output/total tokens using paginated logs. Missing, malformed, inconsistent, unavailable, or timed-out analytics/corrections fall back to the current paginated full-day reader and legacy compatibility route.
- [x] Keep the existing cost-first UI working independently. Fresh full details supply costs immediately; otherwise use cached cost preview or existing summary preview while the preferred detail request runs.
- [x] Test successful preference, denied/missing/error/invalid-response fallback, local boundaries including DST, normalization, force retry, and cache/source isolation. Add explicit preview DST tests deferred in the previous review.
- [x] Run full tests, fmt, Clippy, builds, and credentialed read-only comparison when available. Independently review and fix material findings.

Publication follows the existing workflow: commit/push this change to PR #1, update its validation results, and relaunch the local native build.

## Decisions

The analytics endpoint's timezone parameter does not convert historical UTC buckets into local days. Boundary correction is required. The key currently returns 403 on this endpoint, so the deployed gateway will continue using the fallback until access changes. No credentials or raw logs enter frontend caches or disk. Cache reuse lasts within the running app, not across restarts.

## Verification and review

The final workspace suite passed: 220 tests, with 5 optional live tests ignored. Formatting, backend/shared Clippy with warnings denied, and the native debug build passed after the review fixes. WebAssembly UI Clippy passed before review; the UI is unchanged by this work.

Independent review identified two P2 issues. Both were reproduced with failing regression tests and fixed:

- Boundary corrections now retain missing-field information and require spend and every token counter before using an analytics result. Incomplete correction rows trigger fallback instead of becoming zero counters.
- Capability updates use attempt IDs, so an older result cannot overwrite a later forced retry. The overlap regression covers both stale failure after success and stale success after failure.

No review findings remain deferred. Previously deferred cost-preview DST and missing-midnight cases are now tested. The latest live fallback comparison matched per-model costs: 268 ms for previews and 26.04 s for full details. Successful analytics requests remain mock-tested because the configured key receives HTTP 403; these timings are local measurements, not latency guarantees.
