#!/usr/bin/env python3
"""Render human and machine reports from a completed canonical scan bundle.

The canonical inputs are:

- scan-manifest.json
- candidate-ledger.jsonl
- findings.json
- coverage.json
- panel-votes.jsonl

This program validates their relationships, then writes the Markdown report,
the HTML report, and the findings JSONL to the result directory. It writes
revision.json to the metadata directory. It never asks a model to interpret or
rewrite canonical scan decisions.

Usage: render_report.py <evidence-dir> <result-dir> <metadata-dir>
Python 3.9-compatible. Standard library only.
"""

from __future__ import annotations

import contextlib
import hashlib
import html
import json
import os
import re
import sys
import tempfile
from collections.abc import Mapping
from pathlib import Path, PurePosixPath
from typing import Any, Dict, Iterable, List, NoReturn, Optional, Sequence, Tuple


JsonMap = Mapping[str, object]
Finding = Dict[str, object]

SCHEMA_VERSION = 1
CANDIDATE_CAP = 400
VERIFICATION_CAP = 45
CANONICAL_FILES = (
    "scan-manifest.json",
    "candidate-ledger.jsonl",
    "findings.json",
    "coverage.json",
    "panel-votes.jsonl",
)
CANDIDATE_FIELDS = (
    "findingId",
    "occurrenceId",
    "fingerprints",
    "ruleId",
    "identity",
    "title",
    "impact",
    "file",
    "line",
    "description",
    "evidence",
    "exploit_scenarios",
    "preconditions",
    "category",
    "severity",
    "difficulty",
    "confidence",
    "recommendations",
    "cwe_id",
    "snippet",
    "symbol",
    "duplicate_of",
    "duplicate_reasoning",
)
# `code` is a presentation excerpt read from the reviewed tree at bundle time.
# It belongs to a reportable finding only; the ledger keeps the judged claim.
REPORT_FIELDS = ("id", *CANDIDATE_FIELDS, "code")
MANIFEST_FIELDS = (
    "schemaVersion",
    "kind",
    "scanId",
    "target",
    "startedAt",
    "completedAt",
    "workflow",
    "request",
    "revision",
    "completion",
    "canonicalFiles",
)
LEDGER_FIELDS = (
    "schemaVersion",
    "rank",
    "disposition",
    "dispositionReason",
    "displayId",
    "selectedForPanel",
    "withinCandidateBudget",
    "reports",
    "reporters",
    "candidate",
)
VOTE_FIELDS = (
    "schemaVersion",
    "voteId",
    "findingId",
    "occurrenceId",
    "candidateRank",
    "round",
    "lens",
    "claim",
    "status",
    "verdict",
    "reasoning",
)
CLAIM_FIELDS = (
    "file",
    "line",
    "category",
    "severityAsReported",
    "title",
    "rationale",
    "evidenceAsCited",
    "snippetAsQuoted",
    "symbol",
    "reports",
)
COVERAGE_FIELDS = (
    "droppedComponents",
    "skippedComponents",
    "components",
    "effort",
    "focus",
    "diffFiles",
    "diffLines",
    "diffSizeRejected",
    "scopeFiles",
    "scopeSizeRejected",
    "collapsed",
    "completenessCheckOutcome",
    "topLevelCount",
    "topLevelRejected",
    "unaccountedTopLevelDirs",
    "inventoryRejected",
    "inventoryFallback",
    "emptyDiff",
    "emptyScope",
    "mode",
    "scope",
    "range",
    "researchersPerCell",
    "researchersDispatched",
    "researchersReturned",
    "prunedBuckets",
    "adversarialCasualties",
    "candidatesDroppedByCap",
    "unverifiedByCap",
    "invalidResearchResults",
    "rejectedFindingReports",
)

SEVERITIES = ("HIGH", "MEDIUM", "LOW")
DIFFICULTIES = ("LOW", "MEDIUM", "HIGH")
CONFIDENCES = ("low", "medium", "high")
CONFIDENCE_ORDER = {name: rank for rank, name in enumerate(CONFIDENCES, 1)}
DISPOSITIONS = (
    "reportable",
    "rejected",
    "deferred",
    "verification-incomplete",
)
VOTE_ROUNDS = ("panel", "repanel", "redteam")
VOTE_STATUSES = ("completed", "missing")
VERDICTS = ("TRUE_POSITIVE", "FALSE_POSITIVE")

REVISION_NAME = "revision.json"
TEMPLATE_RELATIVE_PATH = ("..", "templates", "report.html")
REPORT_DATA_MARKER = "__REPORT_DATA__"
HTML_REPORT_NAME = "SECURITY-REVIEW-RESULTS.html"
HTML_REPORT_TITLE = "Security review results"
MONTH_NAMES = (
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
)
MODE_LABELS = {
    "scan": "Whole repository",
    "changes": "Branch changes",
    "commit": "One commit",
}
# The vulnerability taxonomy has one definition: schemas/taxonomy.json. Slugs
# are part of `ruleId`, and therefore of a finding's stable identity, so the
# report maps them to display names instead of renaming them upstream.
TAXONOMY_PATH = (
    Path(__file__).resolve().parent.parent / "schemas" / "taxonomy.json"
)
SEVERITY_DEFINITIONS = (
    (
        "High",
        "Can cause broad compromise, bypass a core security control, or "
        "create serious harm across users or tenants.",
    ),
    (
        "Medium",
        "Can cause material harm, but practical limits reduce the affected "
        "scope, access, or attacker options.",
    ),
    (
        "Low",
        "Has limited direct impact or weakens defense in depth without "
        "creating a complete attack path by itself.",
    ),
)
DIFFICULTY_DEFINITIONS = (
    (
        "Low",
        "Uses a common technique, public tooling, or a short script. It "
        "requires little specialized access or knowledge.",
    ),
    (
        "Medium",
        "Requires a custom exploit, product knowledge, favorable timing, or "
        "access that is not available to every user.",
    ),
    (
        "High",
        "Requires privileged access, detailed internal knowledge, a complex "
        "exploit chain, or narrow operating conditions.",
    ),
)
HEX_RE = re.compile(r"^[0-9a-fA-F]{7,64}\Z")
DISPLAY_ID_RE = re.compile(r"^F[1-9][0-9]{0,8}\Z")
STABLE_FINDING_ID_RE = re.compile(r"^csf_[0-9a-f]{24}\Z")
OCCURRENCE_ID_RE = re.compile(r"^occ_[0-9a-f]{24}\Z")
FINGERPRINT_RE = re.compile(r"^codex-security/v1:sha256:[0-9a-f]{64}\Z")
TARGET_ID_RE = re.compile(r"^security-review-target/v1:sha256:[0-9a-f]{64}\Z")
SCAN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}\Z")
RULE_ID_RE = re.compile(
    r"^[a-z0-9]+(?:-[a-z0-9]+)*\.[a-z0-9]+(?:-[a-z0-9]+)*\Z"
)
IDENTITY_SLUG_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*\Z")
MAX_IDENTITY_FIELD_LENGTH = 160
FINGERPRINT_PREFIX = "codex-security/v1:sha256:"
SEPARATOR_ESCAPES = {0x85: "\\u0085", 0x2028: "\\u2028", 0x2029: "\\u2029"}
UNSAFE_DISPLAY_CODEPOINTS = {
    0x061C,
    0x200E,
    0x200F,
    0x2028,
    0x2029,
    *range(0x202A, 0x202F),
    *range(0x2066, 0x206A),
}


class RenderError(Exception):
    """A canonical bundle refusal."""


def die(message: str) -> NoReturn:
    sys.stderr.write(f"render_report.py: {message}\n")
    raise SystemExit(1)


def load_taxonomy() -> Tuple[Dict[str, str], Tuple[Tuple[str, str], ...]]:
    """Read the one taxonomy definition, or fail before anything is rendered."""
    try:
        with open(TAXONOMY_PATH, encoding="utf-8") as handle:
            payload = json.load(handle)
    except (OSError, ValueError) as error:
        die(f"taxonomy is unreadable: {error}")
    slugs = payload.get("slugs")
    categories = payload.get("categories")
    if not isinstance(slugs, dict) or not slugs:
        die("taxonomy slugs is not a non-empty object")
    if not isinstance(categories, list) or not categories:
        die("taxonomy categories is not a non-empty array")
    definitions = tuple(
        (str(entry["name"]), str(entry["description"]))
        for entry in categories
        if isinstance(entry, dict) and "name" in entry and "description" in entry
    )
    if len(definitions) != len(categories):
        die("a taxonomy category is missing name or description")
    names = {name for name, _ in definitions}
    unknown = sorted({str(value) for value in slugs.values()} - names)
    if unknown:
        die(f"taxonomy slugs map to undefined categories: {', '.join(unknown)}")
    return {str(key): str(value) for key, value in slugs.items()}, definitions


CATEGORY_DISPLAY, CATEGORY_DEFINITIONS = load_taxonomy()


def as_map(value: object) -> Optional[JsonMap]:
    return value if isinstance(value, dict) else None


def exact_keys(value: JsonMap, expected: Iterable[str], label: str) -> None:
    expected_set = set(expected)
    actual_set = set(value)
    if actual_set != expected_set:
        missing = sorted(expected_set - actual_set)
        extra = sorted(actual_set - expected_set)
        detail = []
        if missing:
            detail.append("missing " + ", ".join(missing))
        if extra:
            detail.append("unexpected " + ", ".join(extra))
        raise RenderError(f"{label} fields are invalid: {'; '.join(detail)}")


