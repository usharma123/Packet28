#!/usr/bin/env python3
"""Native MCP retrieval and source-freshness contracts for the agent-DX benchmark.

The runner and the validator call the same pure checks on the captured
JSON-RPC wire, so a validator rerun does not trust any verdict, count or
ledger the runner wrote. Expected facts are derived from the fixture source
bytes declared here, never from what the product reported about itself:

- the fixed-string search must return exactly the 32 source lines that contain
  the query (no missing, extra or duplicated match);
- slim search and glob responses may carry only their documented navigational
  fields, within the slim limits of `cmd_mcp_native_search.rs`, and must be
  smaller than the full result of the same invocation in bytes and tokens;
- the definition the task needs lies outside the slim preview, so its fetch
  and the follow-up read are required retrievals that must run and be counted;
- per-phase round trips, bytes and estimated tokens are recomputed from the
  wire responses linked to each recorded step; elapsed time comes from the
  step ledger and must add up to the reported phase totals.
"""

from __future__ import annotations

import json
from collections import Counter

from benchmark_common import estimate_tokens

TASK_SEARCH = "agent-dx-native"
SEARCH_QUERY = "apply_discount"
GLOB_PATTERN = "src/**/*.rs"
DEFINITION_PATH = "src/teller/discount.rs"
READ_LINES = 3

# crates/suite-cli/src/cmd_mcp_native_search.rs:18-21 and build_search_slim_payload.
SLIM_LIMITS = {"paths": 6, "regions": 8, "symbols": 4, "diagnostics": 4}
SLIM_SEARCH_FIELDS = {
    "match_count", "returned_match_count", "truncated", "paths", "regions", "symbols", "diagnostics",
    "compact_preview", "engine", "search_strategy", "hybrid", "artifact_id", "response_mode",
}
SLIM_ENGINE_FIELDS = {"engine", "plan_kind", "planner_fallback", "stale_reason", "fallback_reason"}
SLIM_HYBRID_FIELDS = {"primary_backend", "secondary_backend", "shadowed", "added_displayed_matches", "added_paths", "notes"}
SLIM_GLOB_FIELDS = {"artifact_id", "compact_preview", "match_count", "returned_match_count", "truncated", "response_mode"}

PHASES = ("initial", "required_retrieval", "verification_retrieval")
# Role of each exchange in the native trace and the ledger phase it belongs to.
ROLE_PHASES = {
    "search": "initial",
    "glob": "initial",
    "search_fetch": "required_retrieval",
    "read": "required_retrieval",
    "glob_fetch": "verification_retrieval",
    "read_fetch": "verification_retrieval",
}

# Source freshness: one out-of-band edit of a tracked, indexed line.
FRESHNESS_PATH = DEFINITION_PATH
FRESHNESS_OLD = "ledger-discount-v1"
FRESHNESS_NEW = "ledger-discount-v2-agent-dx-fresh"


def fixture_files() -> dict[str, bytes]:
    """A small priced-ledger crate whose search results exceed the slim limits."""
    regions = [f"region_{index:02d}" for index in range(1, 11)]
    files = {
        "Cargo.toml": '[package]\nname = "ledger"\nversion = "0.1.0"\nedition = "2021"\n',
        "src/lib.rs": "pub mod pricing;\npub mod teller;\n",
        "src/pricing/mod.rs": "".join(f"pub mod {name};\n" for name in regions),
        "src/teller/mod.rs": "pub mod discount;\n",
        "src/teller/discount.rs": (
            "/// Integer-cent discounts shared by every pricing region.\n"
            "pub fn apply_discount(cents: u64, percent: u64) -> u64 {\n"
            "    // Rounds the discount down; ticket LEDGER-7 asks for nearest-cent rounding.\n"
            "    cents - cents * percent / 100\n"
            "}\n"
            "\n"
            "// Prüfsumme für Rabatte – ledger audit marker ✓\n"
            "pub const DISCOUNT_AUDIT: &str = \"ledger-discount-v1\";\n"
        ),
        "docs/pricing.md": "# Pricing\n\nRegional prices call `apply_discount` from the teller module.\n",
    }
    for index, name in enumerate(regions, 1):
        files[f"src/pricing/{name}.rs"] = (
            "use crate::teller::discount::apply_discount;\n\n"
            f"pub fn seasonal_price_{index:02d}(cents: u64) -> u64 {{\n"
            f"    apply_discount(cents, {index})\n}}\n\n"
            f"pub fn member_price_{index:02d}(cents: u64) -> u64 {{\n"
            f"    apply_discount(cents, {index + 10})\n}}\n"
        )
    return {path: text.encode("utf-8") for path, text in sorted(files.items())}


