# Packet28 Regex Search Benchmark

_Generated: 2026-03-24T15:16:21.005920+00:00_

## Setup

- Workspace: `/Users/utsavsharma/Documents/GitHub/Coverage`
- Packet28 in-process indexes were pre-built per search root before timing.
- Packet28 daemon transport was measured against a resident `packet28d` running at the workspace root, with subtree searches mapped into requested-path filters.
- Speed was measured with `hyperfine` using 2 warmups and 8 measured runs, with stdout redirected to `/dev/null`.
- Token efficiency is measured against a normalized compact Packet28-style packet derived from each tool's match set.
- Packet28 accuracy is collected from full query output; Packet28 timing is measured on compact mode so speed and token costs reflect the reduced interface boundary.
- Accuracy is exact match-set parity against the canonical `ripgrep` `path:line` hit set for each regex scenario.

### Tool Versions

- `packet28-search-cli`: `git 6b6084e`
- `packet28d`: `packet28d 0.2.36`
- `ripgrep`: `ripgrep 15.1.0`
- `grep`: `grep (BSD grep, GNU compatible) 2.6.0-FreeBSD`
- `ast-grep`: `ast-grep 0.42.0`

### One-Time Packet28 Index Build Times

- `workspace daemon index`: `10714.853 ms`
- `inproc crates/packet28-search-cli`: `70.592 ms`
- `inproc crates/packet28-search-core`: `781.356 ms`
- `inproc crates/packet28d`: `586.964 ms`
- `inproc crates/suite-cli`: `1160.310 ms`

## Summary

| Tool | Scenarios | Avg Mean ms | Avg Compact Tokens | Avg True Hits / 1k Tokens | Exact-Match Rate |
| --- | ---: | ---: | ---: | ---: | ---: |
| `packet28-daemon` | 8 | 6.879 | 16.4 | 452.7 | 100% |
| `ripgrep` | 8 | 8.235 | 16.4 | 452.7 | 100% |
| `grep` | 8 | 8.886 | 16.4 | 452.7 | 100% |
| `packet28-inproc` | 8 | 11.701 | 16.4 | 452.7 | 100% |
| `ast-grep` | 4 | 18.438 | 15.2 | 66.0 | 50% |

## Function Definition

Single Rust function definition lookup for handle_packet28_search.

