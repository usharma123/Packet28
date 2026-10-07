#!/usr/bin/env python3
"""Verify the checked-in PER-03 result and its explicit decision boundary."""

from __future__ import annotations

import hashlib
import json
import statistics
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RESULT_PATH = Path(__file__).with_name("result.json")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def median(result: dict[str, object], field: str) -> int:
    invocations = result["invocations"]
    assert isinstance(invocations, list)
    values = [invocation[field] for invocation in invocations]
    assert all(isinstance(value, int) and value > 0 for value in values)
    return int(statistics.median(values))


def main() -> None:
    result = json.loads(RESULT_PATH.read_text(encoding="utf-8"))
    assert result["schema_version"] == 1
    assert len(result["invocations"]) == 3

    mapy_whole = median(result, "mapy_whole_snapshot_us")
    mapy_incremental = median(result, "mapy_incremental_us")
    regex_full = median(result, "regex_full_overlay_us")
    regex_incremental = median(result, "regex_incremental_us")
    assert mapy_whole == 4_782
    assert mapy_incremental == 71_158
    assert mapy_incremental > mapy_whole
    assert regex_full == 387_995
    assert regex_incremental == 129_657
    assert regex_incremental < regex_full

    published = result["published_bytes"]
    assert published["mapy_incremental"] * 1000 < published["mapy_whole_snapshot"] * 2
    assert published["regex_incremental"] < published["regex_full_overlay"]

    work = result["mapy_incremental_work"]
    assert work == {
        "publication_metadata_bytes_decoded": 2711,
        "repository_artifact_bytes_decoded": 3439,
        "repository_artifacts_decoded": 1,
        "repository_artifact_bytes_hashed": 6878,
        "repository_artifact_metadata_checks": 42,
        "changed_paths_considered": 1,
    }

    decision = result["decision"]
    assert decision == {
        "mapy_architecture": "retain_bounded_durable_publication",
        "mapy_byte_gate": "pass",
        "mapy_elapsed_time_gate": "fail",
        "mapy_latency_claim": "rejected",
        "regex_architecture": "retain",
        "regex_byte_gate": "pass",
        "regex_elapsed_time_gate": "pass",
        "durability_coalescing": (
            "not_adopted_without_power_loss_equivalence_evidence"
        ),
    }

    for relative, expected in result["source"]["files"].items():
        actual = sha256(ROOT / relative)
        assert actual == expected, f"{relative}: expected {expected}, found {actual}"

    print(
        "PER-03 result verified: Mapy byte gate pass, latency claim rejected; "
        "Regex byte/time gates pass"
    )


if __name__ == "__main__":
    main()
