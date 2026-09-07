#!/usr/bin/env python3
"""Write the Fabro black box coverage report for one evidence run.

    scripts/fabro-coverage-report.py --evidence DIR [--matrix FILE] [--cells DIR]
                                     [--junit FILE] [--backends host,docker]
                                     [--out DIR] [--strict]

Inputs:

  DIR/records/*.json   the per-scenario gate records the harness wrote
                       (crates/petri/cli/tests/support/fabro/record.rs)
  DIR/<scenario>/<engine>.json
                       the differential matrix's per-engine records
                       (crates/petri/cli/tests/support/fabro/evidence.rs):
                       `assertions[].passed` per engine
  --matrix FILE        crates/fabro/acceptance/scenarios/matrix.json: every
                       required (scenario, backend, agent) cell with its
                       `status` (`planned`, `blocked`, `excluded`), `reason`,
                       and `test`. Without it every recorded scenario counts as
                       required.
  --cells DIR          the per-cell results the scenario tests write
                       (`$PETRI_FABRO_COVERAGE_DIR`, `CellRecord` in
                       tests/support/fabro/scenario.rs): `{cell, status, note}`
  --junit FILE         Nextest's JUnit output for the run. A test the runner
                       reports as failed or skipped never counts as passed,
                       whatever its record says.
  --backends LIST      the backends this runner is responsible for (default
                       all). A required cell on another backend is reported as
                       excluded here: the Docker subset is Linux's job.

Outputs (in --out, default DIR): coverage.json and coverage.md.

Each required cell gets exactly one result:
  passed   every source that reported the cell says passed
  failed   a record, a cell result, an engine record, or the runner says failed
  skipped  a record or cell result says the scenario skipped (an absent asset
           or backend)
  missing  the cell is required but nothing reported it
  blocked  the matrix says the cell is required but blocked
  excluded the matrix excludes the cell
Only "passed" counts as a pass. The report's `ok` is the readiness gate:
every required cell passed, blocked cells included. With --strict the exit
status is 1 when a required cell failed, skipped, or is missing, or when no
result exists at all (an empty run is not a passing gate); a cell the matrix
declares blocked with a reason is visible in the report and in `ok`, but
does not fail the routine run.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

RESULT_ORDER = ["passed", "failed", "skipped", "missing", "blocked", "excluded"]


def read_json(path: Path) -> dict | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"warning: {path}: unreadable: {error}", file=sys.stderr)
        return None
    return value if isinstance(value, dict) else None


def load_records(evidence: Path) -> list[dict]:
    """The per-scenario gate records under records/."""
    records = []
    directory = evidence / "records"
    if directory.is_dir():
        for path in sorted(directory.glob("*.json")):
            record = read_json(path)
            if record is not None:
                record["_path"] = str(path)
                records.append(record)
    return records


def load_engine_records(evidence: Path) -> list[dict]:
    """The differential matrix's per-engine records: <scenario>/<engine>.json."""
    found = []
    if not evidence.is_dir():
        return found
    for path in sorted(evidence.rglob("*.json")):
        relative = path.relative_to(evidence)
        if relative.parts[0] in ("records", "bundles", "cells") or path.name in ("coverage.json",):
            continue
        record = read_json(path)
        if record is None or "engine" not in record or "scenario" not in record:
            continue
        record["_path"] = str(path)
        found.append(record)
    return found


def load_cells(directory: Path | None) -> dict[str, dict]:
    """The scenario tests' per-cell results, by cell id."""
    cells: dict[str, dict] = {}
    if directory and directory.is_dir():
        for path in sorted(directory.glob("*.json")):
            record = read_json(path)
            if record is not None and "cell" in record:
                cells[str(record["cell"])] = record
    return cells


def load_junit(path: Path) -> dict[str, str]:
    """Map `binary::test` and bare test names to passed, failed, or skipped."""
    results: dict[str, str] = {}
    tree = ET.parse(path)
    for case in tree.iter("testcase"):
        classname = case.get("classname", "")
        binary = classname.split("::")[-1] if classname else ""
        name = case.get("name", "")
        if case.find("failure") is not None or case.find("error") is not None:
            result = "failed"
        elif case.find("skipped") is not None:
            result = "skipped"
        else:
            result = "passed"
        results[f"{binary}::{name}"] = result
        results.setdefault(name, result)
    return results


def combine(sources: list[tuple[str, str, str]]) -> tuple[str, str]:
    """Fold (source, result, detail) triples into one cell result."""
    if not sources:
        return "missing", "nothing reported this cell"
    results = {result for _, result, _ in sources}
    details = "; ".join(f"{source}: {detail}" for source, result, detail in sources if detail and result != "passed")
    if "failed" in results:
        return "failed", details or "a source reports a failure"
    if "blocked" in results:
        return "blocked", details or "blocked"
    if "skipped" in results:
        return "skipped", details or "skipped"
    if results == {"passed"}:
        return "passed", ""
    return "failed", f"unrecognized results {sorted(results)}"


