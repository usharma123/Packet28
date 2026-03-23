# Autoresearch Program — Packet28 Context Quality

You are an autonomous research agent optimizing Packet28's context relevance quality.
Your goal: make the ranking/reducer system select the right files for a given task,
with minimal token waste and low latency.

## Setup

Before starting experiments:

1. **Agree on a run tag** (e.g. `mar23`). Branch `autoresearch/<tag>` must not already exist.
2. **Create the branch** from `research/autoresearch-base`:
   ```bash
   git checkout research/autoresearch-base
   git checkout -b autoresearch/<tag>
   ```
3. **Read the in-scope files**:
   - This file (`program.md`)
   - All 3 benchmark cases in `cases/`
   - Relevant source in `mapy-core/`, `contextq-core/`, `packet28-reducer-core/`
4. **Verify the build works**:
   ```bash
   cargo build -p mapy-core -p contextq-core -p packet28-reducer-core
   ```
5. **Initialize results.tsv** with the header row (if not already present).
6. **Confirm and go.**

## Scope

### What you CAN modify
- `mapy-core/` — file/symbol ranking (focus matching, change proximity, dependency centrality, recency)
- `contextq-core/` — context assembly with budget-constrained relevance ranking
- `packet28-reducer-core/` — reducer orchestration
- Tests within those crates
- `autoresearch/**` (except `run_eval.sh`)

### What you CANNOT modify
- Other crates in the workspace
- Packaging, publishing, or install UX
- `autoresearch/run_eval.sh` (this is the fixed eval — like `prepare.py`)
- Broad repo-wide rewrites

### Goal
Improve context relevance — the right files/symbols selected for a given task, with minimal token waste.

## The Experiment Loop

Repeat forever:

1. **Look at git state.** `git status`, `git log --oneline -5`.
2. **Form one hypothesis.** Make the smallest viable change to target crate code.
3. **`git commit`** the change.
4. **Run the eval:**
   ```bash
   bash autoresearch/run_eval.sh > autoresearch/logs/run.log 2>&1
   ```
5. **Read results:** `tail -5 autoresearch/logs/run.log`
6. **If tests fail or crash:** read the error, attempt a fix. If fundamentally broken, give up on that idea and revert.
7. **Record in results.tsv.** Do NOT commit results.tsv — leave it untracked.
8. **If improved:** advance the branch (keep the commit).
9. **If equal or worse:** `git reset --hard HEAD~1`

## Rules

- **One hypothesis at a time.** Don't bundle multiple changes.
- **Small diffs, reviewable.** Every commit should be understandable in <30 seconds.
- **Revert failed ideas immediately.** Don't let broken commits accumulate.
- **Do not hardcode to benchmark cases.** Preserve generality — the eval cases are representative, not exhaustive.
- **NEVER STOP.** Keep running experiments until manually interrupted.
- **If stuck:** re-read crate source, re-read cases, try combining near-misses, try more radical changes.

## Results.tsv Format

Tab-separated. Columns:

```
run_id	target	tests_pass	task_success_rate	avg_tokens	avg_latency_ms	score	status	notes
```

| Column | Description |
|--------|-------------|
| `run_id` | `<git-short-hash>-<unix-timestamp>` |
| `target` | which crate/area was modified |
| `tests_pass` | `true` / `false` |
| `task_success_rate` | 0.0–1.0 (placeholder until wired) |
| `avg_tokens` | average context tokens (placeholder until wired) |
| `avg_latency_ms` | wall-clock ms for target crate tests |
| `score` | computed by `run_eval.sh` (0 if tests fail) |
| `status` | `keep`, `discard`, `crash` |
| `notes` | short description of what was tried |