- Root: `crates/suite-cli`
- Canonical hits (`ripgrep`): `src/cmd_mcp_native.rs:256`
- Packet28 daemon backend: `indexed_regex` transport: `daemon` total: `1`
- Packet28 inproc backend: `indexed_regex` transport: `inproc` total: `1`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 3.413 | 16 | 16.0 | 100% | 100% | yes |
| `packet28-inproc` | 8.322 | 16 | 16.0 | 100% | 100% | yes |
| `ripgrep` | 9.475 | 16 | 16.0 | 100% | 100% | yes |
| `grep` | 15.243 | 16 | 16.0 | 100% | 100% | yes |
| `ast-grep` | 25.329 | 17 | 17.0 | 1% | 100% | no |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'fn\s+handle_packet28_search\(' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'fn\s+handle_packet28_search\(' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'fn\s+handle_packet28_search\(' crates/suite-cli`
- `grep`: `grep -RInE --color=never 'fn[[:space:]]+handle_packet28_search\(' crates/suite-cli`
- `ast-grep`: `ast-grep run --lang rust --heading never --color never -C 0 -p 'pub(crate) fn handle_packet28_search($$$ARGS) -> $$$RET { $$$BODY }' crates/suite-cli`

### Match Sets

- `packet28-daemon` found: `src/cmd_mcp_native.rs:256`
- `packet28-inproc` found: `src/cmd_mcp_native.rs:256`
- `ripgrep` found: `src/cmd_mcp_native.rs:256`
- `grep` found: `src/cmd_mcp_native.rs:256`
- `ast-grep` found: `src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:257, src/cmd_mcp_native.rs:258, src/cmd_mcp_native.rs:259, src/cmd_mcp_native.rs:260, src/cmd_mcp_native.rs:261, src/cmd_mcp_native.rs:262, src/cmd_mcp_native.rs:263, src/cmd_mcp_native.rs:264, src/cmd_mcp_native.rs:265, src/cmd_mcp_native.rs:266, src/cmd_mcp_native.rs:267, src/cmd_mcp_native.rs:268, src/cmd_mcp_native.rs:269, src/cmd_mcp_native.rs:270, src/cmd_mcp_native.rs:271, src/cmd_mcp_native.rs:272, src/cmd_mcp_native.rs:273, src/cmd_mcp_native.rs:274, src/cmd_mcp_native.rs:275, src/cmd_mcp_native.rs:276, src/cmd_mcp_native.rs:277, src/cmd_mcp_native.rs:278, src/cmd_mcp_native.rs:279, src/cmd_mcp_native.rs:280, src/cmd_mcp_native.rs:281, src/cmd_mcp_native.rs:282, src/cmd_mcp_native.rs:283, src/cmd_mcp_native.rs:284, src/cmd_mcp_native.rs:285, src/cmd_mcp_native.rs:286, src/cmd_mcp_native.rs:287, src/cmd_mcp_native.rs:288, src/cmd_mcp_native.rs:289, src/cmd_mcp_native.rs:290, src/cmd_mcp_native.rs:291, src/cmd_mcp_native.rs:292, src/cmd_mcp_native.rs:293, src/cmd_mcp_native.rs:294, src/cmd_mcp_native.rs:295, src/cmd_mcp_native.rs:296, src/cmd_mcp_native.rs:297, src/cmd_mcp_native.rs:298, src/cmd_mcp_native.rs:299, src/cmd_mcp_native.rs:300, src/cmd_mcp_native.rs:301, src/cmd_mcp_native.rs:302, src/cmd_mcp_native.rs:303, src/cmd_mcp_native.rs:304, src/cmd_mcp_native.rs:305, src/cmd_mcp_native.rs:306, src/cmd_mcp_native.rs:307, src/cmd_mcp_native.rs:308, src/cmd_mcp_native.rs:309, src/cmd_mcp_native.rs:310, src/cmd_mcp_native.rs:311, src/cmd_mcp_native.rs:312, src/cmd_mcp_native.rs:313, src/cmd_mcp_native.rs:314, src/cmd_mcp_native.rs:315, src/cmd_mcp_native.rs:316, src/cmd_mcp_native.rs:317, src/cmd_mcp_native.rs:318, src/cmd_mcp_native.rs:319, src/cmd_mcp_native.rs:320, src/cmd_mcp_native.rs:321, src/cmd_mcp_native.rs:322, src/cmd_mcp_native.rs:323, src/cmd_mcp_native.rs:324, src/cmd_mcp_native.rs:325, src/cmd_mcp_native.rs:326, src/cmd_mcp_native.rs:327, src/cmd_mcp_native.rs:328, src/cmd_mcp_native.rs:329, src/cmd_mcp_native.rs:330, src/cmd_mcp_native.rs:331, src/cmd_mcp_native.rs:332, src/cmd_mcp_native.rs:333, src/cmd_mcp_native.rs:334, src/cmd_mcp_native.rs:335, src/cmd_mcp_native.rs:336, src/cmd_mcp_native.rs:337, src/cmd_mcp_native.rs:338, src/cmd_mcp_native.rs:339, src/cmd_mcp_native.rs:340, src/cmd_mcp_native.rs:341, src/cmd_mcp_native.rs:342, src/cmd_mcp_native.rs:343, src/cmd_mcp_native.rs:344, src/cmd_mcp_native.rs:345, src/cmd_mcp_native.rs:346, src/cmd_mcp_native.rs:347, src/cmd_mcp_native.rs:348, src/cmd_mcp_native.rs:349, src/cmd_mcp_native.rs:350, src/cmd_mcp_native.rs:351, src/cmd_mcp_native.rs:352, src/cmd_mcp_native.rs:353, src/cmd_mcp_native.rs:354, src/cmd_mcp_native.rs:355, src/cmd_mcp_native.rs:356, src/cmd_mcp_native.rs:357, src/cmd_mcp_native.rs:358, src/cmd_mcp_native.rs:359, src/cmd_mcp_native.rs:360, src/cmd_mcp_native.rs:361, src/cmd_mcp_native.rs:362, src/cmd_mcp_native.rs:363, src/cmd_mcp_native.rs:364, src/cmd_mcp_native.rs:365, src/cmd_mcp_native.rs:366, src/cmd_mcp_native.rs:367, src/cmd_mcp_native.rs:368, src/cmd_mcp_native.rs:369, src/cmd_mcp_native.rs:370, src/cmd_mcp_native.rs:371, src/cmd_mcp_native.rs:372, src/cmd_mcp_native.rs:373, src/cmd_mcp_native.rs:374, src/cmd_mcp_native.rs:375, src/cmd_mcp_native.rs:376, src/cmd_mcp_native.rs:377, src/cmd_mcp_native.rs:378, src/cmd_mcp_native.rs:379, src/cmd_mcp_native.rs:380, src/cmd_mcp_native.rs:381, src/cmd_mcp_native.rs:382, src/cmd_mcp_native.rs:383, src/cmd_mcp_native.rs:384, src/cmd_mcp_native.rs:385, src/cmd_mcp_native.rs:386, src/cmd_mcp_native.rs:387, src/cmd_mcp_native.rs:388, src/cmd_mcp_native.rs:389, src/cmd_mcp_native.rs:390, src/cmd_mcp_native.rs:391, src/cmd_mcp_native.rs:392, src/cmd_mcp_native.rs:393, src/cmd_mcp_native.rs:394, src/cmd_mcp_native.rs:395, src/cmd_mcp_native.rs:396, src/cmd_mcp_native.rs:397, src/cmd_mcp_native.rs:398, src/cmd_mcp_native.rs:399, src/cmd_mcp_native.rs:400, src/cmd_mcp_native.rs:401, src/cmd_mcp_native.rs:402, src/cmd_mcp_native.rs:403, src/cmd_mcp_native.rs:404, src/cmd_mcp_native.rs:405, src/cmd_mcp_native.rs:406, src/cmd_mcp_native.rs:407, src/cmd_mcp_native.rs:408, src/cmd_mcp_native.rs:409, src/cmd_mcp_native.rs:410, src/cmd_mcp_native.rs:411, src/cmd_mcp_native.rs:412, src/cmd_mcp_native.rs:413, src/cmd_mcp_native.rs:414, src/cmd_mcp_native.rs:415, src/cmd_mcp_native.rs:416, src/cmd_mcp_native.rs:417, src/cmd_mcp_native.rs:418, src/cmd_mcp_native.rs:419, src/cmd_mcp_native.rs:420, src/cmd_mcp_native.rs:421, src/cmd_mcp_native.rs:422, src/cmd_mcp_native.rs:423`
  extra: `src/cmd_mcp_native.rs:257, src/cmd_mcp_native.rs:258, src/cmd_mcp_native.rs:259, src/cmd_mcp_native.rs:260, src/cmd_mcp_native.rs:261, src/cmd_mcp_native.rs:262, src/cmd_mcp_native.rs:263, src/cmd_mcp_native.rs:264, src/cmd_mcp_native.rs:265, src/cmd_mcp_native.rs:266, src/cmd_mcp_native.rs:267, src/cmd_mcp_native.rs:268, src/cmd_mcp_native.rs:269, src/cmd_mcp_native.rs:270, src/cmd_mcp_native.rs:271, src/cmd_mcp_native.rs:272, src/cmd_mcp_native.rs:273, src/cmd_mcp_native.rs:274, src/cmd_mcp_native.rs:275, src/cmd_mcp_native.rs:276, src/cmd_mcp_native.rs:277, src/cmd_mcp_native.rs:278, src/cmd_mcp_native.rs:279, src/cmd_mcp_native.rs:280, src/cmd_mcp_native.rs:281, src/cmd_mcp_native.rs:282, src/cmd_mcp_native.rs:283, src/cmd_mcp_native.rs:284, src/cmd_mcp_native.rs:285, src/cmd_mcp_native.rs:286, src/cmd_mcp_native.rs:287, src/cmd_mcp_native.rs:288, src/cmd_mcp_native.rs:289, src/cmd_mcp_native.rs:290, src/cmd_mcp_native.rs:291, src/cmd_mcp_native.rs:292, src/cmd_mcp_native.rs:293, src/cmd_mcp_native.rs:294, src/cmd_mcp_native.rs:295, src/cmd_mcp_native.rs:296, src/cmd_mcp_native.rs:297, src/cmd_mcp_native.rs:298, src/cmd_mcp_native.rs:299, src/cmd_mcp_native.rs:300, src/cmd_mcp_native.rs:301, src/cmd_mcp_native.rs:302, src/cmd_mcp_native.rs:303, src/cmd_mcp_native.rs:304, src/cmd_mcp_native.rs:305, src/cmd_mcp_native.rs:306, src/cmd_mcp_native.rs:307, src/cmd_mcp_native.rs:308, src/cmd_mcp_native.rs:309, src/cmd_mcp_native.rs:310, src/cmd_mcp_native.rs:311, src/cmd_mcp_native.rs:312, src/cmd_mcp_native.rs:313, src/cmd_mcp_native.rs:314, src/cmd_mcp_native.rs:315, src/cmd_mcp_native.rs:316, src/cmd_mcp_native.rs:317, src/cmd_mcp_native.rs:318, src/cmd_mcp_native.rs:319, src/cmd_mcp_native.rs:320, src/cmd_mcp_native.rs:321, src/cmd_mcp_native.rs:322, src/cmd_mcp_native.rs:323, src/cmd_mcp_native.rs:324, src/cmd_mcp_native.rs:325, src/cmd_mcp_native.rs:326, src/cmd_mcp_native.rs:327, src/cmd_mcp_native.rs:328, src/cmd_mcp_native.rs:329, src/cmd_mcp_native.rs:330, src/cmd_mcp_native.rs:331, src/cmd_mcp_native.rs:332, src/cmd_mcp_native.rs:333, src/cmd_mcp_native.rs:334, src/cmd_mcp_native.rs:335, src/cmd_mcp_native.rs:336, src/cmd_mcp_native.rs:337, src/cmd_mcp_native.rs:338, src/cmd_mcp_native.rs:339, src/cmd_mcp_native.rs:340, src/cmd_mcp_native.rs:341, src/cmd_mcp_native.rs:342, src/cmd_mcp_native.rs:343, src/cmd_mcp_native.rs:344, src/cmd_mcp_native.rs:345, src/cmd_mcp_native.rs:346, src/cmd_mcp_native.rs:347, src/cmd_mcp_native.rs:348, src/cmd_mcp_native.rs:349, src/cmd_mcp_native.rs:350, src/cmd_mcp_native.rs:351, src/cmd_mcp_native.rs:352, src/cmd_mcp_native.rs:353, src/cmd_mcp_native.rs:354, src/cmd_mcp_native.rs:355, src/cmd_mcp_native.rs:356, src/cmd_mcp_native.rs:357, src/cmd_mcp_native.rs:358, src/cmd_mcp_native.rs:359, src/cmd_mcp_native.rs:360, src/cmd_mcp_native.rs:361, src/cmd_mcp_native.rs:362, src/cmd_mcp_native.rs:363, src/cmd_mcp_native.rs:364, src/cmd_mcp_native.rs:365, src/cmd_mcp_native.rs:366, src/cmd_mcp_native.rs:367, src/cmd_mcp_native.rs:368, src/cmd_mcp_native.rs:369, src/cmd_mcp_native.rs:370, src/cmd_mcp_native.rs:371, src/cmd_mcp_native.rs:372, src/cmd_mcp_native.rs:373, src/cmd_mcp_native.rs:374, src/cmd_mcp_native.rs:375, src/cmd_mcp_native.rs:376, src/cmd_mcp_native.rs:377, src/cmd_mcp_native.rs:378, src/cmd_mcp_native.rs:379, src/cmd_mcp_native.rs:380, src/cmd_mcp_native.rs:381, src/cmd_mcp_native.rs:382, src/cmd_mcp_native.rs:383, src/cmd_mcp_native.rs:384, src/cmd_mcp_native.rs:385, src/cmd_mcp_native.rs:386, src/cmd_mcp_native.rs:387, src/cmd_mcp_native.rs:388, src/cmd_mcp_native.rs:389, src/cmd_mcp_native.rs:390, src/cmd_mcp_native.rs:391, src/cmd_mcp_native.rs:392, src/cmd_mcp_native.rs:393, src/cmd_mcp_native.rs:394, src/cmd_mcp_native.rs:395, src/cmd_mcp_native.rs:396, src/cmd_mcp_native.rs:397, src/cmd_mcp_native.rs:398, src/cmd_mcp_native.rs:399, src/cmd_mcp_native.rs:400, src/cmd_mcp_native.rs:401, src/cmd_mcp_native.rs:402, src/cmd_mcp_native.rs:403, src/cmd_mcp_native.rs:404, src/cmd_mcp_native.rs:405, src/cmd_mcp_native.rs:406, src/cmd_mcp_native.rs:407, src/cmd_mcp_native.rs:408, src/cmd_mcp_native.rs:409, src/cmd_mcp_native.rs:410, src/cmd_mcp_native.rs:411, src/cmd_mcp_native.rs:412, src/cmd_mcp_native.rs:413, src/cmd_mcp_native.rs:414, src/cmd_mcp_native.rs:415, src/cmd_mcp_native.rs:416, src/cmd_mcp_native.rs:417, src/cmd_mcp_native.rs:418, src/cmd_mcp_native.rs:419, src/cmd_mcp_native.rs:420, src/cmd_mcp_native.rs:421, src/cmd_mcp_native.rs:422, src/cmd_mcp_native.rs:423`

## Single Call Expression

Exact call-site lookup for packet28_search_via_session(root, session, request.clone()).

- Root: `crates/suite-cli`
- Canonical hits (`ripgrep`): `src/cmd_mcp_native.rs:283`
- Packet28 daemon backend: `indexed_regex` transport: `daemon` total: `1`
- Packet28 inproc backend: `indexed_regex` transport: `inproc` total: `1`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 3.623 | 16 | 16.0 | 100% | 100% | yes |
| `packet28-inproc` | 8.560 | 16 | 16.0 | 100% | 100% | yes |
| `ripgrep` | 8.198 | 16 | 16.0 | 100% | 100% | yes |
| `grep` | 15.807 | 16 | 16.0 | 100% | 100% | yes |
| `ast-grep` | 25.058 | 16 | 16.0 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'packet28_search_via_session\(root, session, request\.clone\(\)\)' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'packet28_search_via_session\(root, session, request\.clone\(\)\)' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'packet28_search_via_session\(root, session, request\.clone\(\)\)' crates/suite-cli`
- `grep`: `grep -RInE --color=never 'packet28_search_via_session\(root, session, request\.clone\(\)\)' crates/suite-cli`
- `ast-grep`: `ast-grep run --lang rust --heading never --color never -C 0 -p 'packet28_search_via_session(root, session, request.clone())' crates/suite-cli`

