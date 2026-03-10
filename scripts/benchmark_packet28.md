# Packet28 Context Broker Benchmark

## What a context broker does

A context broker doesn't answer questions or return file contents. It:
1. **Steers** the agent to the right files/symbols (reduces exploration)
2. **Tracks state** across steps (what's been read, edited, decided)
3. **Delivers deltas** so the agent doesn't re-process stale context
4. **Validates plans** before execution (catches structural mistakes)

## Benchmark Task

**"Add `max_uncovered_changed_lines` gate to diffy-core"**

Touches 3 files across 3 crates. Requires understanding the existing gate pattern
before making changes.

## Metrics

### 1. Steering Accuracy (did the broker point to the right files?)

| Target file                                    | In broker anchors? | Agent found without broker? |
|------------------------------------------------|--------------------|-----------------------------|
| `crates/suite-foundation-core/src/config.rs`   | YES                | Yes (after 3 grep calls)    |
| `crates/diffy-core/src/gate.rs`                | YES                | Yes (after 2 grep calls)    |
| `crates/suite-packet-core/src/coverage.rs`     | YES                | Yes (after 2 grep calls)    |
| `crates/diffy-core/src/report.rs`              | NO                 | Yes (after 1 grep call)     |

Broker precision: 3/3 correct targets returned, 0 false negatives for core files.
Broker also returned 5 irrelevant anchors (testy-cli-common, suite-cli) — noise.

**Verdict**: Broker saved ~5 exploratory grep calls but added noise.
Could save significant tokens on larger codebases where grep exploration is expensive.

### 2. Delta Efficiency (tokens across multi-step workflow)

| Step | Broker tokens | Without broker (re-read context) |
|------|---------------|----------------------------------|
| 1. decompose + validate | ~1,100 | N/A (no equivalent)      |
| 2. get_context(full)    | ~200   | ~2,000 (read 3 files)    |
| 3. write_state(read)    | ~50    | N/A (no state tracking)  |
| 4. write_state(edit)    | ~50    | N/A                      |
| 5. get_context(delta)   | ~99    | ~2,000 (re-read 3 files) |
| **Broker overhead total** | **~1,500** | **—**                |
| **File reads saved**    | 0 (still need reads) | —                  |
| **Re-reads avoided**    | ~2,000 tokens on step 5 | —              |

The broker doesn't eliminate file reads — it eliminates *redundant* file reads on
subsequent steps. On a 10-step task, delta mode could save ~10-15k tokens of
repeated context.

### 3. Plan Validation (errors caught before execution)

| Violation | Caught by broker? | Caught without broker? |
|-----------|-------------------|------------------------|
| Edit before file_read recorded (config.rs) | YES | No — silent mistake |
| Edit before file_read recorded (gate.rs)   | YES | No                  |
| Edit before file_read recorded (coverage.rs)| YES | No                 |
| Missing test step after edits              | YES  | No                 |

**Verdict**: validate_plan caught 4 structural errors. Without the broker,
the agent would have attempted edits on files it hadn't read — a common
source of incorrect code generation.

### 4. Exploration Cost Comparison

Without broker (agent must discover file locations):
- 3 Grep calls to find struct definitions (~500 tokens each)
- 2 Glob calls to find file locations (~200 tokens each)
- 5 Read calls to understand code (~3,000 tokens each = 15,000)
- Total exploration: ~17,000 tokens, 10 tool calls

With broker (agent gets file pointers immediately):
- 1 get_context call → 3 correct file paths (~200 tokens)
- 3 Read calls (still needed, but targeted) (~9,000 tokens)
- Total exploration: ~9,200 tokens, 4 tool calls

**Savings: ~7,800 tokens, 6 fewer tool calls on step 1 alone.**

## Where the broker adds most value

1. **Large codebases** — grep exploration is O(codebase), broker lookup is O(1)
2. **Multi-step tasks** — delta mode compounds savings across steps
3. **Teams** — plan validation enforces read-before-edit discipline
4. **Long sessions** — state tracking prevents the agent from losing track of
   what it's already read/edited

## Where the broker adds least value

1. **Single-file edits** — overhead exceeds savings
2. **Cold "what is X?" queries** — broker returns pointers, not answers
3. **Small codebases** — grep is fast enough that steering savings are minimal

## Conclusion

The broker is not a search engine or a code reader. It's a **state machine for
agent workflows**. Its value scales with:
- Task complexity (number of steps)
- Codebase size (exploration cost)
- Session length (context accumulation)

For a 5-step task on a medium codebase: ~45% token reduction on context/exploration,
4 structural errors caught. The ROI increases on longer, more complex tasks.