def source_lines(data: bytes) -> list[str]:
    return data.decode("utf-8").split("\n")


def expected_matches(files: dict[str, bytes], query: str) -> list[str]:
    """Every `path:line:text` source line containing the fixed-string query."""
    found = []
    for path, data in sorted(files.items()):
        for number, line in enumerate(source_lines(data), 1):
            if query in line:
                found.append(f"{path}:{number}:{line}")
    return found


def expected_glob(files: dict[str, bytes]) -> list[str]:
    return sorted(path for path in files if path.startswith("src/") and path.endswith(".rs"))


def expected_read(files: dict[str, bytes]) -> tuple[int, str]:
    """The read starts after the definition and spans a blank and a non-ASCII line."""
    lines = source_lines(files[DEFINITION_PATH])
    definition = next(n for n, line in enumerate(lines, 1) if f"pub fn {SEARCH_QUERY}" in line)
    start = definition + 3
    return start, "\n".join(f"{n}: {lines[n - 1]}" for n in range(start, start + READ_LINES))


def parse_wire(wire: bytes) -> list[dict]:
    """Pair `> ` requests with `< ` responses by JSON-RPC id, in request order."""
    requests: dict = {}
    order = []
    for line in wire.splitlines(keepends=True):
        direction, raw = line[:2], line[2:]
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict) or "id" not in message:
            continue
        if direction == b"> ":
            requests[message["id"]] = {"id": message["id"], "request": message, "response": None, "response_raw": None}
            order.append(message["id"])
        elif direction == b"< " and message["id"] in requests and requests[message["id"]]["response"] is None:
            requests[message["id"]].update(response=message, response_raw=raw)
    return [requests[key] for key in order]


def tool_calls(exchanges: list[dict]) -> list[dict]:
    calls = []
    for exchange in exchanges:
        request = exchange["request"]
        if request.get("method") != "tools/call":
            continue
        params = request.get("params") or {}
        result = (exchange["response"] or {}).get("result") or {}
        calls.append({
            "id": exchange["id"], "tool": params.get("name"), "arguments": params.get("arguments") or {},
            "structured": result.get("structuredContent") if isinstance(result, dict) else None,
            "text": [item.get("text", "") for item in result.get("content", []) if isinstance(item, dict)]
            if isinstance(result, dict) else [],
            "error": (exchange["response"] or {}).get("error") or (result.get("isError") if isinstance(result, dict) else None),
            "raw": exchange["response_raw"],
        })
    return calls


def _size(call: dict) -> tuple[int, int, int]:
    raw = call["raw"] or b""
    structured = len(json.dumps(call["structured"]).encode()) if call["structured"] is not None else 0
    return len(raw), estimate_tokens(raw.decode("utf-8", "replace")), structured


