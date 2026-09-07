#!/usr/bin/env python3
"""Merge the black box cells' results with the required-cell matrix.

Every scenario test writes one JSON record per cell into
`$PETRI_FABRO_COVERAGE_DIR` (default `target/fabro-coverage/results`). This
script reads those records and `crates/fabro/acceptance/scenarios/matrix.json`
and writes one machine-readable report, `coverage.json`, plus a short table on
stdout.

Every cell of the matrix appears in the report with one state:

    passed     a planned cell whose test reported a pass
    external   a planned cell verified by a test in another suite
    failed     a planned cell whose test reported a failure
    skipped    a planned cell whose test skipped (no Docker, no bundle)
    missing    a planned cell no test reported at all
    blocked    a required cell that cannot run yet, with its reason
    excluded   a cell that is not required, with its reason

A filtered or silently skipped case is therefore visible: it is `missing` or
`skipped`, never absent. The exit code is 1 when any planned cell is not
`passed`, unless `--allow-skipped` is given (a machine with no Docker), and 2
on a usage or input problem. `blocked` and `excluded` never fail the report:
they are the recorded, reviewable gaps.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MATRIX = ROOT / "crates" / "fabro" / "acceptance" / "scenarios" / "matrix.json"
DEFAULT_RESULTS = ROOT / "target" / "fabro-coverage" / "results"
SCHEMA_VERSION = 1


def load_matrix(path: Path) -> dict:
    matrix = json.loads(path.read_text(encoding="utf-8"))
    if matrix.get("schema_version") != SCHEMA_VERSION:
        raise SystemExit(f"error: {path} is not schema version {SCHEMA_VERSION}")
    return matrix


def load_results(directory: Path) -> dict[str, dict]:
    results: dict[str, dict] = {}
    if not directory.is_dir():
        return results
    for path in sorted(directory.glob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            raise SystemExit(f"error: {path} is not JSON: {error}") from error
        cell = record.get("cell")
        if not isinstance(cell, str):
            raise SystemExit(f"error: {path} has no cell name")
        earlier = results.get(cell)
        # A rerun of one cell wins over an earlier record of the same cell.
        if earlier is None or record.get("recorded_at", 0) >= earlier.get("recorded_at", 0):
            results[cell] = record
    return results


def state_of(cell: dict, result: dict | None) -> tuple[str, str | None]:
    status = cell.get("status")
    if status == "blocked":
        return "blocked", cell.get("reason")
    if status == "excluded":
        return "excluded", cell.get("reason")
    if result is None:
        test = cell.get("test") or ""
        # A cell whose test lives in another suite (`crate::suite::test`)
        # reports no record of its own; the report names it so the reader
        # knows where it runs.
        if "::" in test:
            return "external", f"verified by {test}"
        return "missing", "no test reported this cell"
    reported = result.get("status")
    if reported == "passed":
        return "passed", None
    if reported == "skipped":
        return "skipped", result.get("note")
    return "failed", result.get("note")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, default=None, help="the results directory")
    parser.add_argument("--out", type=Path, default=None, help="where to write coverage.json")
    parser.add_argument(
        "--allow-skipped",
        action="store_true",
        help="a skipped planned cell does not fail the report (a machine with no Docker)",
    )
    args = parser.parse_args()

    results_dir = args.results or Path(
        os.environ.get("PETRI_FABRO_COVERAGE_DIR", DEFAULT_RESULTS)
    )
    out = args.out or results_dir.parent / "coverage.json"

    matrix = load_matrix(MATRIX)
    results = load_results(results_dir)

    cells = []
    counts: dict[str, int] = {}
    for cell in matrix["cells"]:
        name = cell["cell"]
        state, note = state_of(cell, results.get(name))
        counts[state] = counts.get(state, 0) + 1
        cells.append(
            {
                "cell": name,
                "scenario": cell.get("scenario"),
                "backend": cell.get("backend"),
                "agent": cell.get("agent"),
                "required": cell.get("required", True),
                "declared": cell.get("status"),
                "state": state,
                "note": note,
                "test": cell.get("test"),
            }
        )

    unreported = sorted(set(results) - {cell["cell"] for cell in matrix["cells"]})
    required = [cell for cell in cells if cell["required"]]
    ok_states = {"passed", "blocked", "excluded", "external"}
    if args.allow_skipped:
        ok_states.add("skipped")
    failing = [cell for cell in cells if cell["state"] not in ok_states]

    report = {
        "schema_version": SCHEMA_VERSION,
        "matrix": str(MATRIX.relative_to(ROOT)),
        "results": str(results_dir),
        "counts": counts,
        "required_total": len(required),
        "required_passed": len([c for c in required if c["state"] == "passed"]),
        "unreported_cells": unreported,
        "ok": not failing and not unreported,
        "cells": cells,
    }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    print(f"black box coverage: {out}")
    for state in ("passed", "external", "failed", "skipped", "missing", "blocked", "excluded"):
        if counts.get(state):
            print(f"  {state:9} {counts[state]}")
    for cell in cells:
        if cell["state"] in ("failed", "missing", "skipped"):
            print(f"  {cell['state']}: {cell['cell']} ({cell['note']})")
    for cell in cells:
        if cell["state"] == "blocked":
            print(f"  blocked: {cell['cell']} ({cell['note']})")
    for name in unreported:
        print(f"  unreported: {name} is not in the matrix")

    if unreported:
        return 2
    return 1 if failing else 0


if __name__ == "__main__":
    sys.exit(main())
