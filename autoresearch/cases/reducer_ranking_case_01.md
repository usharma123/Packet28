# Case 01: Find Most Relevant Files for a Feature Implementation Task

## Task

"Add WebSocket support to the Packet28 daemon for real-time context push"

## Expected Top Files

The ranking system should surface these in the top 5:

- `packet28d/src/main.rs` — daemon entry point, where WS listener would be added
- `packet28-daemon-core/src/` — daemon core logic, connection handling
- Relevant networking/transport modules in the daemon crates

## Should NOT Include

- Unrelated reducers (`covy`, `testy`, etc.)
- `website/` — frontend/docs site
- `scripts/` — build/CI tooling
- `mapy-core/` internals (unless ranking types are needed)

## Evaluation Criteria

1. **Did mapy-core rank the right files in the top 5?**
   - The daemon crate files should dominate the top positions.
   - Networking-related modules should rank higher than generic utilities.

2. **Did contextq-core select them within budget?**
   - With a reasonable token budget (~4000 tokens), the assembled context should include the daemon entry point and core connection handling.
   - Should not waste budget on tangentially related files.

3. **Precision**: at least 3 of top 5 files should be directly relevant to WebSocket/daemon work.

## Scoring

- 3+ correct in top 5: full credit
- 2 correct in top 5: partial credit
- 1 or 0 correct: fail
- Penalty: each irrelevant file in top 5 reduces score by 0.1