def assign_roles(calls: list[dict]) -> dict[str, dict]:
    """Bind the trace's required exchanges by tool, arguments and returned handles."""
    roles: dict[str, dict] = {}

    def first(predicate):
        return next((call for call in calls if predicate(call)), None)

    def handle(role):
        return ((roles.get(role) or {}).get("structured") or {}).get("artifact_id")

    def fetch_of(role):
        artifact = handle(role)
        return first(lambda c: c["tool"] == "packet28_fetch_tool_result" and artifact
                     and c["arguments"].get("artifact_id") == artifact)

    roles["search"] = first(lambda c: c["tool"] == "packet28_search" and c["arguments"].get("query") == SEARCH_QUERY)
    roles["glob"] = first(lambda c: c["tool"] == "packet28_glob" and c["arguments"].get("pattern") == GLOB_PATTERN)
    roles["read"] = first(lambda c: c["tool"] == "packet28_read_regions" and c["arguments"].get("path") == DEFINITION_PATH)
    roles["search_fetch"] = fetch_of("search")
    roles["glob_fetch"] = fetch_of("glob")
    roles["read_fetch"] = fetch_of("read")
    return {role: call for role, call in roles.items() if call is not None}


def _one_line(value) -> bool:
    return isinstance(value, str) and "\n" not in value


def _slim_search_errors(slim: dict, matches: list[str], text: list[str]) -> list[str]:
    errors = []
    extra = sorted(set(slim) - SLIM_SEARCH_FIELDS)
    if extra:
        errors.append(f"slim search carries undocumented fields {extra}")
    if slim.get("response_mode") != "slim":
        errors.append(f"response_mode={slim.get('response_mode')!r}")
    if not slim.get("artifact_id"):
        errors.append("slim search returned no fetchable artifact")
    for field, limit in SLIM_LIMITS.items():
        items = slim.get(field, [])
        if not isinstance(items, list) or len(items) > limit:
            errors.append(f"{field} has {len(items) if isinstance(items, list) else items!r} entries, limit {limit}")
        elif field != "regions" and len(set(map(str, items))) != len(items):
            errors.append(f"{field} repeats an entry")
        if isinstance(items, list) and field == "diagnostics" and not all(_one_line(item) for item in items):
            errors.append("a slim diagnostic is not one line")
    by_path: dict[str, set[int]] = {}
    for match in matches:
        path, number, _ = match.split(":", 2)
        by_path.setdefault(path, set()).add(int(number))
    for path in slim.get("paths", []) or []:
        if path not in by_path:
            errors.append(f"slim path {path!r} has no source match")
    for region in slim.get("regions", []) or []:
        path, _, span = str(region).rpartition(":")
        start, _, end = span.partition("-")
        try:
            lines = range(int(start), int(end or start) + 1)
        except ValueError:
            errors.append(f"slim region {region!r} is malformed")
            continue
        if not by_path.get(path, set()) & set(lines):
            errors.append(f"slim region {region!r} covers no source match")
    texts = [match.split(":", 2)[2] for match in matches]
    for symbol in slim.get("symbols", []) or []:
        if not any(str(symbol) in line for line in texts):
            errors.append(f"slim symbol {symbol!r} occurs in no matched line")
    if not _one_line(slim.get("compact_preview")):
        errors.append("slim compact_preview is not a single line")
    for field, allowed in (("engine", SLIM_ENGINE_FIELDS), ("hybrid", SLIM_HYBRID_FIELDS)):
        value = slim.get(field) or {}
        if not isinstance(value, dict) or set(value) - allowed:
            errors.append(f"slim {field} carries undocumented fields {sorted(set(value) - allowed) if isinstance(value, dict) else value!r}")
            continue
        for item in value.values():
            items = item if isinstance(item, list) else [item]
            if any(isinstance(x, str) and not _one_line(x) for x in items):
                errors.append(f"slim {field} carries a multi-line value")
    if not all(_one_line(item) for item in text):
        errors.append("slim text content spans more than one line")
    return errors