### Match Sets

- `packet28-daemon` found: `src/cmd_mcp_native.rs:283`
- `packet28-inproc` found: `src/cmd_mcp_native.rs:283`
- `ripgrep` found: `src/cmd_mcp_native.rs:283`
- `grep` found: `src/cmd_mcp_native.rs:283`
- `ast-grep` found: `src/cmd_mcp_native.rs:283`

## Daemon Call Expression

Exact call-site lookup for daemon_packet28_search(state, request).

- Root: `crates/packet28d`
- Canonical hits (`ripgrep`): `src/server.rs:320`
- Packet28 daemon backend: `indexed_regex` transport: `daemon` total: `1`
- Packet28 inproc backend: `indexed_regex` transport: `inproc` total: `1`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 2.994 | 14 | 14.0 | 100% | 100% | yes |
| `packet28-inproc` | 8.111 | 14 | 14.0 | 100% | 100% | yes |
| `ripgrep` | 8.230 | 14 | 14.0 | 100% | 100% | yes |
| `grep` | 6.677 | 14 | 14.0 | 100% | 100% | yes |
| `ast-grep` | 13.203 | 14 | 14.0 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28d 'daemon_packet28_search\(state, request\)' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28d 'daemon_packet28_search\(state, request\)' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'daemon_packet28_search\(state, request\)' crates/packet28d`
- `grep`: `grep -RInE --color=never 'daemon_packet28_search\(state, request\)' crates/packet28d`
- `ast-grep`: `ast-grep run --lang rust --heading never --color never -C 0 -p 'daemon_packet28_search(state, request)' crates/packet28d`

### Match Sets

- `packet28-daemon` found: `src/server.rs:320`
- `packet28-inproc` found: `src/server.rs:320`
- `ripgrep` found: `src/server.rs:320`
- `grep` found: `src/server.rs:320`
- `ast-grep` found: `src/server.rs:320`

## Anchored Struct Literal

Anchored line-start regex for SearchRequest literal construction in the standalone search CLI.

- Root: `crates/packet28-search-cli`
- Canonical hits (`ripgrep`): `src/main.rs:172`
- Packet28 daemon backend: `legacy_rg` transport: `daemon` total: `1`
- Packet28 inproc backend: `legacy_rg` transport: `inproc` total: `1`
- Packet28 daemon fallback reason: `candidate set remained too broad for indexed verification (2/3 files)`
- Packet28 inproc fallback reason: `candidate set remained too broad for indexed verification (2/3 files)`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 11.265 | 13 | 13.0 | 100% | 100% | yes |
| `packet28-inproc` | 16.626 | 13 | 13.0 | 100% | 100% | yes |
| `ripgrep` | 8.818 | 13 | 13.0 | 100% | 100% | yes |
| `grep` | 3.278 | 13 | 13.0 | 100% | 100% | yes |
| `ast-grep` | 10.161 | 14 | 14.0 | 10% | 100% | no |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli '^\s*SearchRequest\s*\{' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli '^\s*SearchRequest\s*\{' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never '^\s*SearchRequest\s*\{' crates/packet28-search-cli`
- `grep`: `grep -RInE --color=never '^[[:space:]]*SearchRequest[[:space:]]*\{' crates/packet28-search-cli`
- `ast-grep`: `ast-grep run --lang rust --heading never --color never -C 0 -p 'SearchRequest { $$$FIELDS }' crates/packet28-search-cli`

