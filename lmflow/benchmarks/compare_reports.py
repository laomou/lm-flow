#!/usr/bin/env python3
"""Compare two LMFlow benchmark JSON reports.

Usage:
    python lmflow/benchmarks/compare_reports.py baseline.json candidate.json
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load_report(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("results"), list):
        raise ValueError(f"{path}: expected an object with a results array")
    return value


def compare(baseline: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    before = {item["name"]: item for item in baseline["results"]}
    after = {item["name"]: item for item in candidate["results"]}
    rows: list[dict[str, Any]] = []
    for name in sorted(set(before) | set(after)):
        old = before.get(name)
        new = after.get(name)
        row: dict[str, Any] = {"name": name}
        if old is None:
            row["status"] = "added"
        elif new is None:
            row["status"] = "removed"
        else:
            row["status"] = "changed"
            for field in ("packets_per_second", "nanoseconds_per_packet", "mib_per_second"):
                if field not in old or field not in new:
                    continue
                old_value = float(old[field])
                new_value = float(new[field])
                row[f"{field}_baseline"] = old_value
                row[f"{field}_candidate"] = new_value
                row[f"{field}_delta_percent"] = (
                    (new_value - old_value) * 100.0 / old_value if old_value else None
                )
        rows.append(row)
    return {
        "baseline_language": baseline.get("language"),
        "candidate_language": candidate.get("language"),
        "results": rows,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()
    print(json.dumps(compare(load_report(args.baseline), load_report(args.candidate)),
                     indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