def check_native_retrieval(wire: bytes, files: dict[str, bytes]) -> tuple[dict[str, tuple[bool, str]], dict]:
    """Check the native retrieval trace; return check verdicts and bound roles."""
    calls = tool_calls(parse_wire(wire))
    roles = assign_roles(calls)
    matches = expected_matches(files, SEARCH_QUERY)
    checks: dict[str, tuple[bool, str]] = {}

    def missing(*names):
        absent = [name for name in names if name not in roles]
        failed = [name for name in names if name in roles and (roles[name]["error"] or roles[name]["structured"] is None)]
        return absent, failed

    absent, failed = missing("search", "search_fetch")
    if absent or failed:
        checks["search_returns_fetchable_artifact"] = (False, f"required exchanges missing {absent} or failed {failed}")
        checks["fetched_matches_equal_source"] = (False, f"required exchanges missing {absent} or failed {failed}")
    else:
        slim, full = roles["search"]["structured"], roles["search_fetch"]["structured"]
        errors = _slim_search_errors(slim, matches, roles["search"]["text"])
        if slim.get("match_count") != len(matches):
            errors.append(f"slim match_count {slim.get('match_count')} but the source has {len(matches)} matches")
        if DEFINITION_PATH in (slim.get("paths") or []):
            errors.append("the needed definition is inside the slim preview, so the retrieval is not required")
        slim_size, full_size = _size(roles["search"]), _size(roles["search_fetch"])
        if not (slim_size[0] < full_size[0] and slim_size[1] < full_size[1] and slim_size[2] < full_size[2]):
            errors.append(f"slim response {slim_size[0]} B/{slim_size[1]} tok/{slim_size[2]} B structured is not smaller "
                          f"than the full result {full_size[0]} B/{full_size[1]} tok/{full_size[2]} B structured")
        checks["search_returns_fetchable_artifact"] = (
            not errors, "; ".join(errors) or
            f"slim {len(slim.get('paths', []))} paths/{len(slim.get('regions', []))} regions/"
            f"{len(slim.get('symbols', []))} symbols/{len(slim.get('diagnostics', []))} diagnostics; "
            f"{slim_size[0]} B/{slim_size[1]} tok vs full {full_size[0]} B/{full_size[1]} tok")
        errors = []
        got = [line for line in str(full.get("content") or "").split("\n") if line]
        counted = Counter(got)
        expected = Counter(matches)
        dropped = sorted((expected - counted).elements())
        added = sorted((counted - expected).elements())
        if dropped:
            errors.append(f"{len(dropped)} source matches missing, first {dropped[0]!r}")
        if added:
            errors.append(f"{len(added)} extra or duplicated matches, first {added[0]!r}")
        for key in ("match_count", "returned_match_count"):
            if key in full and full.get(key) != len(matches):
                errors.append(f"full {key} {full.get(key)} but the source has {len(matches)} matches")
        if full.get("task_id") != TASK_SEARCH or full.get("artifact_id") != slim.get("artifact_id"):
            errors.append(f"owner {full.get('task_id')!r}/{full.get('artifact_id')!r} is not the search's task and artifact")
        if full.get("query") != SEARCH_QUERY:
            errors.append(f"full result query {full.get('query')!r}")
        if full.get("content_format") != "path:line:text" or "groups" in full or full.get("truncated"):
            errors.append(f"format={full.get('content_format')} groups={'groups' in full} truncated={full.get('truncated')}")
        checks["fetched_matches_equal_source"] = (
            not errors, "; ".join(errors) or f"{len(got)}/{len(matches)} source-derived matches, exact set, no duplicates")
    absent, failed = missing("read")
    if absent or failed:
        checks["read_regions_exact"] = (False, f"required read missing {absent} or failed {failed}")
    else:
        start, content = expected_read(files)
        args, read = roles["read"]["arguments"], roles["read"]["structured"]
        span = (args.get("line_start"), args.get("line_end"))
        ok = span == (start, start + READ_LINES - 1) and read.get("content") == content and read.get("line_count") == READ_LINES
        checks["read_regions_exact"] = (ok, f"requested {DEFINITION_PATH}:{span[0]}-{span[1]}, expected {start}-{start + READ_LINES - 1} "
                                            f"(blank and non-ASCII lines); exact={read.get('content') == content}")
    absent, failed = missing("glob", "glob_fetch")
    if absent or failed:
        checks["glob_paths_exist"] = (False, f"glob exchanges missing {absent} or failed {failed}")
    else:
        slim, full = roles["glob"]["structured"], roles["glob_fetch"]["structured"]
        paths = full.get("paths") or []
        want = expected_glob(files)
        errors = []
        if set(slim) - SLIM_GLOB_FIELDS:
            errors.append(f"slim glob carries undocumented fields {sorted(set(slim) - SLIM_GLOB_FIELDS)}")
        if slim.get("match_count") != len(want) or full.get("match_count") != len(want):
            errors.append(f"match_count slim={slim.get('match_count')} full={full.get('match_count')}, source has {len(want)}")
        if sorted(paths) != want or len(paths) != len(set(paths)):
            errors.append(f"{len(paths)} paths, expected exactly {len(want)}")
        if full.get("truncated"):
            errors.append("full glob is truncated")
        slim_size, full_size = _size(roles["glob"]), _size(roles["glob_fetch"])
        if not (slim_size[0] < full_size[0] and slim_size[2] < full_size[2]):
            errors.append(f"slim glob {slim_size[0]} B is not smaller than full {full_size[0]} B")
        checks["glob_paths_exist"] = (not errors, "; ".join(errors) or
                                      f"{len(paths)} paths; slim {slim_size[2]} B vs full {full_size[2]} B structured")
    return checks, roles