### Match Sets

- `packet28-daemon` found: `src/main.rs:172`
- `packet28-inproc` found: `src/main.rs:172`
- `ripgrep` found: `src/main.rs:172`
- `grep` found: `src/main.rs:172`
- `ast-grep` found: `src/main.rs:172, src/main.rs:173, src/main.rs:174, src/main.rs:175, src/main.rs:176, src/main.rs:177, src/main.rs:178, src/main.rs:179, src/main.rs:180, src/main.rs:181`
  extra: `src/main.rs:173, src/main.rs:174, src/main.rs:175, src/main.rs:176, src/main.rs:177, src/main.rs:178, src/main.rs:179, src/main.rs:180, src/main.rs:181`

## Alternation-Heavy Regex

Alternation over three standalone CLI command handlers.

- Root: `crates/packet28-search-cli`
- Canonical hits (`ripgrep`): `src/main.rs:101, src/main.rs:110, src/main.rs:137`
- Packet28 daemon backend: `indexed_regex` transport: `daemon` total: `3`
- Packet28 inproc backend: `indexed_regex` transport: `inproc` total: `3`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 4.599 | 13 | 4.3 | 100% | 100% | yes |
| `packet28-inproc` | 8.695 | 13 | 4.3 | 100% | 100% | yes |
| `ripgrep` | 8.135 | 13 | 4.3 | 100% | 100% | yes |
| `grep` | 3.119 | 13 | 4.3 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli 'fn\s+(?:run_query\|run_guard\|run_bench)\(' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli 'fn\s+(?:run_query\|run_guard\|run_bench)\(' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'fn\s+(?:run_query\|run_guard\|run_bench)\(' crates/packet28-search-cli`
- `grep`: `grep -RInE --color=never 'fn[[:space:]]+(run_query\|run_guard\|run_bench)\(' crates/packet28-search-cli`

### Match Sets

- `packet28-daemon` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137`
- `packet28-inproc` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137`
- `ripgrep` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137`
- `grep` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137`

## Broad But Selective Regex

Cross-file alternation over Packet28 search/read/fetch handler names in suite-cli.

- Root: `crates/suite-cli`
- Canonical hits (`ripgrep`): `src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:425, src/cmd_mcp_native.rs:552, src/cmd_mcp.rs:40, src/cmd_mcp.rs:41, src/cmd_mcp.rs:567, src/cmd_mcp.rs:579, src/cmd_mcp.rs:603`
- Packet28 daemon backend: `indexed_regex` transport: `daemon` total: `8`
- Packet28 inproc backend: `indexed_regex` transport: `inproc` total: `8`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 5.579 | 21 | 2.6 | 100% | 100% | yes |
| `packet28-inproc` | 10.642 | 21 | 2.6 | 100% | 100% | yes |
| `ripgrep` | 8.936 | 21 | 2.6 | 100% | 100% | yes |
| `grep` | 16.378 | 21 | 2.6 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'handle_packet28_(?:search\|read_regions\|fetch_tool_result)' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/suite-cli 'handle_packet28_(?:search\|read_regions\|fetch_tool_result)' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'handle_packet28_(?:search\|read_regions\|fetch_tool_result)' crates/suite-cli`
- `grep`: `grep -RInE --color=never 'handle_packet28_(search\|read_regions\|fetch_tool_result)' crates/suite-cli`

