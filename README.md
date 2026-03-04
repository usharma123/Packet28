# Packet28 Context Management Workspace

This workspace is a Rust multi-crate platform for producing, governing, assembling, persisting, and recalling machine-readable context packets.

The current center of gravity is `Packet28` (`suite-cli`) plus the context subsystem:
- `context-kernel-core` for orchestration, budgets, governance hooks, cache integration, and sequence execution.
- `contextq-core` for bounded context assembly.
- `context-memory-core` for persistent packet cache + store/recall APIs.
- `context-scheduler-core` for dependency-aware budgeted step scheduling.
- `guardy-core` + `suite-policy-core` for policy validation and packet audit.

## Crate Interaction Map

```mermaid
flowchart LR
  subgraph CLI["CLI entry points"]
    SuiteCLI["suite-cli Packet28"]
    CovyCLI["covy-cli"]
    DiffyCLI["diffy-cli"]
    TestyCLI["testy-cli"]
  end

  subgraph Context["Context subsystem"]
    Kernel["context-kernel-core"]
    ContextQ["contextq-core"]
    Memory["context-memory-core"]
    Scheduler["context-scheduler-core"]
    Guardy["guardy-core"]
    Policy["suite-policy-core"]
  end

  subgraph Reducers["Reducer and domain cores"]
    DiffyCore["diffy-core"]
    TestyCore["testy-core"]
    Stacky["stacky-core"]
    Buildy["buildy-core"]
    Mapy["mapy-core"]
    ProxyCore["suite-proxy-core"]
    Ingest["suite-ingest"]
    CovyIngest["covy-ingest"]
    CovyCore["covy-core"]
    TestyCommon["testy-cli-common"]
  end

  subgraph Contracts["Shared contracts"]
    Foundation["suite-foundation-core"]
    Packet["suite-packet-core"]
  end

  SuiteCLI --> Kernel
  SuiteCLI --> DiffyCore
  SuiteCLI --> TestyCore
  SuiteCLI --> Guardy
  SuiteCLI --> Ingest

  CovyCLI --> CovyIngest
  CovyCLI --> DiffyCore
  CovyCLI --> TestyCore
  DiffyCLI --> DiffyCore
  TestyCLI --> TestyCommon

  TestyCommon --> CovyIngest
  TestyCommon --> DiffyCore
  TestyCommon --> TestyCore

  CovyIngest --> CovyCore
  CovyCore --> DiffyCore
  CovyCore --> TestyCore

  Kernel --> ContextQ
  Kernel --> Memory
  Kernel --> Scheduler
  Kernel --> Guardy
  Kernel --> Policy
  Kernel --> Stacky
  Kernel --> Buildy
  Kernel --> Mapy
  Kernel --> ProxyCore

  ContextQ --> Foundation
  DiffyCore --> Foundation
  TestyCore --> Foundation
  Guardy --> Foundation
  Ingest --> Packet

  Foundation --> Packet
  ContextQ --> Packet
  Stacky --> Packet
  Buildy --> Packet
  Mapy --> Packet
  ProxyCore --> Packet
  Policy --> Packet
```

## Context System Architecture

```mermaid
flowchart TD
  Cmd["Packet28 command"] --> Req["KernelRequest target plus input packets plus reducer input plus budget plus policy context"]
  Req --> Kernel["context-kernel-core execute"]

  Kernel --> PolicyLoad["load context policy when config path is present"]
  PolicyLoad --> PolicyCheck["enforce reducer allowlist and audit input packets"]

  Kernel --> CacheLookup["context-memory-core lookup by target plus reducer input hash"]

  CacheLookup -->|"hit"| Cached["return cached output packets with cache metadata"]
  CacheLookup -->|"miss"| Reducer["run reducer"]

  Reducer --> OutputAudit["audit output packets when policy is enabled"]
  OutputAudit --> BudgetGate["enforce token byte runtime budgets"]
  BudgetGate --> CacheWrite["cache packets and metadata and persist when enabled"]
  CacheWrite --> Response["KernelResponse output packets plus audit plus metadata"]
  Cached --> Response

  Response --> OptionalAssemble["optional governed assemble contextq.assemble or governed.assemble"]
  OptionalAssemble --> Machine["suite.packet.v1 wrapper compact full handle profiles"]
```

## Context Store And Recall Lifecycle

```mermaid
flowchart LR
  Emit["Reducer output packet"] --> Cache["PacketCache entry"]
  Cache --> Disk[".packet28/packet-cache-v1.bin"]
  Disk --> TTL["TTL prune and version checks"]

  Disk --> List["context store list"]
  Disk --> Get["context store get"]
  Disk --> Stats["context store stats"]
  Disk --> Prune["context store prune"]

  Disk --> Recall["context recall query"]
  Recall --> Rank["token match plus path symbol boosts plus recency boost"]
  Rank --> Hits["ranked recall hits"]
```

## Overall Vision For Context Management

1. One packet contract everywhere.
`EnvelopeV1` and `suite.packet.v1` make every reducer output hashable, budgeted, and machine-consumable.

2. Bounded context by default.
`contextq-core` turns many packets into a single budget-capped context packet with explicit trim metadata.

3. Policy-first execution.
`guardy-core` and `suite-policy-core` are integrated in the kernel path so reducer execution and packet contents are enforceable, not advisory.

4. Reusable local memory.
`context-memory-core` persists reducer outputs with TTL and exposes store/recall APIs so repeated workflows can reuse prior context cheaply.

5. Composable execution graph.
`context-scheduler-core` plus kernel sequence execution provide a base for multi-step dependency-aware pipelines under explicit budgets.

In short: the system is moving toward a governed local context runtime where every tool result is a packet, every packet is auditable, and assembled context is deterministic, bounded, and reusable.

## Minimal Workflow

Build:

```bash
cargo build --release -p suite-cli
```

Validate policy:

```bash
./target/release/Packet28 guard validate --context-config context.yaml
```

Run reducer and governed assembly:

```bash
./target/release/Packet28 diff analyze \
  --coverage tests/fixtures/lcov/basic.info \
  --base HEAD \
  --head HEAD \
  --no-issues-state \
  --json \
  --context-config context.yaml
```

Assemble packets directly:

```bash
./target/release/Packet28 context assemble \
  --packet a.json \
  --packet b.json \
  --budget-tokens 5000 \
  --budget-bytes 32000 \
  --context-config context.yaml
```

Inspect and recall memory:

```bash
./target/release/Packet28 context store stats --root . --json
./target/release/Packet28 context store list --root . --limit 20 --json
./target/release/Packet28 context recall --root . --query "missing mappings parser" --limit 5 --json
```

## Protocol Docs

- `docs/packet-envelope-v1.md`
- `docs/machine-output-contract.md`
- `docs/wire-profiles.md`
- `docs/schema-registry.md`
- `docs/context-store-v1.md`