def native_ledger(wire: bytes, steps: list[dict]) -> tuple[dict, list[str]]:
    """Recompute per-phase totals from wire responses linked to step records.

    Each step names the JSON-RPC request id it measured. Its bytes and tokens
    must equal the linked wire response; the six trace roles must carry their
    contract phase; elapsed time is taken from the steps.
    """
    errors = []
    calls = {call["id"]: call for call in tool_calls(parse_wire(wire))}
    roles = assign_roles(list(calls.values()))
    by_id = {}
    for step in steps:
        request_id = step.get("request_id")
        if request_id in by_id:
            errors.append(f"request {request_id} is counted twice")
        by_id[request_id] = step
        call = calls.get(request_id)
        if call is None or call["raw"] is None:
            errors.append(f"step {step.get('tool')} names request {request_id!r} with no wire response")
            continue
        size = _size(call)
        if (step.get("response_bytes"), step.get("response_est_tokens")) != size[:2]:
            errors.append(f"request {request_id} recorded {step.get('response_bytes')} B/{step.get('response_est_tokens')} tok, "
                          f"wire has {size[0]} B/{size[1]} tok")
        if step.get("phase") not in PHASES or not isinstance(step.get("elapsed_ms"), (int, float)) or step["elapsed_ms"] < 0:
            errors.append(f"request {request_id} has phase {step.get('phase')!r} elapsed {step.get('elapsed_ms')!r}")
    for request_id in calls:
        if request_id not in by_id:
            errors.append(f"tool call {request_id} ({calls[request_id]['tool']}) is not counted in any phase")
    for role, phase in ROLE_PHASES.items():
        call = roles.get(role)
        if call is None:
            errors.append(f"{role} exchange was not executed")
        elif (by_id.get(call["id"]) or {}).get("phase") != phase:
            errors.append(f"{role} exchange is counted as {(by_id.get(call['id']) or {}).get('phase')!r}, contract phase {phase}")
    ledger = {}
    for phase in PHASES:
        members = [step for step in steps if step.get("phase") == phase and step.get("request_id") in calls]
        ledger[phase] = {
            "round_trips": len(members),
            "response_bytes": sum(_size(calls[s["request_id"]])[0] for s in members),
            "est_tokens": sum(_size(calls[s["request_id"]])[1] for s in members),
            "elapsed_ms": round(sum(float(s.get("elapsed_ms") or 0) for s in members), 1),
        }
    return ledger, errors