### Match Sets

- `packet28-daemon` found: `src/cmd_mcp.rs:40, src/cmd_mcp.rs:41, src/cmd_mcp.rs:567, src/cmd_mcp.rs:579, src/cmd_mcp.rs:603, src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:425, src/cmd_mcp_native.rs:552`
- `packet28-inproc` found: `src/cmd_mcp.rs:40, src/cmd_mcp.rs:41, src/cmd_mcp.rs:567, src/cmd_mcp.rs:579, src/cmd_mcp.rs:603, src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:425, src/cmd_mcp_native.rs:552`
- `ripgrep` found: `src/cmd_mcp.rs:40, src/cmd_mcp.rs:41, src/cmd_mcp.rs:567, src/cmd_mcp.rs:579, src/cmd_mcp.rs:603, src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:425, src/cmd_mcp_native.rs:552`
- `grep` found: `src/cmd_mcp.rs:40, src/cmd_mcp.rs:41, src/cmd_mcp.rs:567, src/cmd_mcp.rs:579, src/cmd_mcp.rs:603, src/cmd_mcp_native.rs:256, src/cmd_mcp_native.rs:425, src/cmd_mcp_native.rs:552`

## Broad Declaration Regex

Broad declaration regex over the packet28-search-core crate.

- Root: `crates/packet28-search-core`
- Canonical hits (`ripgrep`): `src/weights.rs:7, src/lib.rs:52, src/lib.rs:86, src/lib.rs:92, src/lib.rs:264, src/lib.rs:357, src/lib.rs:409, src/lib.rs:483, src/lib.rs:492, src/lib.rs:560, src/lib.rs:2547, src/lib.rs:2552, src/lib.rs:2594, src/lib.rs:2605, src/lib.rs:2754, src/lib.rs:2770, src/lib.rs:2787, src/lib.rs:2805`
- Packet28 daemon backend: `legacy_rg` transport: `daemon` total: `18`
- Packet28 inproc backend: `legacy_rg` transport: `inproc` total: `18`
- Packet28 daemon fallback reason: `planner could not derive an index-safe branch set`
- Packet28 inproc fallback reason: `planner could not derive an index-safe branch set`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 10.783 | 19 | 1.1 | 100% | 100% | yes |
| `packet28-inproc` | 15.629 | 19 | 1.1 | 100% | 100% | yes |
| `ripgrep` | 6.990 | 19 | 1.1 | 100% | 100% | yes |
| `grep` | 7.976 | 19 | 1.1 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-core 'pub\s+(?:fn\|struct\|enum)\s+[A-Za-z_][A-Za-z0-9_]*' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-core 'pub\s+(?:fn\|struct\|enum)\s+[A-Za-z_][A-Za-z0-9_]*' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'pub\s+(?:fn\|struct\|enum)\s+[A-Za-z_][A-Za-z0-9_]*' crates/packet28-search-core`
- `grep`: `grep -RInE --color=never 'pub[[:space:]]+(fn\|struct\|enum)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*' crates/packet28-search-core`

### Match Sets

- `packet28-daemon` found: `src/lib.rs:2547, src/lib.rs:2552, src/lib.rs:2594, src/lib.rs:2605, src/lib.rs:264, src/lib.rs:2754, src/lib.rs:2770, src/lib.rs:2787, src/lib.rs:2805, src/lib.rs:357, src/lib.rs:409, src/lib.rs:483, src/lib.rs:492, src/lib.rs:52, src/lib.rs:560, src/lib.rs:86, src/lib.rs:92, src/weights.rs:7`
- `packet28-inproc` found: `src/lib.rs:2547, src/lib.rs:2552, src/lib.rs:2594, src/lib.rs:2605, src/lib.rs:264, src/lib.rs:2754, src/lib.rs:2770, src/lib.rs:2787, src/lib.rs:2805, src/lib.rs:357, src/lib.rs:409, src/lib.rs:483, src/lib.rs:492, src/lib.rs:52, src/lib.rs:560, src/lib.rs:86, src/lib.rs:92, src/weights.rs:7`
- `ripgrep` found: `src/lib.rs:2547, src/lib.rs:2552, src/lib.rs:2594, src/lib.rs:2605, src/lib.rs:264, src/lib.rs:2754, src/lib.rs:2770, src/lib.rs:2787, src/lib.rs:2805, src/lib.rs:357, src/lib.rs:409, src/lib.rs:483, src/lib.rs:492, src/lib.rs:52, src/lib.rs:560, src/lib.rs:86, src/lib.rs:92, src/weights.rs:7`
- `grep` found: `src/lib.rs:2547, src/lib.rs:2552, src/lib.rs:2594, src/lib.rs:2605, src/lib.rs:264, src/lib.rs:2754, src/lib.rs:2770, src/lib.rs:2787, src/lib.rs:2805, src/lib.rs:357, src/lib.rs:409, src/lib.rs:483, src/lib.rs:492, src/lib.rs:52, src/lib.rs:560, src/lib.rs:86, src/lib.rs:92, src/weights.rs:7`

## Common Function Sweep

Common function-signature regex over the standalone search CLI.

- Root: `crates/packet28-search-cli`
- Canonical hits (`ripgrep`): `src/main.rs:79, src/main.rs:89, src/main.rs:101, src/main.rs:110, src/main.rs:137, src/main.rs:171, src/main.rs:184, src/main.rs:204, src/main.rs:237, src/main.rs:258, src/main.rs:281, src/main.rs:307, src/main.rs:344, src/main.rs:360, src/main.rs:365, src/main.rs:370, src/main.rs:374, src/main.rs:380, src/main.rs:384, src/main.rs:396, src/main.rs:441, src/main.rs:451, tests/e2e.rs:12, tests/e2e.rs:16, tests/e2e.rs:31, tests/e2e.rs:35, tests/e2e.rs:40, tests/e2e.rs:52, tests/e2e.rs:98, tests/e2e.rs:113, tests/e2e.rs:155, tests/e2e.rs:160, tests/e2e.rs:182, tests/e2e.rs:207`
- Packet28 daemon backend: `legacy_rg` transport: `daemon` total: `34`
- Packet28 inproc backend: `legacy_rg` transport: `inproc` total: `34`
- Packet28 daemon fallback reason: `planner derived only weak/common literals; routing broad regex to legacy_rg`
- Packet28 inproc fallback reason: `planner derived only weak/common literals; routing broad regex to legacy_rg`

| Tool | Mean ms | Compact Tokens | Tokens / True Hit | Precision | Recall | Exact |
| --- | ---: | ---: | ---: | ---: | ---: | :---: |
| `packet28-daemon` | 12.773 | 19 | 0.6 | 100% | 100% | yes |
| `packet28-inproc` | 17.020 | 19 | 0.6 | 100% | 100% | yes |
| `ripgrep` | 7.097 | 19 | 0.6 | 100% | 100% | yes |
| `grep` | 2.607 | 19 | 0.6 | 100% | 100% | yes |

### Commands

- `packet28-daemon`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli 'fn\s+[a-z_][A-Za-z0-9_]*\(' --engine auto --transport daemon --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `packet28-inproc`: `/Users/utsavsharma/Documents/GitHub/Coverage/target/release/packet28-search-cli query /Users/utsavsharma/Documents/GitHub/Coverage/crates/packet28-search-cli 'fn\s+[a-z_][A-Za-z0-9_]*\(' --engine auto --transport inproc --max-matches-per-file 1000 --max-total-matches 1000 --compact`
- `ripgrep`: `rg -n --no-heading --color never 'fn\s+[a-z_][A-Za-z0-9_]*\(' crates/packet28-search-cli`
- `grep`: `grep -RInE --color=never 'fn[[:space:]]+[a-z_][A-Za-z0-9_]*\(' crates/packet28-search-cli`

### Match Sets

- `packet28-daemon` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137, src/main.rs:171, src/main.rs:184, src/main.rs:204, src/main.rs:237, src/main.rs:258, src/main.rs:281, src/main.rs:307, src/main.rs:344, src/main.rs:360, src/main.rs:365, src/main.rs:370, src/main.rs:374, src/main.rs:380, src/main.rs:384, src/main.rs:396, src/main.rs:441, src/main.rs:451, src/main.rs:79, src/main.rs:89, tests/e2e.rs:113, tests/e2e.rs:12, tests/e2e.rs:155, tests/e2e.rs:16, tests/e2e.rs:160, tests/e2e.rs:182, tests/e2e.rs:207, tests/e2e.rs:31, tests/e2e.rs:35, tests/e2e.rs:40, tests/e2e.rs:52, tests/e2e.rs:98`
- `packet28-inproc` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137, src/main.rs:171, src/main.rs:184, src/main.rs:204, src/main.rs:237, src/main.rs:258, src/main.rs:281, src/main.rs:307, src/main.rs:344, src/main.rs:360, src/main.rs:365, src/main.rs:370, src/main.rs:374, src/main.rs:380, src/main.rs:384, src/main.rs:396, src/main.rs:441, src/main.rs:451, src/main.rs:79, src/main.rs:89, tests/e2e.rs:113, tests/e2e.rs:12, tests/e2e.rs:155, tests/e2e.rs:16, tests/e2e.rs:160, tests/e2e.rs:182, tests/e2e.rs:207, tests/e2e.rs:31, tests/e2e.rs:35, tests/e2e.rs:40, tests/e2e.rs:52, tests/e2e.rs:98`
- `ripgrep` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137, src/main.rs:171, src/main.rs:184, src/main.rs:204, src/main.rs:237, src/main.rs:258, src/main.rs:281, src/main.rs:307, src/main.rs:344, src/main.rs:360, src/main.rs:365, src/main.rs:370, src/main.rs:374, src/main.rs:380, src/main.rs:384, src/main.rs:396, src/main.rs:441, src/main.rs:451, src/main.rs:79, src/main.rs:89, tests/e2e.rs:113, tests/e2e.rs:12, tests/e2e.rs:155, tests/e2e.rs:16, tests/e2e.rs:160, tests/e2e.rs:182, tests/e2e.rs:207, tests/e2e.rs:31, tests/e2e.rs:35, tests/e2e.rs:40, tests/e2e.rs:52, tests/e2e.rs:98`
- `grep` found: `src/main.rs:101, src/main.rs:110, src/main.rs:137, src/main.rs:171, src/main.rs:184, src/main.rs:204, src/main.rs:237, src/main.rs:258, src/main.rs:281, src/main.rs:307, src/main.rs:344, src/main.rs:360, src/main.rs:365, src/main.rs:370, src/main.rs:374, src/main.rs:380, src/main.rs:384, src/main.rs:396, src/main.rs:441, src/main.rs:451, src/main.rs:79, src/main.rs:89, tests/e2e.rs:113, tests/e2e.rs:12, tests/e2e.rs:155, tests/e2e.rs:16, tests/e2e.rs:160, tests/e2e.rs:182, tests/e2e.rs:207, tests/e2e.rs:31, tests/e2e.rs:35, tests/e2e.rs:40, tests/e2e.rs:52, tests/e2e.rs:98`

## Observations

- Packet28 is measured on both the resident daemon transport and the in-process CLI path. The daemon path is the primary “instant grep” target; the in-process path remains exact and competitive.
- Guarded `rg` fallback remains part of Packet28 for broad or unselective regexes, but fallback reasons are preserved in the Packet28 result rather than forcing the caller to replay the search.
- `ast-grep` remains only an external comparison point for regex-expressible code-shaped scenarios; Packet28 does not delegate to it.

