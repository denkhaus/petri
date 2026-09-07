#!/usr/bin/env python3
"""Write the Fabro black box coverage report for one evidence run.

    scripts/fabro-coverage-report.py --evidence DIR [--manifest FILE] [--junit FILE]
                                     [--out DIR] [--strict]

Inputs:

  DIR/records/*.json   the evidence records the harness wrote
                       (schema: crates/petri/cli/tests/support/fabro/evidence.rs)
  --manifest FILE      the scenario manifest: which scenarios are required,
                       blocked, or excluded, on which backends, and which tests
                       prove them. Without it every recorded scenario counts as
                       required.
  --junit FILE         Nextest's JUnit output for the run. A test the runner
                       reports as failed or skipped never counts as passed,
                       whatever its record says.

Outputs (in --out, default DIR): coverage.json and coverage.md.

Each required scenario-and-backend cell gets exactly one result:
  passed   a record with outcome "passed" and no runner disagreement
  failed   a record or the runner says the test failed
  skipped  the record says the scenario skipped (an absent asset or backend)
  missing  no record for the cell
  blocked  the manifest says the scenario is required but blocked
  excluded the manifest excludes the scenario
Only "passed" counts as a pass. With --strict the exit status is 1 unless
every required cell passed and at least one record exists: an empty run is
not a passing gate.

Manifest shape (schema_version 1; the coverage task owns the final format):
  {"schema_version": 1,
   "scenarios": [{"id": "code-review", "family": "review",
                  "status": "required" | "blocked" | "excluded",
                  "backends": ["host", "docker"],
                  "tests": ["fabro_blackbox::contract_..."],
                  "reason": "why it is blocked or excluded"}]}
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

RESULT_ORDER = ["passed", "failed", "skipped", "missing", "blocked", "excluded"]


def load_records(evidence: Path) -> list[dict]:
    records = []
    directory = evidence / "records"
    if not directory.is_dir():
        return records
    for path in sorted(directory.glob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            print(f"warning: {path}: unreadable record: {error}", file=sys.stderr)
            continue
        record["_path"] = str(path)
        records.append(record)
    return records


def load_junit(path: Path) -> dict[str, str]:
    """Map `binary::test` to passed, failed, or skipped from Nextest's JUnit."""
    results: dict[str, str] = {}
    tree = ET.parse(path)
    for case in tree.iter("testcase"):
        classname = case.get("classname", "")
        binary = classname.split("::")[-1] if classname else ""
        name = case.get("name", "")
        key = f"{binary}::{name}"
        if case.find("failure") is not None or case.find("error") is not None:
            results[key] = "failed"
        elif case.find("skipped") is not None:
            results[key] = "skipped"
        else:
            results[key] = "passed"
    return results


def cell_result(status: str, cell_records: list[dict], junit: dict[str, str]) -> tuple[str, str]:
    if status == "excluded":
        return "excluded", "excluded by the manifest"
    if status == "blocked":
        return "blocked", "required but blocked"
    if not cell_records:
        return "missing", "no evidence record"
    outcomes = {r.get("outcome") for r in cell_records}
    tests = {r.get("scenario", {}).get("test") for r in cell_records}
    runner = {junit[t] for t in tests if t in junit}
    if "failed" in outcomes or "failed" in runner:
        return "failed", "a record or the test runner reports a failure"
    if "blocked" in outcomes:
        return "blocked", "the record says the scenario is blocked"
    if "skipped" in outcomes or "skipped" in runner:
        reasons = sorted({r.get("skip_reason") or "" for r in cell_records} - {""})
        return "skipped", "; ".join(reasons) or "skipped"
    if outcomes == {"passed"}:
        return "passed", ""
    return "failed", f"unrecognized outcomes {sorted(str(o) for o in outcomes)}"


