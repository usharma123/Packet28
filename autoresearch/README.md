# Autoresearch for Packet28

Karpathy-style autoresearch adapted for Packet28 context quality optimization.

Instead of optimizing `val_bpb` on a GPU, we optimize **context relevance quality** — how well the reducer/ranking system selects the right files for a given task.

## The Three Files That Matter

| File | Role | Who edits |
|------|------|-----------|
| `run_eval.sh` | Fixed evaluation harness (like `prepare.py`) | **Nobody** during experiments |
| Target crate code (`mapy-core`, `contextq-core`, `packet28-reducer-core`) | The code being optimized | **The agent** |
| `program.md` | Agent instructions ("research org code") | **The human** |

## How the Loop Works

1. **Branch**: each run gets `autoresearch/<tag>` branched from `research/autoresearch-base`
2. **Modify**: agent makes one small change to target crate code
3. **Run**: `bash autoresearch/run_eval.sh`
4. **Measure**: tests pass? score improved?
5. **Keep or discard**: `git reset --hard HEAD~1` if no improvement
6. **Repeat forever** until manually interrupted

## The Metric

Tests must pass as a gate. Score formula:

```
score = task_success_rate * 100 - 0.001 * avg_tokens - 0.01 * avg_latency_ms
```

If `tests_pass` is false, score = 0.

- `task_success_rate`: fraction of benchmark cases where the right files were ranked in top positions
- `avg_tokens`: average token count of assembled context (lower is better)
- `avg_latency_ms`: wall-clock time for target crate tests (lower is better)

## Design Choices

- **One crate at a time** — don't shotgun changes across the workspace
- **Small diffs** — every commit should be reviewable in <30 seconds
- **Git-based keep/revert** — no experiment branches that linger
- **Production experiments on child branches only** — `research/autoresearch-base` stays clean

## Benchmark Cases

See `cases/` for natural-language evaluation scenarios:
- `reducer_ranking_case_01.md` — feature implementation file selection
- `reducer_ranking_case_02.md` — bug-fix context narrowing
- `reducer_ranking_case_03.md` — precision over recall in file selection

## Quick Start

```bash
# Run the eval harness
bash autoresearch/run_eval.sh

# Check results
cat autoresearch/results.tsv

# Check logs
ls autoresearch/logs/
```
