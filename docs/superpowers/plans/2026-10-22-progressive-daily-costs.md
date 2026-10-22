# Progressive Daily Costs Implementation Plan

> **For agentic workers:** Continue approved native execution with test-driven changes and one independent final review.

**Goal:** Show selected-day cost and model costs quickly, then fill in token details without changing current budget figures.

**Architecture:** Add a separate dated cost response and on-demand command alongside full daily usage. Fetch the lightweight UTC-day summary and correct its boundaries using only the two intervals where the local and UTC days differ. Run costs and full detail requests concurrently; full details take precedence once available.

**Spec:** User approved “costs quickly from summaries, then token details,” preserving local-day correctness.

## Constraints and decisions

- The heatmap stores UTC totals only; do not relabel that total as a local-day total.
- For local start before UTC midnight, add the earlier interval; for local start after midnight, subtract it. Apply the inverse adjustment at the end. Filter every correction interval as half-open even if the server end is inclusive.
- Use the selected key's SHA-256 hash for both summary and correction requests. Merge normalized model aliases consistently with full usage.
- Summary/correction failures must not block the full detail request. Never render missing token counters as zero.
- Reuse fresh full-day cache for costs when possible; otherwise fetch a lightweight summary without adding another persistent cache.
- Preserve request/generation guards, date selection, manual refresh, tab/settings persistence, and current budget sources.
- If a midnight does not exist, or correction pagination is incomplete/unavailable, let full usage supply the result rather than show uncertain preview figures.

## Task 1: Cost-first backend and UI

**Files:** `shared/src/lib.rs`; new `src-tauri/src/litellm/costs.rs`; existing `daily.rs`, `litellm.rs`, `service.rs`, `tray.rs`; `ui/src/tauri.rs`, `app.rs`, `usage.rs`; `README.md`.

- [x] Add failing backend tests for east/west UTC boundaries, exact midnight exclusion, normalized model costs, UTC zero-correction fast path, unsupported/capped correction response, and invalid/future selection.
- [x] Add `DailyCosts { date, models: Vec<ModelCost> }`, generation-guarded `get_day_costs(date, force)`, and boundary-corrected lightweight fetch using the existing page loader.
- [x] Add failing UI helper tests proving costs appear without tokens, full details supersede preview, and wrong-date preview never renders.
- [x] Load cost and detail slots independently; render total/model cost as soon as either is available, hide token charts until complete details arrive, and preserve cost display if tokens fail.
- [x] Verify full tests, fmt, both Clippy targets, release frontend, and native build. Exercise delayed detail/error/date-race behavior in the built frontend. Run an optional live cost/full comparison printing aggregate timing only.
- [ ] Review, fix material findings, update existing PR, and relaunch native app.

## Verification and review

209 tests passed, with 5 optional live tests ignored. Formatting, both Clippy targets, the release frontend build, and the native debug build passed. The optional live comparison returned matching per-model costs: preview in 775 ms, full details in 13.74 s. Browser tests verified progressive rendering, missing token states, failed detail requests preserving costs, date races, late preview rejection, and full-detail precedence.

Independent review found no Critical or Important issues. One Minor test gap is deferred: the preview-specific tests use fixed offsets; add synthetic DST-transition and missing-midnight cases to exercise the new correction path directly. Existing full-usage date-range tests cover DST, but are not substitutes for these preview tests.

The implementation uses UTC summaries with local boundary corrections rather than relabeling the heatmap's UTC totals. Busy boundary intervals can delay the preview. The native build was relaunched locally before publication.
