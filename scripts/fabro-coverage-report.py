#!/usr/bin/env python3
"""Write the Fabro black box coverage report for one evidence run.

    scripts/fabro-coverage-report.py --evidence DIR [--matrix FILE] [--cells DIR]
                                     [--decisions DIR] [--junit FILE]
                                     [--backends host,docker] [--out DIR] [--strict]
                                     [--readiness]

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
  --decisions DIR      crates/fabro/acceptance/decisions: the decision records.
                       A record's `known_defects` names the independent
                       assertions the pinned Fabro is known to fail in the
                       scenarios the record covers. A `fabro` engine record's
                       failed assertion listed there is expected: the cell
                       passes with a note naming the record. A `petri` record
                       never gets that allowance, and a failed assertion no
                       record lists still fails the cell. Default: the
                       repository's decisions directory.
  --junit FILE         Nextest's JUnit output for the run. A test the runner
                       reports as failed or skipped never counts as passed,
                       whatever its record says.
  --backends LIST      the backends this runner is responsible for (default
                       all). A required cell on another backend is reported as
                       excluded here: the Docker subset is Linux's job.

Outputs (in --out, default DIR): coverage.json and coverage.md.

Each required cell gets exactly one result:
  passed   every source that reported the cell says passed (a pinned-Fabro
           assertion a decision record lists as a known defect counts as
           passed, and the note names the record)
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
does not fail the routine run. With --readiness the exit status is 1 whenever
`ok` is false: the final readiness gate, which a blocked required cell fails
(`mise run check:fabro:readiness`).
"""

from __future__ import annotations

import argparse
import datetime as dt
import fnmatch
import json
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 and older
    tomllib = None

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


# One decision record's known defects: (record id, scenario patterns, assertion names).
KnownDefects = list[tuple[str, list[str], list[str]]]


def load_known_defects(directory: Path | None) -> KnownDefects:
    """The `known_defects` of every decision record: the independent assertions
    the pinned Fabro is known to fail in the scenarios the record covers. A
    record that does not parse ends the report: a malformed decision must not
    silently excuse nothing or everything."""
    found: KnownDefects = []
    if directory is None or not directory.is_dir():
        return found
    if tomllib is None:
        sys.exit("fabro-coverage-report.py: reading decision records needs Python 3.11 or newer (tomllib)")
    for path in sorted(directory.glob("*.toml")):
        try:
            record = tomllib.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            sys.exit(f"{path}: not a decision record: {error}")
        names = record.get("known_defects", [])
        if not isinstance(names, list) or not all(isinstance(n, str) and n.strip() for n in names):
            sys.exit(f"{path}: `known_defects` must be a list of assertion names")
        if not names:
            continue
        scenarios = record.get("scenarios", ["*"])
        if not isinstance(scenarios, list) or not scenarios:
            sys.exit(f"{path}: `scenarios` must be a non-empty list")
        found.append((str(record.get("id", path.stem)), [str(s) for s in scenarios], names))
    return found


def known_defect(defects: KnownDefects, scenario: str, assertion: str) -> str | None:
    """The id of the decision record that lists `assertion` as a known defect
    of the pinned Fabro in `scenario`, if one does."""
    for decision, patterns, names in defects:
        if assertion in names and any(fnmatch.fnmatchcase(scenario, pattern) for pattern in patterns):
            return decision
    return None


def load_junit(path: Path) -> dict[str, str]:
    """Map `package::binary::test`, `binary::test`, and bare test names to
    passed, failed, or skipped."""
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
        if classname:
            results[f"{classname}::{name}"] = result
        results[f"{binary}::{name}"] = result
        results.setdefault(name, result)
    return results


def combine(sources: list[tuple[str, str, str]]) -> tuple[str, str]:
    """Fold (source, result, detail) triples into one cell result."""
    if not sources:
        return "missing", "nothing reported this cell"
    results = {result for _, result, _ in sources}
    details = "; ".join(f"{source}: {detail}" for source, result, detail in sources if detail)
    if "failed" in results:
        return "failed", details or "a source reports a failure"
    if "blocked" in results:
        return "blocked", details or "blocked"
    if "skipped" in results:
        return "skipped", details or "skipped"
    if results == {"passed"}:
        # A passed source may still carry a note (a known defect applied).
        return "passed", details
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


