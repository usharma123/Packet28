# Case 03: Select Useful Files Without Over-Reading

## Task

"Understand how file relevance scoring works so I can tune the weights"

## Expected Top Files

The ranking system should surface:

- `mapy-core/src/runtime.rs` — the ranking algorithm implementation
- `mapy-core/src/types.rs` — `RankedFile`, `RankedSymbol`, and related types

## Should NOT Include

- More than ~5 files total — this is a focused exploration query
- Unrelated crate internals (contextq assembly, reducer orchestration)
- Test files (unless they document scoring behavior)
- CLI/daemon code

## Evaluation Criteria

1. **Precision matters more than recall.**
   - Including the 2 key files is necessary.
   - Including >5 files total is a penalty — the user asked a focused question.

2. **The assembled context should be self-contained.**
   - Reading the top files should be enough to understand the scoring algorithm.
   - Should not require jumping to 10 other files.

3. **Type definitions should accompany the algorithm.**
   - `types.rs` (or equivalent) should appear alongside the runtime/scoring code.

## Scoring

- Both key files in top 3, ≤5 total: full credit
- Both key files in top 5, ≤7 total: partial credit
- Missing a key file OR >7 total: fail
- Bonus: if the top 2 files alone explain the scoring system end-to-end