def safe_text(value: object, field: str, allow_empty: bool = True) -> str:
    if not isinstance(value, str):
        raise RenderError(f"{field} must be a string")
    if not allow_empty and not value:
        raise RenderError(f"{field} must not be empty")
    for character in value:
        codepoint = ord(character)
        if (
            (codepoint < 0x20 and character not in "\n\t")
            or 0x7F <= codepoint <= 0x9F
            or codepoint in UNSAFE_DISPLAY_CODEPOINTS
        ):
            raise RenderError(f"{field} contains a control character")
    return value


def non_negative_int(value: object, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise RenderError(f"{field} must be a non-negative integer")
    return value


def positive_int(value: object, field: str) -> int:
    result = non_negative_int(value, field)
    if result < 1:
        raise RenderError(f"{field} must be at least 1")
    return result


def read_json(directory: str, name: str) -> object:
    path = os.path.join(directory, name)
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except OSError as error:
        raise RenderError(f"{name} is missing or unreadable: {error}") from error
    except ValueError as error:
        raise RenderError(f"{name} is not valid JSON: {error}") from error


def read_jsonl(directory: str, name: str) -> List[JsonMap]:
    path = os.path.join(directory, name)
    records: List[JsonMap] = []
    try:
        with open(path, encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, 1):
                if not line.strip():
                    raise RenderError(
                        f"{name}:{line_number} is an empty JSONL record"
                    )
                try:
                    value = json.loads(line)
                except ValueError as error:
                    raise RenderError(
                        f"{name}:{line_number} is not valid JSON: {error}"
                    ) from error
                record = as_map(value)
                if record is None:
                    raise RenderError(
                        f"{name}:{line_number} is not a JSON object"
                    )
                records.append(record)
    except OSError as error:
        raise RenderError(f"{name} is missing or unreadable: {error}") from error
    return records


def sha256_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def validate_manifest(value: object) -> JsonMap:
    manifest = as_map(value)
    if manifest is None:
        raise RenderError("scan-manifest.json must be a JSON object")
    exact_keys(manifest, MANIFEST_FIELDS, "scan-manifest.json")
    if manifest.get("schemaVersion") != SCHEMA_VERSION:
        raise RenderError("scan-manifest.json has an unsupported schemaVersion")
    if manifest.get("kind") != "security-review.completed-scan":
        raise RenderError("scan-manifest.json has an invalid kind")
    scan_id = manifest.get("scanId")
    if not isinstance(scan_id, str) or not SCAN_ID_RE.fullmatch(scan_id):
        raise RenderError("scan-manifest.json has an invalid scanId")
    target = as_map(manifest.get("target"))
    if target is None:
        raise RenderError("scan-manifest.json target must be an object")
    exact_keys(target, ("id", "idSource", "scanRoot"), "manifest target")
    target_id = target.get("id")
    if not isinstance(target_id, str) or not TARGET_ID_RE.fullmatch(target_id):
        raise RenderError("scan-manifest.json target.id is invalid")
    if target.get("idSource") not in ("git-origin", "git-root", "local-path"):
        raise RenderError("scan-manifest.json target.idSource is invalid")
    safe_text(target.get("scanRoot"), "manifest target.scanRoot", False)
    safe_text(manifest.get("startedAt"), "manifest startedAt", False)
    safe_text(manifest.get("completedAt"), "manifest completedAt", False)
    request = as_map(manifest.get("request"))
    revision = as_map(manifest.get("revision"))
    workflow = as_map(manifest.get("workflow"))
    completion = as_map(manifest.get("completion"))
    if (
        request is None
        or revision is None
        or workflow is None
        or completion is None
    ):
        raise RenderError(
            "scan-manifest.json workflow, request, revision, and "
            "completion must be objects"
        )
    exact_keys(
        request,
        ("mode", "scope", "range", "base", "commit", "effort", "focus"),
        "manifest request",
    )
    exact_keys(workflow, ("name", "stateVersion"), "manifest workflow")
    exact_keys(
        completion,
        (
            "status",
            "reasons",
            "verificationStatus",
            "rawCandidateReports",
            "uniqueCandidates",
            "dispositions",
            "findings",
            "panelVoteRecords",
            "completedVoteRecords",
            "missingVoteRecords",
        ),
        "manifest completion",
    )
    if workflow.get("name") != "security-review":
        raise RenderError("scan-manifest.json workflow.name is invalid")
    positive_int(workflow.get("stateVersion"), "manifest workflow.stateVersion")
    if request.get("mode") not in ("scan", "changes", "commit"):
        raise RenderError("scan-manifest.json request.mode is invalid")
    if request.get("effort") not in ("low", "medium", "high", "max"):
        raise RenderError("scan-manifest.json request.effort is invalid")
    string_list(request.get("scope"), "manifest request.scope")
    for optional_field in ("range", "base", "commit", "focus"):
        optional_value = request.get(optional_field)
        if optional_value is not None:
            safe_text(
                optional_value,
                f"manifest request.{optional_field}",
                False,
            )
    if completion.get("status") not in ("complete", "partial"):
        raise RenderError("scan-manifest.json completion.status is invalid")
    if completion.get("verificationStatus") not in ("verified", "unverified"):
        raise RenderError(
            "scan-manifest.json completion.verificationStatus is invalid"
        )
    reasons = string_list(
        completion.get("reasons"),
        "manifest completion.reasons",
    )
    if completion.get("status") == "complete" and reasons:
        raise RenderError("a complete scan manifest cannot give partial reasons")
    if completion.get("status") == "partial" and not reasons:
        raise RenderError("a partial scan manifest must give a reason")
    for field in (
        "rawCandidateReports",
        "uniqueCandidates",
        "findings",
        "panelVoteRecords",
        "completedVoteRecords",
        "missingVoteRecords",
    ):
        non_negative_int(
            completion.get(field),
            f"manifest completion.{field}",
        )
    dispositions = as_map(completion.get("dispositions"))
    if dispositions is None:
        raise RenderError("manifest completion.dispositions must be an object")
    allowed_dispositions = set(DISPOSITIONS)
    if set(dispositions) - allowed_dispositions:
        raise RenderError("manifest completion.dispositions is invalid")
    for name, count in dispositions.items():
        non_negative_int(count, f"manifest completion.dispositions.{name}")
    canonical_files = manifest.get("canonicalFiles")
    if canonical_files != list(CANONICAL_FILES):
        raise RenderError(
            "scan-manifest.json canonicalFiles does not name the canonical bundle"
        )
    return manifest


def normalize_repo_path(value: object, field: str) -> str:
    path = safe_text(value, field, False)
    if "\\" in path or path.startswith("/"):
        raise RenderError(f"{field} must be a repository-relative POSIX path")
    parsed = PurePosixPath(path)
    if path in (".", "") or ".." in parsed.parts:
        raise RenderError(f"{field} escapes or does not name a repository file")
    return path


def expected_identity(
    item: JsonMap,
    target_id: str,
    scan_id: str,
    label: str,
) -> Dict[str, object]:
    rule_id = item.get("ruleId")
    if (
        not isinstance(rule_id, str)
        or len(rule_id) > MAX_IDENTITY_FIELD_LENGTH
        or not RULE_ID_RE.fullmatch(rule_id)
    ):
        raise RenderError(f"{label} has an invalid ruleId")
    raw_identity = as_map(item.get("identity"))
    if raw_identity is None or set(raw_identity) - {"anchor", "instance"}:
        raise RenderError(f"{label} has an invalid identity object")
    anchor = raw_identity.get("anchor")
    if (
        not isinstance(anchor, str)
        or len(anchor) > MAX_IDENTITY_FIELD_LENGTH
        or not IDENTITY_SLUG_RE.fullmatch(anchor)
    ):
        raise RenderError(f"{label} has an invalid identity.anchor")
    instance = raw_identity.get("instance")
    if instance is not None and (
        not isinstance(instance, str)
        or len(instance) > MAX_IDENTITY_FIELD_LENGTH
        or not IDENTITY_SLUG_RE.fullmatch(instance)
    ):
        raise RenderError(f"{label} has an invalid identity.instance")
    identity: Dict[str, str] = {"anchor": anchor}
    if isinstance(instance, str):
        identity["instance"] = instance
    material = "\0".join(
        [
            "codex-security/v1",
            target_id,
            rule_id,
            anchor,
            instance or "",
        ]
    )
    fingerprint = FINGERPRINT_PREFIX + sha256_text(material)
    return {
        "findingId": "csf_" + sha256_text(fingerprint)[:24],
        "occurrenceId": "occ_"
        + sha256_text(scan_id + "\0" + fingerprint)[:24],
        "fingerprints": {"primary": fingerprint},
        "ruleId": rule_id,
        "identity": identity,
    }


def string_list(value: object, field: str) -> List[str]:
    if not isinstance(value, list):
        raise RenderError(f"{field} must be an array")
    return [
        safe_text(item, f"{field}[{index}]")
        for index, item in enumerate(value)
    ]


def canonical_evidence(value: object, field: str) -> List[str]:
    """The source-to-sink proof, one citation per hop.

    A bundle written before the list shape carries one blob; it becomes a
    one-entry list so both render. That fallback goes when the shape is retired.
    """
    if isinstance(value, str):
        text = safe_text(value, field)
        return [text] if text.strip() else []
    return string_list(value, field)


def non_empty_string_list(value: object, field: str) -> List[str]:
    items = string_list(value, field)
    if not items or any(not item.strip() for item in items):
        raise RenderError(f"{field} must list at least one non-empty entry")
    return items


def canonical_code(value: object, line: int, label: str) -> Dict[str, object]:
    code = as_map(value)
    if code is None:
        raise RenderError(f"{label} must be an object")
    exact_keys(code, ("language", "label", "lines"), label)
    raw_lines = code.get("lines")
    if not isinstance(raw_lines, list):
        raise RenderError(f"{label}.lines must be an array")
    lines: List[Dict[str, object]] = []
    previous: Optional[int] = None
    highlighted = 0
    for index, raw_line in enumerate(raw_lines):
        item = as_map(raw_line)
        if item is None:
            raise RenderError(f"{label}.lines[{index}] must be an object")
        if set(item) - {"number", "text", "highlight"} or "number" not in item:
            raise RenderError(f"{label}.lines[{index}] fields are invalid")
        number = positive_int(item.get("number"), f"{label}.lines[{index}].number")
        if previous is not None and number != previous + 1:
            raise RenderError(f"{label}.lines must be consecutive")
        previous = number
        entry: Dict[str, object] = {
            "number": number,
            "text": safe_text(item.get("text"), f"{label}.lines[{index}].text"),
        }
        highlight = item.get("highlight")
        if highlight is not None:
            if not isinstance(highlight, bool):
                raise RenderError(
                    f"{label}.lines[{index}].highlight must be boolean"
                )
            if highlight:
                highlighted += 1
                if number != line:
                    raise RenderError(
                        f"{label} highlights a line other than the finding line"
                    )
                entry["highlight"] = True
        lines.append(entry)
    if lines and highlighted != 1:
        raise RenderError(f"{label} must highlight the finding line exactly once")
    if lines and not lines[0]["number"] <= line <= lines[-1]["number"]:
        raise RenderError(f"{label} does not cover the finding line")
    return {
        "language": safe_text(code.get("language"), f"{label}.language", False),
        "label": safe_text(code.get("label"), f"{label}.label", False),
        "lines": lines,
    }


def duplicate_reference(value: object, label: str) -> Optional[str]:
    """Validate a finding's optional `duplicate_of` back-reference.

    It is null for an ordinary finding, or the stable `findingId` of the
    primary this finding duplicates. Cross-finding integrity — the primary
    exists, is not this finding, and is not itself a duplicate — is checked
    later in `validate_relationships`, once every finding has been read.
    """
    if value is None:
        return None
    text = safe_text(value, f"{label}.duplicate_of", allow_empty=False)
    if not STABLE_FINDING_ID_RE.fullmatch(text):
        raise RenderError(f"{label}.duplicate_of is not a stable finding id")
    return text


def canonical_finding(
    value: object,
    target_id: str,
    scan_id: str,
    label: str,
    require_display_id: bool,
) -> Finding:
    item = as_map(value)
    if item is None:
        raise RenderError(f"{label} must be a JSON object")
    exact_keys(
        item,
        REPORT_FIELDS if require_display_id else CANDIDATE_FIELDS,
        label,
    )
    display_id: Optional[str] = None
    if require_display_id:
        candidate_id = item.get("id")
        if not isinstance(candidate_id, str) or not DISPLAY_ID_RE.fullmatch(
            candidate_id
        ):
            raise RenderError(f"{label} has an invalid display id")
        display_id = candidate_id

    expected = expected_identity(item, target_id, scan_id, label)
    for field, expected_value in expected.items():
        if item.get(field) != expected_value:
            raise RenderError(f"{label} has an invalid derived {field}")
    if not STABLE_FINDING_ID_RE.fullmatch(str(item.get("findingId") or "")):
        raise RenderError(f"{label} has an invalid findingId")
    if not OCCURRENCE_ID_RE.fullmatch(str(item.get("occurrenceId") or "")):
        raise RenderError(f"{label} has an invalid occurrenceId")
    fingerprints = as_map(item.get("fingerprints"))
    if (
        fingerprints is None
        or set(fingerprints) != {"primary"}
        or not FINGERPRINT_RE.fullmatch(str(fingerprints.get("primary") or ""))
    ):
        raise RenderError(f"{label} has invalid fingerprints")

    severity = item.get("severity")
    difficulty = item.get("difficulty")
    confidence = item.get("confidence")
    if severity not in SEVERITIES:
        raise RenderError(f"{label} severity is invalid")
    if difficulty not in DIFFICULTIES:
        raise RenderError(f"{label} difficulty is invalid")
    if confidence not in CONFIDENCES:
        raise RenderError(f"{label} confidence is invalid")
    line = positive_int(item.get("line"), f"{label}.line")
    cwe = item.get("cwe_id")
    if cwe is not None:
        cwe = safe_text(cwe, f"{label}.cwe_id")
        if cwe and not re.fullmatch(r"CWE-[0-9]{1,5}", cwe):
            raise RenderError(f"{label}.cwe_id is invalid")

    finding: Finding = {
        **expected,
        "title": safe_text(item.get("title"), f"{label}.title", False),
        "impact": safe_text(item.get("impact"), f"{label}.impact"),
        "file": normalize_repo_path(item.get("file"), f"{label}.file"),
        "line": line,
        "description": safe_text(
            item.get("description"),
            f"{label}.description",
            False,
        ),
        "evidence": canonical_evidence(item.get("evidence"), f"{label}.evidence"),
        "exploit_scenarios": non_empty_string_list(
            item.get("exploit_scenarios"),
            f"{label}.exploit_scenarios",
        ),
        "preconditions": string_list(
            item.get("preconditions"),
            f"{label}.preconditions",
        ),
        "category": safe_text(
            item.get("category"),
            f"{label}.category",
            False,
        ),
        "severity": severity,
        "difficulty": difficulty,
        "confidence": confidence,
        "recommendations": string_list(
            item.get("recommendations"),
            f"{label}.recommendations",
        ),
        "cwe_id": cwe or None,
        "snippet": safe_text(item.get("snippet"), f"{label}.snippet"),
        "symbol": safe_text(item.get("symbol"), f"{label}.symbol"),
        "duplicate_of": duplicate_reference(
            item.get("duplicate_of"),
            label,
        ),
        "duplicate_reasoning": safe_text(
            item.get("duplicate_reasoning"),
            f"{label}.duplicate_reasoning",
        ),
    }
    if display_id is not None:
        finding = {
            "id": display_id,
            **finding,
            "code": canonical_code(item.get("code"), line, f"{label}.code"),
        }
        return {field: finding[field] for field in REPORT_FIELDS}
    return {field: finding[field] for field in CANDIDATE_FIELDS}


def validate_findings(
    raw: object,
    target_id: str,
    scan_id: str,
) -> List[Finding]:
    if not isinstance(raw, list):
        raise RenderError("findings.json must be a JSON array")
    findings = [
        canonical_finding(
            value,
            target_id,
            scan_id,
            f"findings.json item {index}",
            True,
        )
        for index, value in enumerate(raw)
    ]
    for index, finding in enumerate(findings, 1):
        if finding["id"] != f"F{index}":
            raise RenderError("findings.json display IDs must match report order")
    for field in ("id", "findingId", "occurrenceId"):
        values = [finding[field] for finding in findings]
        if len(values) != len(set(values)):
            raise RenderError(f"findings.json contains duplicate {field} values")
    return findings


def validate_ledger(
    raw: Sequence[JsonMap],
    target_id: str,
    scan_id: str,
) -> List[Dict[str, object]]:
    ledger: List[Dict[str, object]] = []
    stable_ids = set()
    for index, item in enumerate(raw, 1):
        label = f"candidate-ledger.jsonl record {index}"
        exact_keys(item, LEDGER_FIELDS, label)
        if item.get("schemaVersion") != SCHEMA_VERSION:
            raise RenderError(f"{label} has an invalid schemaVersion")
        if item.get("rank") != index:
            raise RenderError(
                "candidate-ledger.jsonl ranks must be sequential and ordered"
            )
        disposition = item.get("disposition")
        if disposition not in DISPOSITIONS:
            raise RenderError(f"{label} has an invalid disposition")
        reason = safe_text(
            item.get("dispositionReason"),
            f"{label}.dispositionReason",
            False,
        )
        allowed_reasons = {
            "reportable": {"verified-panel-quorum"},
            "rejected": {
                "panel-rejected",
                "repanel-rejected",
                "redteam-refuted",
            },
            "deferred": {"candidate-budget", "verification-budget"},
            "verification-incomplete": {
                "panel-record-missing",
                "panel-incomplete",
            },
        }
        if reason not in allowed_reasons[str(disposition)]:
            raise RenderError(f"{label} dispositionReason is inconsistent")
        display_id = item.get("displayId")
        if disposition == "reportable":
            if not isinstance(display_id, str) or not DISPLAY_ID_RE.fullmatch(
                display_id
            ):
                raise RenderError(f"{label} reportable entry needs displayId")
        elif display_id is not None:
            raise RenderError(f"{label} non-reportable entry has displayId")
        for boolean_field in ("selectedForPanel", "withinCandidateBudget"):
            if not isinstance(item.get(boolean_field), bool):
                raise RenderError(f"{label}.{boolean_field} must be boolean")
        selected = index <= VERIFICATION_CAP
        within_candidate_budget = index <= CANDIDATE_CAP
        if item.get("selectedForPanel") is not selected:
            raise RenderError(f"{label}.selectedForPanel is inconsistent")
        if item.get("withinCandidateBudget") is not within_candidate_budget:
            raise RenderError(f"{label}.withinCandidateBudget is inconsistent")
        if disposition == "deferred":
            expected_reason = (
                "candidate-budget"
                if not within_candidate_budget
                else "verification-budget"
            )
            if selected or reason != expected_reason:
                raise RenderError(f"{label} deferred budget is inconsistent")
        elif not selected:
            raise RenderError(f"{label} must be deferred outside the panel cap")
        reports = positive_int(item.get("reports"), f"{label}.reports")
        reporters = string_list(item.get("reporters"), f"{label}.reporters")
        candidate = canonical_finding(
            item.get("candidate"),
            target_id,
            scan_id,
            f"{label}.candidate",
            False,
        )
        stable_id = candidate["findingId"]
        if stable_id in stable_ids:
            raise RenderError("candidate-ledger.jsonl repeats a findingId")
        stable_ids.add(stable_id)
        ledger.append(
            {
                "schemaVersion": SCHEMA_VERSION,
                "rank": index,
                "disposition": disposition,
                "dispositionReason": reason,
                "displayId": display_id,
                "selectedForPanel": item["selectedForPanel"],
                "withinCandidateBudget": item["withinCandidateBudget"],
                "reports": reports,
                "reporters": reporters,
                "candidate": candidate,
            }
        )
    return ledger


def validate_votes(
    raw: Sequence[JsonMap],
    ledger_by_id: Mapping[object, Mapping[str, object]],
) -> List[Dict[str, object]]:
    records: List[Dict[str, object]] = []
    vote_ids = set()
    for index, item in enumerate(raw, 1):
        label = f"panel-votes.jsonl record {index}"
        exact_keys(item, VOTE_FIELDS, label)
        if item.get("schemaVersion") != SCHEMA_VERSION:
            raise RenderError(f"{label} has an invalid schemaVersion")
        vote_id = safe_text(item.get("voteId"), f"{label}.voteId", False)
        if vote_id in vote_ids:
            raise RenderError("panel-votes.jsonl repeats a voteId")
        vote_ids.add(vote_id)
        finding_id = item.get("findingId")
        ledger_entry = ledger_by_id.get(finding_id)
        if ledger_entry is None:
            raise RenderError(f"{label} references an unknown findingId")
        if ledger_entry.get("selectedForPanel") is not True:
            raise RenderError(f"{label} references a deferred candidate")
        candidate = as_map(ledger_entry.get("candidate")) or {}
        if item.get("occurrenceId") != candidate.get("occurrenceId"):
            raise RenderError(f"{label} occurrenceId does not match the ledger")
        rank = positive_int(item.get("candidateRank"), f"{label}.candidateRank")
        if rank != ledger_entry.get("rank"):
            raise RenderError(f"{label} candidateRank does not match the ledger")
        round_name = item.get("round")
        if round_name not in VOTE_ROUNDS:
            raise RenderError(f"{label} has an invalid round")
        lens = safe_text(item.get("lens"), f"{label}.lens", False)
        allowed_lenses = (
            {"RED_TEAM"}
            if round_name == "redteam"
            else {"REACHABILITY", "IMPACT", "DEFENSES"}
        )
        if lens not in allowed_lenses:
            raise RenderError(f"{label} lens is inconsistent with its round")
        expected_vote_id = (
            f"{round_name}:{item['occurrenceId']}:{lens.lower()}"
        )
        if vote_id != expected_vote_id:
            raise RenderError(f"{label} has an invalid derived voteId")
        claim = as_map(item.get("claim"))
        if claim is None:
            raise RenderError(f"{label}.claim must be an object")
        exact_keys(claim, CLAIM_FIELDS, f"{label}.claim")
        expected_claim = {
            "file": candidate["file"],
            "line": candidate["line"],
            "category": candidate["category"],
            "severityAsReported": candidate["severity"],
            "title": candidate["title"],
            "rationale": candidate["description"],
            "evidenceAsCited": "\n".join(candidate["evidence"]) or "(none)",
            "snippetAsQuoted": candidate["snippet"] or "(none)",
            "symbol": candidate["symbol"] or "(none)",
            "reports": ledger_entry["reports"],
        }
        if dict(claim) != expected_claim:
            raise RenderError(
                f"{label}.claim differs from the candidate evidence"
            )
        status = item.get("status")
        if status not in VOTE_STATUSES:
            raise RenderError(f"{label} has an invalid status")
        verdict = item.get("verdict")
        reasoning = safe_text(item.get("reasoning"), f"{label}.reasoning")
        if status == "completed":
            if verdict not in VERDICTS or not reasoning:
                raise RenderError(
                    f"{label} completed vote needs verdict and reasoning"
                )
        elif verdict is not None or reasoning:
            raise RenderError(
                f"{label} missing vote must not claim verdict or reasoning"
            )
        records.append(
            {
                "schemaVersion": SCHEMA_VERSION,
                "voteId": vote_id,
                "findingId": finding_id,
                "occurrenceId": item["occurrenceId"],
                "candidateRank": rank,
                "round": round_name,
                "lens": lens,
                "claim": dict(claim),
                "status": status,
                "verdict": verdict,
                "reasoning": reasoning,
            }
        )
    return records


def disposition_counts(
    ledger: Sequence[Mapping[str, object]],
) -> Dict[str, int]:
    counts: Dict[str, int] = {}
    for entry in ledger:
        disposition = str(entry["disposition"])
        counts[disposition] = counts.get(disposition, 0) + 1
    return counts


def optional_count(value: object, field: str) -> Optional[int]:
    if value is None:
        return None
    return non_negative_int(value, field)


def optional_text(value: object, field: str) -> Optional[str]:
    if value is None:
        return None
    return safe_text(value, field)


def validate_coverage(value: object) -> Dict[str, object]:
    coverage = as_map(value)
    if coverage is None:
        raise RenderError("coverage.json must be a JSON object")
    exact_keys(coverage, COVERAGE_FIELDS, "coverage.json")

    string_fields = (
        "droppedComponents",
        "unaccountedTopLevelDirs",
        "inventoryRejected",
        "prunedBuckets",
        "adversarialCasualties",
        "invalidResearchResults",
        "rejectedFindingReports",
    )
    normalized: Dict[str, object] = {
        field: string_list(coverage.get(field), f"coverage.{field}")
        for field in string_fields
    }

    skipped: List[Dict[str, object]] = []
    raw_skipped = coverage.get("skippedComponents")
    if not isinstance(raw_skipped, list):
        raise RenderError("coverage.skippedComponents must be an array")
    for index, raw_item in enumerate(raw_skipped):
        label = f"coverage.skippedComponents[{index}]"
        item = as_map(raw_item)
        if item is None:
            raise RenderError(f"{label} must be an object")
        exact_keys(item, ("name", "paths", "reason"), label)
        skipped.append(
            {
                "name": safe_text(item.get("name"), f"{label}.name", False),
                "paths": string_list(item.get("paths"), f"{label}.paths"),
                "reason": safe_text(
                    item.get("reason"),
                    f"{label}.reason",
                    False,
                ),
            }
        )
    normalized["skippedComponents"] = skipped

    components: List[Dict[str, object]] = []
    raw_components = coverage.get("components")
    if not isinstance(raw_components, list):
        raise RenderError("coverage.components must be an array")
    for index, raw_item in enumerate(raw_components):
        label = f"coverage.components[{index}]"
        item = as_map(raw_item)
        if item is None:
            raise RenderError(f"{label} must be an object")
        exact_keys(item, ("name", "paths"), label)
        components.append(
            {
                "name": safe_text(item.get("name"), f"{label}.name", False),
                "paths": string_list(item.get("paths"), f"{label}.paths"),
            }
        )
    normalized["components"] = components

    effort = coverage.get("effort")
    if effort not in ("low", "medium", "high", "max"):
        raise RenderError("coverage.effort is invalid")
    mode = coverage.get("mode")
    if mode not in ("scan", "changes", "commit"):
        raise RenderError("coverage.mode is invalid")
    collapsed = coverage.get("collapsed")
    if collapsed not in (None, "small-diff", "small-scope"):
        raise RenderError("coverage.collapsed is invalid")
    completeness = coverage.get("completenessCheckOutcome")
    if completeness not in (
        "checked",
        "partial",
        "not-checkable",
        "not-applicable",
    ):
        raise RenderError("coverage.completenessCheckOutcome is invalid")
    fallback = coverage.get("inventoryFallback")
    if fallback not in (
        None,
        "incomplete-partition",
        "inventory-failed",
        "empty-partition",
    ):
        raise RenderError("coverage.inventoryFallback is invalid")
    for field in ("emptyDiff", "emptyScope"):
        if not isinstance(coverage.get(field), bool):
            raise RenderError(f"coverage.{field} must be boolean")
    raw_scope = coverage.get("scope")
    scope = (
        None
        if raw_scope is None
        else string_list(raw_scope, "coverage.scope")
    )
    normalized.update(
        {
            "effort": effort,
            "focus": safe_text(coverage.get("focus"), "coverage.focus", False),
            "diffFiles": optional_count(
                coverage.get("diffFiles"),
                "coverage.diffFiles",
            ),
            "diffLines": optional_count(
                coverage.get("diffLines"),
                "coverage.diffLines",
            ),
            "diffSizeRejected": optional_text(
                coverage.get("diffSizeRejected"),
                "coverage.diffSizeRejected",
            ),
            "scopeFiles": optional_count(
                coverage.get("scopeFiles"),
                "coverage.scopeFiles",
            ),
            "scopeSizeRejected": optional_text(
                coverage.get("scopeSizeRejected"),
                "coverage.scopeSizeRejected",
            ),
            "collapsed": collapsed,
            "completenessCheckOutcome": completeness,
            "topLevelCount": optional_count(
                coverage.get("topLevelCount"),
                "coverage.topLevelCount",
            ),
            "topLevelRejected": optional_text(
                coverage.get("topLevelRejected"),
                "coverage.topLevelRejected",
            ),
            "inventoryFallback": fallback,
            "emptyDiff": coverage["emptyDiff"],
            "emptyScope": coverage["emptyScope"],
            "mode": mode,
            "scope": scope,
            "range": optional_text(coverage.get("range"), "coverage.range"),
            "researchersPerCell": positive_int(
                coverage.get("researchersPerCell"),
                "coverage.researchersPerCell",
            ),
            "researchersDispatched": non_negative_int(
                coverage.get("researchersDispatched"),
                "coverage.researchersDispatched",
            ),
            "researchersReturned": non_negative_int(
                coverage.get("researchersReturned"),
                "coverage.researchersReturned",
            ),
            "candidatesDroppedByCap": non_negative_int(
                coverage.get("candidatesDroppedByCap"),
                "coverage.candidatesDroppedByCap",
            ),
            "unverifiedByCap": non_negative_int(
                coverage.get("unverifiedByCap"),
                "coverage.unverifiedByCap",
            ),
        }
    )
    return normalized


def validate_relationships(
    manifest: JsonMap,
    findings: Sequence[Finding],
    ledger: Sequence[Mapping[str, object]],
    votes: Sequence[Mapping[str, object]],
    coverage: Mapping[str, object],
) -> None:
    ledger_by_id = {
        (as_map(entry["candidate"]) or {}).get("findingId"): entry
        for entry in ledger
    }
    reportable = [
        entry for entry in ledger if entry["disposition"] == "reportable"
    ]
    if len(reportable) != len(findings):
        raise RenderError(
            "findings.json is not the reportable candidate-ledger subset"
        )
    findings_by_id = {finding["findingId"]: finding for finding in findings}
    for entry in reportable:
        candidate = as_map(entry["candidate"]) or {}
        finding = findings_by_id.get(candidate.get("findingId"))
        if finding is None:
            raise RenderError("a reportable ledger entry is absent from findings")
        if entry.get("displayId") != finding.get("id"):
            raise RenderError("a reportable ledger displayId does not match findings")
        if {
            key: value
            for key, value in finding.items()
            if key not in ("id", "code")
        } != dict(candidate):
            raise RenderError(
                "a reportable ledger candidate differs from findings.json"
            )

    # A finding flagged a duplicate must point at another reported finding that
    # exists, is not itself, and is not also a duplicate — no chains or cycles —
    # and must carry the reasoning the appendix shows for the demotion.
    for finding in findings:
        duplicate_of = finding.get("duplicate_of")
        reasoning = str(finding.get("duplicate_reasoning") or "").strip()
        if duplicate_of is None:
            if reasoning:
                raise RenderError(
                    f"finding {finding['id']} records duplicate reasoning but "
                    "names no primary finding"
                )
            continue
        if duplicate_of == finding["findingId"]:
            raise RenderError(
                f"finding {finding['id']} is marked a duplicate of itself"
            )
        primary = findings_by_id.get(duplicate_of)
        if primary is None:
            raise RenderError(
                f"finding {finding['id']} duplicates an unknown finding"
            )
        if primary.get("duplicate_of") is not None:
            raise RenderError(
                f"finding {finding['id']} duplicates a finding that is itself "
                "a duplicate"
            )
        if not reasoning:
            raise RenderError(
                f"finding {finding['id']} is a duplicate without recorded "
                "reasoning"
            )

    votes_by_finding: Dict[object, List[Mapping[str, object]]] = {}
    vote_coordinates = set()
    for vote in votes:
        votes_by_finding.setdefault(vote["findingId"], []).append(vote)
        coordinate = (vote["findingId"], vote["round"], vote["lens"])
        if coordinate in vote_coordinates:
            raise RenderError(
                "panel-votes.jsonl repeats a finding round and lens"
            )
        vote_coordinates.add(coordinate)
    for entry in ledger:
        candidate = as_map(entry["candidate"]) or {}
        finding_votes = votes_by_finding.get(candidate.get("findingId"), [])
        initial = [vote for vote in finding_votes if vote["round"] == "panel"]
        if entry["selectedForPanel"]:
            if len(initial) != 3 or {
                vote["lens"] for vote in initial
            } != {"REACHABILITY", "IMPACT", "DEFENSES"}:
                raise RenderError(
                    "a panel-selected candidate lacks its three dispatched "
                    "initial vote records"
                )
        elif finding_votes:
            raise RenderError("a deferred candidate has panel vote records")
    for finding in findings:
        panel = [
            vote
            for vote in votes_by_finding.get(finding["findingId"], [])
            if vote["round"] == "panel" and vote["status"] == "completed"
        ]
        true_votes = sum(
            vote["verdict"] == "TRUE_POSITIVE" for vote in panel
        )
        if len(panel) != 3 or true_votes < 2:
            raise RenderError(
                f"finding {finding['id']} lacks a complete keep-quorum panel"
            )
        # Only a unanimous panel earns high confidence. The engine computes
        # this, and it is re-checked here against the votes themselves so a
        # bundle edited after the fact cannot publish a stronger claim.
        ceiling = "high" if true_votes == 3 else "medium"
        if CONFIDENCE_ORDER[str(finding["confidence"])] > CONFIDENCE_ORDER[ceiling]:
            raise RenderError(
                f"finding {finding['id']} claims {finding['confidence']} "
                f"confidence, but {true_votes} of {len(panel)} panel voters "
                f"confirmed it, which earns at most {ceiling}"
            )

    completion = as_map(manifest["completion"]) or {}
    if completion.get("uniqueCandidates") != len(ledger):
        raise RenderError("manifest uniqueCandidates does not match the ledger")
    if completion.get("findings") != len(findings):
        raise RenderError("manifest findings does not match findings.json")
    if completion.get("panelVoteRecords") != len(votes):
        raise RenderError(
            "manifest panelVoteRecords does not match panel-votes.jsonl"
        )
    completed_vote_records = sum(
        vote["status"] == "completed" for vote in votes
    )
    if completion.get("completedVoteRecords") != completed_vote_records:
        raise RenderError(
            "manifest completedVoteRecords does not match panel-votes.jsonl"
        )
    if completion.get("missingVoteRecords") != (
        len(votes) - completed_vote_records
    ):
        raise RenderError(
            "manifest missingVoteRecords does not match panel-votes.jsonl"
        )
    if completion.get("dispositions") != disposition_counts(ledger):
        raise RenderError("manifest dispositions does not match the ledger")
    if completion.get("rawCandidateReports") != sum(
        int(entry["reports"]) for entry in ledger
    ):
        raise RenderError(
            "manifest rawCandidateReports does not match the ledger"
        )
    if len(ledger_by_id) != len(ledger):
        raise RenderError("candidate ledger stable identities are not unique")

    deferred_by_candidate_cap = sum(
        not bool(entry["withinCandidateBudget"]) for entry in ledger
    )
    deferred_by_verification_cap = sum(
        bool(entry["withinCandidateBudget"])
        and not bool(entry["selectedForPanel"])
        for entry in ledger
    )
    if coverage.get("candidatesDroppedByCap") != deferred_by_candidate_cap:
        raise RenderError(
            "coverage candidatesDroppedByCap does not match the ledger"
        )
    if coverage.get("unverifiedByCap") != deferred_by_verification_cap:
        raise RenderError("coverage unverifiedByCap does not match the ledger")
    if coverage.get("mode") != (as_map(manifest["request"]) or {}).get("mode"):
        raise RenderError("coverage mode does not match the manifest")
    if coverage.get("effort") != (
        as_map(manifest["request"]) or {}
    ).get("effort"):
        raise RenderError("coverage effort does not match the manifest")
    if coverage.get("scope") != (
        (as_map(manifest["request"]) or {}).get("scope") or None
    ):
        raise RenderError("coverage scope does not match the manifest")
    partial_inputs = (
        bool(disposition_counts(ledger).get("deferred"))
        or bool(
            disposition_counts(ledger).get("verification-incomplete")
        )
        or bool(coverage.get("invalidResearchResults"))
        or bool(coverage.get("rejectedFindingReports"))
        or any(vote["status"] == "missing" for vote in votes)
        or completion.get("verificationStatus") != "verified"
    )
    expected_completion = "partial" if partial_inputs else "complete"
    if completion.get("status") != expected_completion:
        raise RenderError(
            "manifest completion.status is inconsistent with canonical data"
        )


def escape_markdown(value: object) -> str:
    text = safe_text(value, "Markdown value")
    text = html.escape(text, quote=False)
    text = text.replace("\\", "\\\\")
    for character in ("`", "*", "_", "{", "}", "[", "]", "#", "|"):
        text = text.replace(character, "\\" + character)
    lines = []
    for line in text.split("\n"):
        leading = len(line) - len(line.lstrip(" \t"))
        if leading:
            prefix = "".join(
                "&#9;" if character == "\t" else "&#32;"
                for character in line[:leading]
            )
            line = prefix + line[leading:]
        line = re.sub(r"^([-+])(?=\s|$)", r"\\\1", line)
        line = re.sub(
            r"^([0-9]{1,9})([.)])(?=\s|$)",
            lambda match: match.group(1) + "\\" + match.group(2),
            line,
        )
        if re.fullmatch(r"(?:-{3,}|={3,}|~{3,})\s*", line):
            line = "\\" + line
        lines.append(line)
    # The renderer supplies the <br> tags. Model text was HTML-escaped above.
    # This preserves line breaks without giving a later line a fresh block
    # context in which it could become a list, heading, fence, or HTML block.
    return "<br>\n".join(lines)


def code_span(value: object) -> str:
    text = safe_text(value, "code span").replace("\r", " ").replace("\n", " ")
    longest = max((len(match) for match in re.findall(r"`+", text)), default=0)
    fence = "`" * (longest + 1)
    padding = " " if text.startswith("`") or text.endswith("`") else ""
    return f"{fence}{padding}{text}{padding}{fence}"


def indented_code(value: object) -> str:
    text = safe_text(value, "code block")
    if not text:
        return "    (not supplied)"
    return "\n".join("    " + line for line in text.splitlines())


def coverage_markdown(coverage: JsonMap) -> List[str]:
    lines = ["## Coverage", ""]
    mode = escape_markdown(coverage.get("mode") or "unknown")
    focus = escape_markdown(coverage.get("focus") or "unknown")
    lines.append(f"- Mode: {mode}")
    lines.append(f"- Focus: {focus}")
    scope = coverage.get("scope")
    if isinstance(scope, list) and scope:
        lines.append(
            "- Scope: "
            + ", ".join(code_span(item) for item in scope if isinstance(item, str))
        )
    else:
        lines.append("- Scope: whole target")
    components = coverage.get("components")
    if isinstance(components, list) and components:
        rendered = []
        for component in components:
            item = as_map(component)
            if item is not None:
                rendered.append(escape_markdown(item.get("name") or "unnamed"))
        if rendered:
            lines.append("- Components examined: " + ", ".join(rendered))
    skipped = coverage.get("skippedComponents")
    if isinstance(skipped, list) and skipped:
        lines.append("- Deliberately skipped components:")
        for component in skipped:
            item = as_map(component)
            if item is None:
                continue
            name = escape_markdown(item.get("name") or "unnamed")
            reason = escape_markdown(item.get("reason") or "no reason recorded")
            paths = item.get("paths")
            path_text = ""
            if isinstance(paths, list):
                path_text = ", ".join(
                    code_span(path) for path in paths if isinstance(path, str)
                )
            lines.append(f"  - {name}: {path_text} — {reason}")
    completeness = coverage.get("completenessCheckOutcome")
    if isinstance(completeness, str):
        lines.append(
            "- Completeness check: " + escape_markdown(completeness)
        )
    unaccounted = coverage.get("unaccountedTopLevelDirs")
    if isinstance(unaccounted, list) and unaccounted:
        lines.append(
            "- Unaccounted top-level directories: "
            + ", ".join(
                code_span(item) for item in unaccounted if isinstance(item, str)
            )
        )
    collapsed = coverage.get("collapsed")
    if isinstance(collapsed, str):
        lines.append(
            "- Proportionate execution shape: " + escape_markdown(collapsed)
        )
    deferred_candidate = coverage.get("candidatesDroppedByCap")
    deferred_verification = coverage.get("unverifiedByCap")
    if isinstance(deferred_candidate, int) and deferred_candidate:
        lines.append(
            f"- Deferred by candidate budget: {deferred_candidate}"
        )
    if isinstance(deferred_verification, int) and deferred_verification:
        lines.append(
            f"- Deferred by verification budget: {deferred_verification}"
        )
    invalid = coverage.get("invalidResearchResults")
    if isinstance(invalid, list) and invalid:
        lines.append(f"- Unusable research results: {len(invalid)}")
    rejected = coverage.get("rejectedFindingReports")
    if isinstance(rejected, list) and rejected:
        lines.append(
            f"- Reported findings dropped for failing the contract: "
            f"{len(rejected)}"
        )
        for reason in rejected:
            lines.append(f"  - {escape_markdown(reason)}")
    lines.append("")
    return lines


def finding_detail_markdown(
    finding: Finding,
    votes: Sequence[Mapping[str, object]],
) -> List[str]:
    finding_votes = [
        vote
        for vote in votes
        if vote["findingId"] == finding["findingId"]
        and vote["round"] == "panel"
        and vote["status"] == "completed"
    ]
    true_votes = sum(
        vote["verdict"] == "TRUE_POSITIVE" for vote in finding_votes
    )
    lines = [
        "**Impact.** " + escape_markdown(finding["impact"]),
        "",
        (
            f"**Where.** {code_span(finding['file'])}:{finding['line']}"
            + (
                f" in {code_span(finding['symbol'])}"
                if finding["symbol"]
                else ""
            )
        ),
        "",
        "**What.** " + escape_markdown(finding["description"]),
        "",
        "**Evidence.**",
        "",
    ]
    citations = finding["evidence"]
    if isinstance(citations, list) and citations:
        lines.extend("- " + escape_markdown(item) for item in citations)
    else:
        lines.append("- None recorded.")
    lines.extend([
        "",
        "**Exploit scenario.**",
        "",
    ])
    scenarios = finding["exploit_scenarios"]
    if isinstance(scenarios, list) and scenarios:
        lines.extend(
            f"{position}. " + escape_markdown(item)
            for position, item in enumerate(scenarios, 1)
        )
    else:
        lines.append("1. None recorded.")
    lines.extend(["", "**Preconditions.**", ""])
    preconditions = finding["preconditions"]
    if isinstance(preconditions, list) and preconditions:
        lines.extend("- " + escape_markdown(item) for item in preconditions)
    else:
        lines.append("- None recorded.")
    lines.extend(
        [
            "",
            "**Relevant code.**",
            "",
            indented_code(finding["snippet"]),
            "",
            "**Fix.**",
            "",
        ]
    )
    recommendations = finding["recommendations"]
    if isinstance(recommendations, list) and recommendations:
        lines.extend(
            f"{position}. " + escape_markdown(item)
            for position, item in enumerate(recommendations, 1)
        )
    else:
        lines.append("1. None recorded.")
    lines.extend(
        [
            "",
            (
                f"**Verification.** {true_votes}/{len(finding_votes)} initial "
                "panel voters confirmed."
            ),
            "",
        ]
    )
    return lines


def finding_markdown(
    finding: Finding,
    votes: Sequence[Mapping[str, object]],
) -> List[str]:
    title = escape_markdown(finding["title"])
    lines = [
        (
            f"### {finding['id']} — {title} "
            f"({finding['severity']}, difficulty {finding['difficulty']}, "
            f"confidence {finding['confidence']})"
        ),
        "",
        f"**Stable finding ID.** {code_span(finding['findingId'])}",
        "",
        f"**Occurrence ID.** {code_span(finding['occurrenceId'])}",
        "",
    ]
    lines.extend(finding_detail_markdown(finding, votes))
    return lines


def duplicate_markdown(
    finding: Finding,
    primaries_by_id: Mapping[object, Finding],
    votes: Sequence[Mapping[str, object]],
) -> List[str]:
    primary = primaries_by_id.get(finding["duplicate_of"]) or {}
    primary_label = str(primary.get("id") or "an earlier finding")
    primary_title = escape_markdown(primary.get("title") or "unknown finding")
    title = escape_markdown(finding["title"])
    where = f"**Where.** {code_span(finding['file'])}:{finding['line']}"
    if finding["symbol"]:
        where += f" in {code_span(finding['symbol'])}"
    # Concise by default: the label, the reason, and the location. The full
    # verified detail stays folded in a <details> block a reader opens only
    # when the duplicate is the one they need to inspect.
    return [
        f"### {finding['id']} — {title} ({finding['severity']})",
        "",
        f"**Duplicate of {primary_label} — {primary_title}.**",
        "",
        "**Why it is a duplicate.** "
        + escape_markdown(finding["duplicate_reasoning"]),
        "",
        where,
        "",
        "<details>",
        "<summary>Full finding detail</summary>",
        "",
        *finding_detail_markdown(finding, votes),
        "</details>",
        "",
    ]


def render_markdown(
    manifest: JsonMap,
    findings: Sequence[Finding],
    coverage: JsonMap,
    votes: Sequence[Mapping[str, object]],
) -> str:
    request = as_map(manifest["request"]) or {}
    target = as_map(manifest["target"]) or {}
    completion = as_map(manifest["completion"]) or {}
    revision = as_map(manifest["revision"]) or {}
    primaries = [finding for finding in findings if not finding["duplicate_of"]]
    duplicates = [finding for finding in findings if finding["duplicate_of"]]
    primaries_by_id = {finding["findingId"]: finding for finding in primaries}
    counts = {severity: 0 for severity in SEVERITIES}
    for finding in primaries:
        counts[str(finding["severity"])] += 1
    revision_value = revision.get("commit") or "unversioned"
    headline = (
        f"{len(primaries)} unique panel-verified finding"
        f"{'' if len(primaries) == 1 else 's'}"
    )
    if primaries:
        headline += (
            f": {counts['HIGH']} high, {counts['MEDIUM']} medium, "
            f"and {counts['LOW']} low."
        )
    else:
        headline += "."
    if duplicates:
        headline += (
            f" {len(duplicates)} further report"
            f"{'' if len(duplicates) == 1 else 's'} folded into the appendix "
            f"as duplicate{'' if len(duplicates) == 1 else 's'}."
        )
    lines = [
        "# Security review results",
        "",
        (
            f"Reviewed {code_span(target.get('scanRoot'))} at "
            f"{code_span(revision_value)} in "
            f"{escape_markdown(request.get('mode') or 'unknown')} mode, at "
            f"{escape_markdown(request.get('effort') or 'unknown')} effort. "
            f"Completed {escape_markdown(manifest['completedAt'])}. {headline}"
        ),
        "",
        (
            "**Completion.** "
            + escape_markdown(completion.get("status") or "unknown")
            + (
                " — "
                + "; ".join(
                    escape_markdown(reason)
                    for reason in completion.get("reasons", [])
                    if isinstance(reason, str)
                )
                if completion.get("reasons")
                else ""
            )
        ),
        "",
    ]
    lines.extend(coverage_markdown(coverage))
    lines.extend(["## Findings", ""])
    if primaries:
        for finding in primaries:
            lines.extend(finding_markdown(finding, votes))
    elif duplicates:
        lines.extend(
            [
                "Every verified finding was identified as a duplicate of "
                "another. See the appendix.",
                "",
            ]
        )
    else:
        lines.extend(
            [
                "No candidate met the reportable verification standard.",
                "",
            ]
        )
    if duplicates:
        lines.extend(
            [
                "## Appendix: duplicate findings",
                "",
                (
                    f"The duplicate scan judged these {len(duplicates)} verified "
                    f"report{'' if len(duplicates) == 1 else 's'} to describe a "
                    "vulnerability already listed above. Each is kept for the "
                    "record with the finding it duplicates and the reason."
                ),
                "",
            ]
        )
        for finding in duplicates:
            lines.extend(
                duplicate_markdown(finding, primaries_by_id, votes)
            )
    completed_votes = sum(vote["status"] == "completed" for vote in votes)
    lines.extend(
        [
            "## What was verified",
            "",
            (
                f"The canonical bundle records {len(votes)} dispatched "
                f"verification vote{'' if len(votes) == 1 else 's'}, of which "
                f"{completed_votes} completed. Every reported finding has a "
                "complete three-voter initial panel and at least two "
                "TRUE_POSITIVE verdicts."
            ),
            "",
            (
                "Findings were derived from source and history review. The "
                "workflow does not attest whether agents executed commands."
            ),
            "",
        ]
    )
    return "\n".join(lines)


def verification_summary(
    manifest: JsonMap,
    findings: Sequence[Finding],
    votes: Sequence[Mapping[str, object]],
) -> Dict[str, object]:
    completion = as_map(manifest["completion"]) or {}
    panel_reviewed = 0
    panel_quorum = 0
    for finding in findings:
        panel = [
            vote
            for vote in votes
            if vote["findingId"] == finding["findingId"]
            and vote["round"] == "panel"
            and vote["status"] == "completed"
        ]
        if len(panel) == 3:
            panel_reviewed += 1
        if sum(vote["verdict"] == "TRUE_POSITIVE" for vote in panel) >= 2:
            panel_quorum += 1
    return {
        "status": completion["verificationStatus"],
        "completion_status": completion["status"],
        "candidates": completion["rawCandidateReports"],
        "candidates_deduped": completion["uniqueCandidates"],
        "panel_votes": sum(vote["status"] == "completed" for vote in votes),
        "panel_reviewed_findings": panel_reviewed,
        "panel_quorum_findings": panel_quorum,
        "attested_findings": len(findings),
        "reason": (
            "; ".join(
                str(reason)
                for reason in completion.get("reasons", [])
            )
            or None
        ),
    }


def display_date(value: object) -> str:
    text = str(value or "")
    match = re.match(r"^(\d{4})-(\d{2})-(\d{2})", text)
    if not match:
        return text
    year, month, day = (int(part) for part in match.groups())
    if not 1 <= month <= 12:
        return text
    return f"{day} {MONTH_NAMES[month - 1]} {year}"


def title_word(value: object) -> str:
    text = str(value or "")
    return text[:1].upper() + text[1:].lower()


def display_category(slug: object) -> str:
    """Map a canonical slug to its display name, or refuse the bundle.

    There is deliberately no fallback. Title-casing an unknown slug once let a
    taxonomy break ship inside a finished report, so an unmapped slug is a
    refusal the run cannot miss.
    """
    text = str(slug or "")
    mapped = CATEGORY_DISPLAY.get(text)
    if not mapped:
        raise RenderError(
            f"category slug {text!r} is not in the taxonomy; "
            f"see {TAXONOMY_PATH.name}"
        )
    return mapped


def rule_category(rule_id: object) -> str:
    """The category slug is the first segment of the ruleId."""
    return str(rule_id or "").split(".", 1)[0]


def display_id(value: object) -> str:
    text = str(value or "")
    match = re.fullmatch(r"F([1-9][0-9]*)", text)
    if not match:
        return text
    return f"F-{int(match.group(1)):03d}"


def repository_name(scan_root: object) -> str:
    """The reviewed repository's own name, for the report's identity line."""
    text = str(scan_root or "").replace("\\", "/").rstrip("/")
    name = text.rsplit("/", 1)[-1] if "/" in text else text
    return name or text or "repository"


def component_for_file(
    path: str,
    components: Sequence[Mapping[str, object]],
) -> Optional[str]:
    """Name the examined component a finding's file belongs to.

    One component covers every finding, so naming it beside each location says
    nothing. A component whose path is the whole tree localizes nothing either.
    Both cases yield no name rather than a constant repeated on every finding.
    """
    if len(components) < 2:
        return None
    best_name: Optional[str] = None
    best_length = 0
    for component in components:
        name = component.get("name")
        raw_paths = component.get("paths")
        if not isinstance(name, str) or not isinstance(raw_paths, list):
            continue
        for raw_path in raw_paths:
            if not isinstance(raw_path, str):
                continue
            prefix = raw_path.rstrip("/")
            if prefix in ("", "."):
                continue
            if path == prefix or path.startswith(prefix + "/"):
                length = len(prefix)
            else:
                continue
            if length > best_length:
                best_name = name
                best_length = length
    return best_name


def panel_confirmations(
    finding: Finding,
    votes: Sequence[Mapping[str, object]],
) -> Tuple[int, int]:
    panel = [
        vote
        for vote in votes
        if vote["findingId"] == finding["findingId"]
        and vote["round"] == "panel"
        and vote["status"] == "completed"
    ]
    true_votes = sum(vote["verdict"] == "TRUE_POSITIVE" for vote in panel)
    return true_votes, len(panel)


def html_finding(
    finding: Finding,
    components: Sequence[Mapping[str, object]],
    votes: Sequence[Mapping[str, object]],
) -> Dict[str, object]:
    true_votes, panel_size = panel_confirmations(finding, votes)
    recommendations = string_list(
        finding["recommendations"],
        "finding recommendations",
    )
    payload: Dict[str, object] = {
        "id": display_id(finding["id"]),
        "title": finding["title"],
        "severity": title_word(finding["severity"]),
        "difficulty": title_word(finding["difficulty"]),
        "confidence": title_word(finding["confidence"]),
        "category": display_category(rule_category(finding["ruleId"])),
        "file": finding["file"],
        "line": finding["line"],
        "symbol": finding["symbol"],
        # The excerpt read from the tree is preferred. This is the reporter's
        # quoted line, which the page falls back to when there is no excerpt.
        "snippet": finding["snippet"],
        # The claim is the body text. The source-to-sink citations get their
        # own block, which the page keeps collapsed.
        "description": [
            paragraph.strip()
            for paragraph in re.split(r"\n\s*\n", str(finding["description"]))
            if paragraph.strip()
        ],
        "evidence": list(finding["evidence"]),
        "impact": finding["impact"],
        "exploitScenarios": string_list(
            finding["exploit_scenarios"],
            "finding exploit_scenarios",
        ),
        "preconditions": string_list(
            finding["preconditions"],
            "finding preconditions",
        ),
        "recommendations": recommendations or ["None recorded."],
        "verification": (
            f"{true_votes}/{panel_size} review lenses confirmed."
            if panel_size
            else "No completed panel vote was recorded."
        ),
        "cwe": finding["cwe_id"] or "",
        "code": finding["code"],
    }
    component = component_for_file(str(finding["file"]), components)
    if component:
        payload["target"] = component
    return payload


def html_duplicate(
    finding: Finding,
    primaries_by_id: Mapping[object, Finding],
    components: Sequence[Mapping[str, object]],
    votes: Sequence[Mapping[str, object]],
) -> Dict[str, object]:
    # A duplicate carries the same full payload as any finding, so the page can
    # disclose its detail on demand, plus a back-reference to its primary and
    # the reason it was demoted.
    payload = html_finding(finding, components, votes)
    primary = primaries_by_id.get(finding["duplicate_of"]) or {}
    payload["duplicateOfId"] = (
        display_id(primary["id"]) if primary.get("id") else ""
    )
    payload["duplicateOfTitle"] = str(primary.get("title") or "")
    payload["duplicateReasoning"] = str(finding["duplicate_reasoning"] or "")
    return payload


def report_payload(
    manifest: JsonMap,
    findings: Sequence[Finding],
    coverage: Mapping[str, object],
    votes: Sequence[Mapping[str, object]],
) -> Dict[str, object]:
    request = as_map(manifest["request"]) or {}
    target = as_map(manifest["target"]) or {}
    raw_components = coverage.get("components")
    components = [
        component
        for component in (
            raw_components if isinstance(raw_components, list) else []
        )
        if isinstance(component, dict)
    ]
    mode = str(request.get("mode") or "")
    completion = as_map(manifest["completion"]) or {}
    primaries = [finding for finding in findings if not finding["duplicate_of"]]
    duplicates = [finding for finding in findings if finding["duplicate_of"]]
    primaries_by_id = {finding["findingId"]: finding for finding in primaries}
    return {
        "report": {
            "title": HTML_REPORT_TITLE,
            "date": display_date(manifest["completedAt"]),
        },
        # A partial scan must say so where the findings are read, or a reader
        # takes an incomplete review for a clean one.
        "completion": {
            "status": completion.get("status"),
            "reasons": string_list(
                completion.get("reasons"),
                "manifest completion.reasons",
            ),
        },
        "scan": {
            "root": target.get("scanRoot"),
            "repository": repository_name(target.get("scanRoot")),
            "revision": revision_tag(manifest["revision"]),
            "mode": mode,
            "modeLabel": MODE_LABELS.get(mode, mode or "Unknown"),
            "scope": list(request.get("scope") or []),
            "effort": request.get("effort"),
            "generatedAt": manifest["completedAt"],
        },
        "coverage": {
            "completeness": coverage.get("completenessCheckOutcome"),
            "researchersDispatched": coverage.get("researchersDispatched"),
            "researchersReturned": coverage.get("researchersReturned"),
            "components": components,
            "skippedComponents": coverage.get("skippedComponents") or [],
            "unaccountedTopLevelDirs": (
                coverage.get("unaccountedTopLevelDirs") or []
            ),
        },
        "severityOrder": [name for name, _ in SEVERITY_DEFINITIONS],
        "difficultyOrder": [name for name, _ in DIFFICULTY_DEFINITIONS],
        "severityDefinitions": [
            {"name": name, "description": description}
            for name, description in SEVERITY_DEFINITIONS
        ],
        "difficultyDefinitions": [
            {"name": name, "description": description}
            for name, description in DIFFICULTY_DEFINITIONS
        ],
        "categories": [
            {"name": name, "description": description}
            for name, description in CATEGORY_DEFINITIONS
        ],
        "findings": [
            html_finding(finding, components, votes) for finding in primaries
        ],
        "duplicates": [
            html_duplicate(finding, primaries_by_id, components, votes)
            for finding in duplicates
        ],
    }


def embed_json(value: object) -> str:
    """Serialize report data for a <script> block.

    `ensure_ascii` escapes every non-ASCII codepoint, which covers the line and
    paragraph separators that would otherwise end a JavaScript statement. The
    angle brackets and ampersand are escaped so no string can close the script
    element or open an HTML comment.
    """
    text = json.dumps(value, ensure_ascii=True, indent=2, sort_keys=True)
    return (
        text.replace("&", "\\u0026")
        .replace("<", "\\u003c")
        .replace(">", "\\u003e")
    )


def read_template() -> str:
    path = os.path.normpath(
        os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            *TEMPLATE_RELATIVE_PATH,
        )
    )
    try:
        with open(path, encoding="utf-8") as handle:
            template = handle.read()
    except OSError as error:
        raise RenderError(
            f"the report template is missing or unreadable: {error}"
        ) from error
    if template.count(REPORT_DATA_MARKER) != 1:
        raise RenderError(
            "the report template must name the data marker exactly once"
        )
    return template


