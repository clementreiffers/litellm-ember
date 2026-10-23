# GitHub Review Corrections and Persistent Daily Cache

User requested addressing PR #1 comments and explicitly approved improving the JSON cache. Base: `7d74db6`. Continue native execution and update the existing PR.

## Scope and decisions

- Comment 4353332584: move `/key/info` outside the analytics mutex. Represent Unknown/Loading/Ready, coalesce ordinary initialization through a shared future, and let forced initialization supersede older work. Separate identity and capability attempt IDs reject stale completions.
- Comment 4353337341: share fully validated correction rows by exact `(start, end)` within a connection. Keep at most 60 ranges for 15 seconds, remove failures, preserve pagination validation, and bypass completed values on force. Shared futures capture independent transport state to avoid an ownership cycle. No raw rows persist to disk.
- Comment 4353424990: implement distinct versioned `daily-usage.json`, not the UTC `history.json`. Store at most 30 recent local dates with source identity, exact bounds, timestamps, DailyCosts and optional complete DailyUsage. Past TTL is 15 minutes, today 60 seconds. Expose old costs as a labeled preview during network revalidation; never invent token counters or replace valid entries with failed/truncated data.

## Checks

- [x] Reproduce analytics lock contention and duplicate boundary requests with failing HTTP tests.
- [x] Fix coalescing, forced supersession, connection isolation, shared failure cleanup, and complete-pagination behavior.
- [x] Add JSON roundtrip, source/bounds/DST/TTL/window/capacity tests; service restart and failed-refresh tests; stale frontend preview guards.
- [x] Run workspace tests, formatting, both Clippy targets, frontend/native builds, and built-browser stale-cache checks.
- [ ] Review completed changes, fix material findings, push, reply to each GitHub thread with evidence, check CI, and relaunch locally.

## Integration notes

Network work occurs outside state locks. The service coordinator serializes atomic JSON writes on a blocking worker, so older writes cannot replace a newer snapshot. Existing in-memory caches continue to supply current-session results. Persisted values are validated again for source identity and resolved local-day boundaries before use. Existing heatmap aggregate estimates and UTC notification history retain their own contracts.

The adversarial follow-up is recorded in `2026-10-23-adversarial-review-fixes.md`. Final cache schema is version 2 with independent cost and usage validation timestamps. Paired forced requests share an attempt token and supersede pre-refresh boundary downloads, including when both screens select the same date. Final verification: 259 passed, 6 ignored; both lint targets, formatting, builds and browser checks passed.