def engine_result(record: dict, defects: KnownDefects) -> tuple[str, str]:
    """One engine record's result. A `fabro` record's failed assertion that a
    decision record lists as a known defect of the pinned Fabro in this
    scenario is expected and does not fail the cell; the note names the
    record. Every other failed assertion, on either engine, fails."""
    engine = str(record.get("engine"))
    scenario = str(record.get("scenario"))
    failed: list[str] = []
    expected: list[str] = []
    for assertion in record.get("assertions", []):
        if assertion.get("passed", False):
            continue
        name = str(assertion.get("name"))
        decision = known_defect(defects, scenario, name) if engine == "fabro" else None
        if decision is None:
            failed.append(name)
        else:
            expected.append(f"`{name}` is the known defect {decision}")
    if failed:
        return "failed", f"{engine} failed {failed}"
    if expected:
        return "passed", "; ".join(expected)
    return "passed", ""


def applied_known_defects(engines: list[dict], defects: KnownDefects) -> list[dict]:
    """Every failed pinned-Fabro assertion a decision record excused, for the report."""
    applied = []
    for record in engines:
        if str(record.get("engine")) != "fabro":
            continue
        scenario = str(record.get("scenario"))
        for assertion in record.get("assertions", []):
            if assertion.get("passed", False):
                continue
            name = str(assertion.get("name"))
            decision = known_defect(defects, scenario, name)
            if decision is not None:
                applied.append({"scenario": scenario, "assertion": name, "decision": decision})
    return applied


def cell_key(scenario: str, backend: str, agent: str | None) -> str:
    return f"{scenario}@{backend}/{agent or 'any'}"


def build_entries(
    matrix: dict | None,
    records: list[dict],
    engines: list[dict],
    cells: dict[str, dict],
    junit: dict[str, str],
    defects: KnownDefects,
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
        result, detail = engine_result(record, defects)
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
    parser.add_argument(
        "--decisions",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "crates" / "fabro" / "acceptance" / "decisions",
        help="the decision records directory (known defects of the pinned Fabro)",
    )
    parser.add_argument("--junit", type=Path)
    parser.add_argument("--backends", help="comma-separated backends required on this runner")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--strict", action="store_true", help="fail on a failed, skipped, or missing required cell (routine CI)")
    parser.add_argument(
        "--readiness",
        action="store_true",
        help="fail unless every required cell passed, blocked cells included (the final readiness gate)",
    )
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
    defects = load_known_defects(args.decisions)
    known = applied_known_defects(engines, defects)
    entries = build_entries(matrix, records, engines, cells, junit, defects)
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
        "decisions": str(args.decisions) if defects else None,
        "junit": str(args.junit) if junit else None,
        "records": len(records),
        "engine_records": len(engines),
        "cell_results": len(cells),
        "totals": totals,
        "ok": ok,
        "ci_ok": ci_ok,
        "backends": args.backends,
        "mixed_pins": mixed_pins,
        "known_defects": known,
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
    if known:
        lines += ["", "Known defects of the pinned Fabro applied (recorded, never accepted as Petri behaviour):", ""]
        lines += [f"- `{k['scenario']}`: `{k['assertion']}` ({k['decision']})" for k in known]
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
    if not ok and not args.readiness:
        print("coverage: the readiness gate is not passed", file=sys.stderr)
    if args.readiness and not ok:
        if not reported:
            reason = "no evidence records were written"
        elif blocked:
            reason = f"{blocked} required cell(s) are blocked"
        else:
            reason = "a required cell did not pass"
        print(f"coverage: the final readiness gate fails ({reason})", file=sys.stderr)
        return 1
    if args.strict and not ci_ok:
        reason = "no evidence records were written" if not reported else "a required cell failed, skipped, or is missing"
        print(f"coverage: the run is not clean ({reason})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