def render_html(
    manifest: JsonMap,
    findings: Sequence[Finding],
    coverage: Mapping[str, object],
    votes: Sequence[Mapping[str, object]],
) -> str:
    payload = report_payload(manifest, findings, coverage, votes)
    return read_template().replace(REPORT_DATA_MARKER, embed_json(payload))


def revision_tag(revision: object) -> str:
    value = as_map(revision) or {}
    commit = value.get("commit") or value.get("head")
    if not commit:
        return "UNVERSIONED"
    if not isinstance(commit, str) or not HEX_RE.fullmatch(commit):
        raise RenderError("manifest revision cannot produce a display tag")
    return commit[:12] + ("" if value.get("dirty") is False else "-dirty")


def atomic_write(path: str, text: str) -> None:
    directory = os.path.dirname(path)
    handle, temporary = tempfile.mkstemp(dir=directory, prefix=".render.")
    try:
        with os.fdopen(handle, "w", encoding="utf-8", newline="\n") as output:
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temporary)
        raise


def jsonl_line(finding: Finding) -> str:
    return json.dumps(
        finding,
        ensure_ascii=False,
        separators=(",", ":"),
    ).translate(SEPARATOR_ESCAPES)


def render(
    bundle_dir: str,
    report_dir: str,
    metadata_dir: str,
) -> Tuple[List[Finding], Dict[str, object]]:
    manifest = validate_manifest(read_json(bundle_dir, "scan-manifest.json"))
    target = as_map(manifest["target"]) or {}
    target_id = str(target["id"])
    scan_id = str(manifest["scanId"])
    findings = validate_findings(
        read_json(bundle_dir, "findings.json"),
        target_id,
        scan_id,
    )
    ledger = validate_ledger(
        read_jsonl(bundle_dir, "candidate-ledger.jsonl"),
        target_id,
        scan_id,
    )
    ledger_by_id = {
        (as_map(entry["candidate"]) or {}).get("findingId"): entry
        for entry in ledger
    }
    votes = validate_votes(
        read_jsonl(bundle_dir, "panel-votes.jsonl"),
        ledger_by_id,
    )
    coverage = validate_coverage(read_json(bundle_dir, "coverage.json"))
    validate_relationships(manifest, findings, ledger, votes, coverage)

    markdown = render_markdown(manifest, findings, coverage, votes)
    atomic_write(
        os.path.join(report_dir, "SECURITY-REVIEW-RESULTS.md"),
        markdown,
    )
    atomic_write(
        os.path.join(report_dir, HTML_REPORT_NAME),
        render_html(manifest, findings, coverage, votes),
    )
    atomic_write(
        os.path.join(report_dir, "SECURITY-REVIEW-RESULTS.jsonl"),
        "".join(jsonl_line(finding) + "\n" for finding in findings),
    )

    counts = {severity: 0 for severity in SEVERITIES}
    for finding in findings:
        counts[str(finding["severity"])] += 1
    verification = verification_summary(manifest, findings, votes)
    revision = manifest["revision"]
    revision_metadata = {
        "generated_at": manifest["completedAt"],
        "scan_id": scan_id,
        "target_id": target_id,
        "target_id_source": target["idSource"],
        "scan_root": target["scanRoot"],
        "products_dir": report_dir,
        "metadata_dir": metadata_dir,
        "mode": (as_map(manifest["request"]) or {}).get("mode"),
        "scope": (as_map(manifest["request"]) or {}).get("scope") or [],
        "revision": revision,
        "revision_source": "self-reported",
        "effort": (as_map(manifest["request"]) or {}).get("effort"),
        "findings": {
            "total": len(findings),
            "high": counts["HIGH"],
            "medium": counts["MEDIUM"],
            "low": counts["LOW"],
        },
        "verification": verification,
        "canonical_bundle": {
            "schema_version": SCHEMA_VERSION,
            "directory": bundle_dir,
            "files": list(CANONICAL_FILES),
        },
    }
    atomic_write(
        os.path.join(metadata_dir, REVISION_NAME),
        json.dumps(revision_metadata, ensure_ascii=False, indent=2) + "\n",
    )
    return findings, verification


def main(argv: Sequence[str]) -> int:
    if len(argv) != 3:
        die(
            "usage: render_report.py "
            "<evidence-dir> <result-dir> <metadata-dir>"
        )
    bundle_dir, report_dir, metadata_dir = argv
    for label, directory in (
        ("evidence", bundle_dir),
        ("result", report_dir),
        ("metadata", metadata_dir),
    ):
        if not os.path.isdir(directory):
            die(f"{label} directory does not exist: {directory}")
    try:
        findings, verification = render(
            bundle_dir,
            report_dir,
            metadata_dir,
        )
    except RenderError as error:
        die(str(error))
    except OSError as error:
        die(f"could not read or write the completed bundle: {error}")
    print(
        f"wrote SECURITY-REVIEW-RESULTS.md, {HTML_REPORT_NAME}, "
        f"SECURITY-REVIEW-RESULTS.jsonl ({len(findings)} finding"
        f"{'' if len(findings) == 1 else 's'}) into {report_dir}, and "
        f"{REVISION_NAME} into {metadata_dir}"
    )
    print(f"verification.status: {verification.get('status')}")
    print(f"completion.status: {verification.get('completion_status')}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
