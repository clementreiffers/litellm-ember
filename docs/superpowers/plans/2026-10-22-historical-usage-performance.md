# Historical Usage Performance Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans for the approved native execution workflow.

**Goal:** Reduce historical first-load latency and reuse recently visited days in the existing feature PR.

**Architecture:** Prefer `/spend/logs/v2` with exact UTC timestamps for the selected local day, 1,000 rows per page, and at most three concurrent page requests. Retain up to 30 aggregated days in memory for 15 minutes (60 seconds for today); manual refresh bypasses the selected day's cache. Preserve the existing legacy query for servers without access to v2 and capped responses.

**Tech Stack:** Existing Rust/Tauri/Leptos stack; use the already locked `sha2` crate directly to send only a key hash in query parameters.

**Spec:** Original historical-day design plus the user's approved app-side optimization: pagination, concurrency, multiple cached days, and manual corrections.

## Global constraints

- Keep current budget figures and local-day semantics, including DST and adjacent-midnight filtering.
- Publish only complete results; reject missing, duplicate, inconsistent, or failed pages.
- Fetch no prompts or response bodies through v2; deserialize only fields needed for daily usage.
- Keep the cache bounded, connection-specific, and in memory. Never store raw logs or secrets in it.
- Keep navigation responsive and retain stale-response protection. Refresh must not destroy other cached days for the same connection.

## Review focus

- Exact local-day bounds, including 23/25-hour days and nonexistent midnight: range and aggregation tests.
- Partial, capped, duplicate, or failed pages: no partial totals or cache publication.
- More than one day in cache, expiration, eviction, and forced corrections: cache behavior tests.
- Changed credentials or endpoint: no cross-connection cache reuse.
- Concurrent navigation and slow obsolete responses: existing regression plus bounded page concurrency test.

## Task 1: Faster complete historical queries and cache

**Files:** `src-tauri/src/litellm.rs`, new `src-tauri/src/litellm/daily.rs`, `src-tauri/src/service.rs`, `src-tauri/Cargo.toml`, `Cargo.lock`, `README.md`.

- [ ] Write and run failing tests for v2 exact timestamp/hash query, complete multi-page aggregation, bounded concurrency, malformed/duplicate/failed pages, capped and unavailable-v2 fallback.
- [ ] Implement `Client::fetch_day` through a focused daily module, using three concurrent requests after the first page; verify totals, page lengths, and request identity before publishing. Use local-day filtering even when the server's end boundary is inclusive.
- [ ] Write and run failing tests for A→B→A cache reuse, 15-minute expiration, 30-entry eviction, force bypass, same-day out-of-order updates, and isolation from live rows.
- [ ] Implement bounded aggregate caching and preserve it across manual refresh only when endpoint and key still match.
- [ ] Run full Rust tests, rustfmt, both existing Clippy commands, and release frontend build. Benchmark yesterday through the actual Rust client and compare totals with the legacy query; also measure cached revisit. Keep live tests optional and print aggregates/timings only.
- [ ] Perform one fresh review, address material findings with regression tests, update README/PR, push, and relaunch the clean native build.

## Approval and execution

The user approved adding these app-side improvements to the existing PR. Continue the previously selected native execution workflow without another handoff round. The cache size and expiry are implementation defaults stated before work.

## Results and review decisions

Implemented all steps. The independent review exposed automatic-refresh cache loss, cross-day refresh publication interference, and truncated legacy fallback acceptance; each received a failing regression test and a verified fix. Cache publication now uses per-date request identities, and both manual and scheduled connection refreshes retain aggregate data only for matching credentials and endpoint.

When v2 reports a capped count, the legacy endpoint remains a compatibility fallback. If it also reports truncation, show an error rather than adding recursive range splitting or publishing incomplete totals. This keeps the change bounded but means exceptionally high-volume days may require server changes. `sha2` is now a direct dependency, reusing version 0.10.9 already in the lockfile, to avoid sending raw credentials in query parameters.

Verification: 205 tests passed and 4 optional live tests were ignored. Formatting, backend/shared and WebAssembly UI Clippy, release frontend build, and native debug build passed. Two actual Rust-client comparisons returned matching aggregates for 4,651 requests: v2 took 7.21 and 15.34 seconds versus 19.32 and 17.94 seconds for the legacy query; cached revisits took 14.8 and 23.0 microseconds. Network/server timing varies; cache reuse is the largest repeat-navigation improvement.