def build_entries(manifest: dict | None, records: list[dict], junit: dict[str, str]) -> list[dict]:
    by_cell: dict[tuple[str, str], list[dict]] = {}
    for record in records:
        scenario = record.get("scenario", {})
        key = (str(scenario.get("id")), str(scenario.get("backend")))
        by_cell.setdefault(key, []).append(record)

    entries = []
    if manifest is None:
        for (scenario_id, backend), cell_records in sorted(by_cell.items()):
            result, detail = cell_result("required", cell_records, junit)
            entries.append(
                {
                    "scenario": scenario_id,
                    "backend": backend,
                    "status": "required",
                    "result": result,
                    "detail": detail,
                    "records": [r.get("record_id") for r in cell_records],
                    "tests": sorted({r.get("scenario", {}).get("test") or "" for r in cell_records} - {""}),
                }
            )
        return entries

    listed: set[tuple[str, str]] = set()
    for scenario in manifest.get("scenarios", []):
        scenario_id = str(scenario.get("id"))
        status = scenario.get("status", "required")
        backends = scenario.get("backends") or ["host"]
        for backend in backends:
            cell_records = by_cell.get((scenario_id, backend), [])
            listed.add((scenario_id, backend))
            result, detail = cell_result(status, cell_records, junit)
            if status != "required" and scenario.get("reason"):
                detail = scenario["reason"]
            entries.append(
                {
                    "scenario": scenario_id,
                    "backend": backend,
                    "status": status,
                    "result": result,
                    "detail": detail,
                    "records": [r.get("record_id") for r in cell_records],
                    "tests": scenario.get("tests", []),
                }
            )
    for (scenario_id, backend), cell_records in sorted(by_cell.items()):
        if (scenario_id, backend) in listed:
            continue
        result, detail = cell_result("required", cell_records, junit)
        entries.append(
            {
                "scenario": scenario_id,
                "backend": backend,
                "status": "unlisted",
                "result": result,
                "detail": f"recorded but not in the manifest; {detail}".rstrip("; "),
                "records": [r.get("record_id") for r in cell_records],
                "tests": sorted({r.get("scenario", {}).get("test") or "" for r in cell_records} - {""}),
            }
        )
    return entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--junit", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()

    evidence: Path = args.evidence
    out: Path = args.out or evidence
    out.mkdir(parents=True, exist_ok=True)
    manifest = None
    if args.manifest:
        manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    junit = load_junit(args.junit) if args.junit and args.junit.is_file() else {}
    records = load_records(evidence)
    entries = build_entries(manifest, records, junit)

    totals = {name: 0 for name in RESULT_ORDER}
    totals["required"] = 0
    for entry in entries:
        if entry["status"] == "required":
            totals["required"] += 1
        totals[entry["result"]] += 1
    ok = bool(entries) and all(e["result"] == "passed" for e in entries if e["status"] == "required")

    pins = {}
    for record in records:
        for name, value in (record.get("pins") or {}).items():
            pins.setdefault(name, set()).add(json.dumps(value, sort_keys=True))
    mixed_pins = {name: sorted(values) for name, values in pins.items() if len(values) > 1}
    if mixed_pins:
        ok = False

    report = {
        "schema_version": 1,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "evidence_dir": str(evidence),
        "manifest": str(args.manifest) if args.manifest else None,
        "junit": str(args.junit) if junit else None,
        "records": len(records),
        "totals": totals,
        "ok": ok,
        "mixed_pins": mixed_pins,
        "entries": entries,
    }
    (out / "coverage.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    lines = [
        "# Fabro black box coverage",
        "",
        f"Evidence: `{evidence}`; records: {len(records)}; manifest: "
        f"{'`' + str(args.manifest) + '`' if args.manifest else 'none (every recorded scenario counts as required)'}; "
        f"runner results: {'JUnit' if junit else 'none'}.",
        "",
        "| Required | Passed | Failed | Skipped | Missing | Blocked | Excluded |",
        "| --- | --- | --- | --- | --- | --- | --- |",
        f"| {totals['required']} | {totals['passed']} | {totals['failed']} | {totals['skipped']} | "
        f"{totals['missing']} | {totals['blocked']} | {totals['excluded']} |",
        "",
        "Only `passed` counts. Skipped, missing, blocked, and excluded cells never do.",
        "",
        "| Scenario | Backend | Status | Result | Detail |",
        "| --- | --- | --- | --- | --- |",
    ]
    for entry in entries:
        lines.append(
            f"| {entry['scenario']} | {entry['backend']} | {entry['status']} | {entry['result']} | "
            f"{entry['detail'].replace('|', '/')} |"
        )
    if mixed_pins:
        lines += ["", "Records cite more than one revision for: " + ", ".join(sorted(mixed_pins)) + "."]
    lines += ["", f"Gate: {'passed' if ok else 'NOT passed'}."]
    (out / "coverage.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    print(
        f"coverage: required {totals['required']}, passed {totals['passed']}, failed {totals['failed']}, "
        f"skipped {totals['skipped']}, missing {totals['missing']}, blocked {totals['blocked']}, "
        f"excluded {totals['excluded']} -> {out / 'coverage.md'}"
    )
    if mixed_pins:
        print(f"coverage: records disagree on pins {sorted(mixed_pins)}", file=sys.stderr)
    if args.strict and not ok:
        reason = "no evidence records were written" if not entries else "a required cell is not passed"
        print(f"coverage: the gate is not passed ({reason})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
