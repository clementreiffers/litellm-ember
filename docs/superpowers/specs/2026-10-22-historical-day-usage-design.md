# Historical daily usage

## Intent

Let users inspect the Consommation tab's daily costs and tokens for today or any earlier date for which their LiteLLM server retains logs.

## Agreed scope

- Add a date picker, previous/next-day buttons, and an Aujourd'hui button to the daily usage card. The user selected this navigation approach.
- Apply the selected date to daily spending, total tokens, input/output token split, and both per-model breakdowns.
- Keep the budget-period figures current, as explicitly requested by the user. This includes the header, budget gauge, period model costs, and menu-bar amount.

## Approaches

1. **Date picker, previous/next arrows, and Today button — selected.** Supports direct date jumps, day-by-day browsing, and a quick return to live usage. Adds a compact row of controls.
2. **Date picker only.** Uses fewer controls but makes adjacent-day browsing and returning to today less convenient.

## Proposed behavior

- Start in Today mode. Preserve the selected date while switching tabs or opening settings within the running app.
- Use local calendar days, matching the current daily usage calculation. Label the selected date clearly and keep French UI wording.
- Allow any earlier date without imposing the History calendar's 30-day limit. Disable navigation into the future and reject invalid or future dates in the backend.
- Today mode follows the existing live refreshes and advances when the local day changes. Choosing today's date or pressing Aujourd'hui returns to this mode.
- Fetch past-day usage on demand. Show loading, empty, and error states without displaying another day's figures under the selected date.
- Ignore responses superseded by a newer date selection or a connection change. A live update must not replace the selected past day's figures.
- Manual refresh also reloads the selected past day's usage.
- An empty successful response means no usage was returned. Historical availability depends on the LiteLLM server's retained logs; Ember cannot reconstruct deleted records.

These details are proposed implementation defaults, rather than additional user requirements.

## Implementation outline

- Reuse the existing daily aggregation and charts. Query a UTC date range covering the selected local day, then filter logs by that local date, including daylight-saving boundaries.
- Add a date-specific backend command and shared response carrying the date and model usage. Keep historical results separate from the live Stats snapshot.
- Reuse the existing on-demand loading and stale-response protection patterns. Keep caching bounded and avoid historical background polling.
- Keep API keys in the backend and preserve existing settings/cache compatibility.

## Non-goals

Historical budget reconstruction, changes to the History or Activity tabs, durable storage of historical request logs, and new dependencies unless implementation demonstrates a need.

## Verification

- Test selected-day aggregation, local midnight boundaries, invalid/future dates, and date-specific cache behavior with synthetic logs and mock HTTP responses.
- Test rapid date changes and connection changes so obsolete responses cannot replace the selected day's data.
- Verify controls, empty/error states, returning to Today, and current budget figures while viewing a past day.
- Run repository tests, formatting checks, Clippy, and a frontend build when the local Rust/WebAssembly toolchain is available.