def record_result(record: dict) -> tuple[str, str]:
    outcome = str(record.get("outcome"))
    if outcome == "skipped":
        return "skipped", str(record.get("skip_reason") or "skipped")
    if outcome == "blocked":
        return "blocked", str(record.get("block_reason") or "blocked")
    if outcome == "passed":
        return "passed", ""
    failed = [a.get("name") for a in record.get("assertions", []) if a.get("outcome") == "failed"]
    return "failed", f"failed assertions {failed}" if failed else outcome


def engine_result(record: dict) -> tuple[str, str]:
    failed = [a.get("name") for a in record.get("assertions", []) if not a.get("passed", False)]
    if failed:
        return "failed", f"{record.get('engine')} failed {failed}"
    return "passed", ""


def cell_key(scenario: str, backend: str, agent: str | None) -> str:
    return f"{scenario}@{backend}/{agent or 'any'}"


def build_entries(
    matrix: dict | None,
    records: list[dict],
    engines: list[dict],
    cells: dict[str, dict],
    junit: dict[str, str],
) -> list[dict]:
    # Sources by (scenario, backend) for records, and by scenario for engines.
    by_scenario_backend: dict[tuple[str, str], list[tuple[str, str, str]]] = {}
    tests_by_scenario_backend: dict[tuple[str, str], set[str]] = {}
    for record in records:
        meta = record.get("scenario", {})
        key = (str(meta.get("id")), str(meta.get("backend")))
        result, detail = record_result(record)
        by_scenario_backend.setdefault(key, []).append((f"record {record.get('record_id')}", result, detail))
        if meta.get("test"):
            tests_by_scenario_backend.setdefault(key, set()).add(str(meta["test"]))
    by_scenario: dict[str, list[tuple[str, str, str]]] = {}
    for record in engines:
        result, detail = engine_result(record)
        by_scenario.setdefault(str(record["scenario"]), []).append((f"engine {record.get('engine')}", result, detail))

    def sources_for(scenario: str, backend: str, cell: str | None, tests: list[str]) -> tuple[list[tuple[str, str, str]], list[str]]:
        sources = list(by_scenario_backend.get((scenario, backend), []))
        names = set(tests) | tests_by_scenario_backend.get((scenario, backend), set())
        if cell is not None and cell in cells:
            result = cells[cell]
            sources.append((f"cell {cell}", str(result.get("status")), str(result.get("note") or "")))
        if backend == "host":
            # The differential matrix runs on the host backend.
            sources.extend(by_scenario.get(scenario, []))
        for name in sorted(names):
            if name in junit:
                sources.append((f"runner {name}", junit[name], "" if junit[name] == "passed" else f"the runner reports {junit[name]}"))
        return sources, sorted(names)

    entries: list[dict] = []
    seen: set[tuple[str, str]] = set()
    if matrix is not None:
        for cell in matrix.get("cells", []):
            scenario = cell.get("scenario")
            backend = str(cell.get("backend"))
            agent = cell.get("agent")
            status = str(cell.get("status", "planned"))
            cell_id = str(cell.get("cell") or cell_key(str(scenario), backend, agent))
            tests = [cell["test"]] if cell.get("test") else []
            if status == "excluded":
                result, detail = "excluded", str(cell.get("reason") or "excluded by the matrix")
            elif status == "blocked":
                result, detail = "blocked", str(cell.get("reason") or "required but blocked")
            else:
                sources, tests = sources_for(str(scenario), backend, cell_id, tests)
                result, detail = combine(sources)
            if scenario is not None:
                seen.add((str(scenario), backend))
            entries.append(
                {
                    "cell": cell_id,
                    "scenario": scenario,
                    "backend": backend,
                    "agent": agent,
                    "status": "required" if cell.get("required", True) and status not in ("excluded",) else status,
                    "matrix_status": status,
                    "result": result,
                    "detail": detail,
                    "tests": tests,
                }
            )
    # Scenarios that reported but are not in the matrix (or there is none).
    extra = {key for key in by_scenario_backend} | {(s, "host") for s in by_scenario}
    for scenario, backend in sorted(extra - seen):
        sources, tests = sources_for(scenario, backend, None, [])
        result, detail = combine(sources)
        entries.append(
            {
                "cell": cell_key(scenario, backend, None),
                "scenario": scenario,
                "backend": backend,
                "agent": None,
                "status": "required" if matrix is None else "unlisted",
                "matrix_status": None,
                "result": result,
                "detail": detail if matrix is None else f"recorded but not in the matrix; {detail}".rstrip("; "),
                "tests": tests,
            }
        )
    return entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--matrix", type=Path, help="the scenario matrix (matrix.json)")
    parser.add_argument("--manifest", type=Path, help=argparse.SUPPRESS)  # older name for --matrix
    parser.add_argument("--cells", type=Path, help="the per-cell results directory")
    parser.add_argument("--junit", type=Path)
    parser.add_argument("--backends", help="comma-separated backends required on this runner")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()

    evidence: Path = args.evidence
    out: Path = args.out or evidence
    out.mkdir(parents=True, exist_ok=True)
    matrix_path = args.matrix or args.manifest
    matrix = json.loads(matrix_path.read_text(encoding="utf-8")) if matrix_path else None
    cells_dir = args.cells or (evidence / "cells")
    junit = load_junit(args.junit) if args.junit and args.junit.is_file() else {}
    records = load_records(evidence)
    engines = load_engine_records(evidence)
    cells = load_cells(cells_dir)
    entries = build_entries(matrix, records, engines, cells, junit)
    if args.backends:
        here = {b.strip() for b in args.backends.split(",") if b.strip()}
        for entry in entries:
            if entry["backend"] not in here and entry["status"] == "required":
                entry["status"] = "excluded"
                entry["result"] = "excluded"
                entry["detail"] = f"backend `{entry['backend']}` is not required on this runner"

    totals = {name: 0 for name in RESULT_ORDER}
    totals["required"] = 0
    for entry in entries:
        if entry["status"] == "required":
            totals["required"] += 1
        totals[entry["result"]] += 1
    reported = any(e["result"] not in ("missing", "blocked", "excluded") for e in entries)
    required = [e for e in entries if e["status"] == "required"]
    ok = reported and all(e["result"] == "passed" for e in required)
    ci_ok = reported and not any(e["result"] in ("failed", "skipped", "missing") for e in required)

    pins: dict[str, set[str]] = {}
    for record in records + engines:
        for name, value in (record.get("pins") or {}).items():
            if name == "petri" and isinstance(value, dict):
                value = value.get("commit")
            if isinstance(value, dict) or value is None:
                continue
            pins.setdefault(name, set()).add(json.dumps(value, sort_keys=True))
    mixed_pins = {name: sorted(values) for name, values in pins.items() if len(values) > 1}
    if mixed_pins:
        ok = False
        ci_ok = False

    report = {
        "schema_version": 1,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "evidence_dir": str(evidence),
        "matrix": str(matrix_path) if matrix_path else None,
        "cells_dir": str(cells_dir) if cells else None,
        "junit": str(args.junit) if junit else None,
        "records": len(records),
        "engine_records": len(engines),
        "cell_results": len(cells),
        "totals": totals,
        "ok": ok,
        "ci_ok": ci_ok,
        "backends": args.backends,
        "mixed_pins": mixed_pins,
        "entries": entries,
    }
    (out / "coverage.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    lines = [
        "# Fabro black box coverage",
        "",
        f"Evidence: `{evidence}`; scenario records: {len(records)}; engine records: {len(engines)}; "
        f"cell results: {len(cells)}; matrix: "
        f"{'`' + str(matrix_path) + '`' if matrix_path else 'none (every recorded scenario counts as required)'}; "
        f"runner results: {'JUnit' if junit else 'none'}.",
        "",
        "| Required | Passed | Failed | Skipped | Missing | Blocked | Excluded |",
        "| --- | --- | --- | --- | --- | --- | --- |",
        f"| {totals['required']} | {totals['passed']} | {totals['failed']} | {totals['skipped']} | "
        f"{totals['missing']} | {totals['blocked']} | {totals['excluded']} |",
        "",
        "Only `passed` counts. Skipped, missing, blocked, and excluded cells never do.",
        "",
        "| Cell | Status | Result | Detail |",
        "| --- | --- | --- | --- |",
    ]
    for entry in entries:
        lines.append(
            f"| {entry['cell']} | {entry['status']} | {entry['result']} | {entry['detail'].replace('|', '/')} |"
        )
    if mixed_pins:
        lines += ["", "Records cite more than one revision for: " + ", ".join(sorted(mixed_pins)) + "."]
    blocked = sum(1 for e in required if e["result"] == "blocked")
    if ok:
        lines += ["", "Gate: passed."]
    elif ci_ok:
        lines += ["", f"Gate: NOT passed ({blocked} required cell(s) blocked with a recorded reason); the routine run is clean."]
    else:
        lines += ["", "Gate: NOT passed."]
    (out / "coverage.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    print(
        f"coverage: required {totals['required']}, passed {totals['passed']}, failed {totals['failed']}, "
        f"skipped {totals['skipped']}, missing {totals['missing']}, blocked {totals['blocked']}, "
        f"excluded {totals['excluded']} -> {out / 'coverage.md'}"
    )
    if mixed_pins:
        print(f"coverage: records disagree on pins {sorted(mixed_pins)}", file=sys.stderr)
    if not ok:
        print("coverage: the readiness gate is not passed", file=sys.stderr)
    if args.strict and not ci_ok:
        reason = "no evidence records were written" if not reported else "a required cell failed, skipped, or is missing"
        print(f"coverage: the run is not clean ({reason})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