def ledger_metrics(ledger: dict) -> dict:
    return {
        "mcp_round_trips": sum(item["round_trips"] for item in ledger.values()),
        "acquisition_tokens": ledger["initial"]["est_tokens"],
        "required_retrieval_tokens": ledger["initial"]["est_tokens"] + ledger["required_retrieval"]["est_tokens"],
        "required_elapsed_ms": round(ledger["initial"]["elapsed_ms"] + ledger["required_retrieval"]["elapsed_ms"], 1),
        "verification_retrieval_tokens": ledger["verification_retrieval"]["est_tokens"],
        "all_calls_tokens": sum(item["est_tokens"] for item in ledger.values()),
        "ledgers": ledger,
    }


def edited_freshness_source(original: bytes) -> bytes:
    edited = original.replace(FRESHNESS_OLD.encode(), FRESHNESS_NEW.encode())
    if edited == original:
        raise ValueError(f"{FRESHNESS_PATH} does not contain {FRESHNESS_OLD!r}")
    return edited


def search_result_matches(result: dict) -> list[str]:
    """`path:line:text` lines of a full search result (MCP full mode or daemon response)."""
    lines = []
    for group in result.get("groups") or []:
        for match in group.get("matches") or []:
            lines.append(f"{match.get('path')}:{match.get('line')}:{match.get('text')}")
    return lines


def check_freshness(wire: bytes, forced: dict, edited: bytes) -> dict[str, tuple[bool, str]]:
    """After an unreported edit, normal search shows current source and forced index never serves stale."""
    calls = [call for call in tool_calls(parse_wire(wire)) if call["tool"] == "packet28_search"]
    by_query = {call["arguments"].get("query"): call for call in calls}
    current = {FRESHNESS_PATH: edited}
    checks = {}
    new_call, old_call = by_query.get(FRESHNESS_NEW), by_query.get(FRESHNESS_OLD)
    want_new = expected_matches(current, FRESHNESS_NEW)
    if new_call is None or new_call["error"] or new_call["structured"] is None:
        checks["edit_visible_to_search"] = (False, "search for the new marker did not run or failed")
    else:
        got = search_result_matches(new_call["structured"])
        engine = (new_call["structured"].get("engine") or {})
        checks["edit_visible_to_search"] = (
            Counter(got) == Counter(want_new),
            f"got {got} expected {want_new}; engine={engine.get('engine')} reason={engine.get('fallback_reason') or engine.get('stale_reason')}")
    if old_call is None or old_call["error"] or old_call["structured"] is None:
        checks["replaced_text_not_served"] = (False, "search for the replaced text did not run or failed")
    else:
        got = search_result_matches(old_call["structured"])
        checks["replaced_text_not_served"] = (
            not got and old_call["structured"].get("match_count") == 0, f"stale matches {got}")
    before, after = forced.get("before_edit") or {}, forced.get("after_edit") or {}
    want_old = expected_matches({FRESHNESS_PATH: fixture_files()[FRESHNESS_PATH]}, FRESHNESS_OLD)
    before_ok = before.get("type") == "packet28_search" and Counter(search_result_matches(before.get("response") or {})) == Counter(want_old)
    if after.get("type") == "error":
        verdict, detail = True, f"forced index refused: {str(after.get('message'))[:200]}"
    elif after.get("type") == "packet28_search":
        got = search_result_matches(after.get("response") or {})
        verdict = Counter(got) == Counter(want_new)
        detail = f"forced index answered with {'current' if verdict else 'stale'} source {got}"
    else:
        verdict, detail = False, f"forced search reply {after!r:.200}"
    checks["forced_index_never_stale"] = (
        before_ok and verdict, f"before edit forced search served={before_ok}; after edit: {detail}")
    return checks
