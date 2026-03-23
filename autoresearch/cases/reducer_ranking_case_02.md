# Case 02: Narrow Context for a Bug-Fix Query

## Task

"Fix: contextq assembly drops high-relevance sections when two packets share duplicate refs"

## Expected Top Files

The ranking system should tightly focus on:

- `contextq-core/src/assemble.rs` — the assembly logic where deduplication happens
- `suite-packet-core/src/context.rs` — packet context handling, where refs are resolved

## Should NOT Include

- `mapy-core/` — file ranking is not the bug location
- CLI crates (`packet28-cli`, `packet28d`) — not relevant to assembly logic
- Build infrastructure, scripts, website

## Evaluation Criteria

1. **Did the system focus tightly on the 2-3 most relevant files?**
   - `assemble.rs` should be #1 or #2.
   - Context/ref handling should appear in top 3.

2. **Token count should be minimal.**
   - A bug fix query should produce a narrow, focused context.
   - Ideal: <2000 tokens of assembled context.
   - Penalize if >5 files are included.

3. **Signal-to-noise ratio**: the assembled context should contain the deduplication logic and ref handling, not general boilerplate.

## Scoring

- Both key files in top 3, <5 total files: full credit
- One key file in top 3, <7 total files: partial credit
- Neither key file in top 3: fail
- Token penalty: -0.1 per 500 tokens over 2000
