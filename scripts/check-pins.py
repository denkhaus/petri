#!/usr/bin/env python3
"""Check that every citation of a pinned library revision agrees.

    scripts/check-pins.py [--evidence DIR]

Sources compared:

  Cargo.toml                          pebble-coding-agent, pebble-agent, lithos-llm,
                                      the four sandbox-driver entries
  crates/petri/cli/Cargo.toml         twin-openai, twin-anthropic
  crates/fabro/corpus-pin.txt         the Fabro reference commit
  crates/fabro/acceptance/bundles.lock.json   fabro_reference.commit
  crates/fabro/acceptance/CONTRACT.md the "Pinned revisions" table
  DIR/records/*.json                  the `pins` block of every evidence record
                                      (default: target/fabro-evidence/latest when present)

Exit 1 with every disagreement listed; exit 0 when all agree.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REV = re.compile(r'^(?P<name>[A-Za-z0-9_-]+)\s*=\s*\{[^}]*\bgit\s*=\s*"(?P<git>[^"]+)"[^}]*\brev\s*=\s*"(?P<rev>[0-9a-f]+)"', re.M)


def manifest_revisions(path: Path) -> dict[str, tuple[str, str]]:
    return {m["name"]: (m["git"], m["rev"]) for m in REV.finditer(path.read_text(encoding="utf-8"))}


def same(problems: list[str], label: str, values: dict[str, str]) -> str | None:
    distinct = sorted(set(values.values()))
    if len(distinct) == 1:
        return distinct[0]
    problems.append(f"{label}: revisions disagree: " + ", ".join(f"{k}={v}" for k, v in sorted(values.items())))
    return None


def contract_table(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    start = text.find("## Pinned revisions")
    if start < 0:
        return {}
    section = text[start:]
    end = section.find("\n## ", 1)
    section = section if end < 0 else section[:end]
    table: dict[str, str] = {}
    for line in section.splitlines():
        m = re.match(r"^\|\s*`?([a-z0-9_-]+)`?\s*\|\s*`([0-9a-f]{7,40})`", line)
        if m:
            table[m.group(1)] = m.group(2)
    return table


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    problems: list[str] = []

    workspace = manifest_revisions(ROOT / "Cargo.toml")
    cli = manifest_revisions(ROOT / "crates/petri/cli/Cargo.toml")
    expected: dict[str, str] = {}

    pebble = same(problems, "pebble", {n: workspace[n][1] for n in ("pebble-coding-agent", "pebble-agent") if n in workspace})
    if pebble:
        expected["pebble"] = pebble
    if "lithos-llm" in workspace:
        expected["lithos_llm"] = workspace["lithos-llm"][1]
    else:
        problems.append("Cargo.toml: lithos-llm pin not found")
    sandbox = same(problems, "sandbox-driver", {n: r for n, (_, r) in workspace.items() if n.startswith("sandbox-driver")})
    if sandbox:
        expected["sandbox_driver"] = sandbox
    twins = same(problems, "twins", {n: r for n, (_, r) in cli.items() if n.startswith("twin-")})
    if twins:
        expected["twins"] = twins
    for name in ("pebble", "lithos_llm", "sandbox_driver", "twins"):
        if name not in expected and not any(name in p for p in problems):
            problems.append(f"the {name} pin was not found in the manifests")

    pin_file = ROOT / "crates/fabro/corpus-pin.txt"
    fabro = next((line.split()[0] for line in pin_file.read_text().splitlines() if line.strip() and not line.startswith("#")), None)
    if fabro:
        expected["fabro_reference"] = fabro
    else:
        problems.append(f"{pin_file}: no pin")
    lock = json.loads((ROOT / "crates/fabro/acceptance/bundles.lock.json").read_text(encoding="utf-8"))
    if lock.get("fabro_reference", {}).get("commit") != fabro:
        problems.append(f"bundles.lock.json fabro_reference.commit {lock.get('fabro_reference', {}).get('commit')} != pin {fabro}")

    contract = contract_table(ROOT / "crates/fabro/acceptance/CONTRACT.md")
    if not contract:
        problems.append("CONTRACT.md: no `## Pinned revisions` table")
    for name, rev in expected.items():
        cited = contract.get(name)
        if cited is None:
            problems.append(f"CONTRACT.md: no row for {name}")
        elif not rev.startswith(cited):
            problems.append(f"CONTRACT.md: {name} is {cited}, the manifest pins {rev}")

    evidence = args.evidence or (ROOT / "target/fabro-evidence/latest")
    records = sorted(evidence.glob("records/*.json")) if evidence.is_dir() else []
    for path in records:
        try:
            pins = json.loads(path.read_text(encoding="utf-8")).get("pins") or {}
        except ValueError as error:
            problems.append(f"{path}: not JSON: {error}")
            continue
        for name, rev in expected.items():
            cited = pins.get(name)
            if isinstance(cited, dict):
                cited = cited.get("commit")
            if cited is None:
                problems.append(f"{path.name}: no {name} pin")
            elif cited != rev:
                problems.append(f"{path.name}: {name} cites {cited}, the manifest pins {rev}")

    for name, rev in sorted(expected.items()):
        print(f"{name:16} {rev}")
    print(f"evidence records checked: {len(records)}" + (f" ({evidence})" if records else ""))
    if problems:
        print("pin check failed:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print("pins agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
