#!/usr/bin/env python3
"""Rule loading, matching, and composition for the code-review workflow.

Parses the built-in and repository YAML rule files, validates them against the
closed version-1 contract, matches repository-relative POSIX paths against
their glob patterns, composes the effective check set for each reviewed file,
and canonicalizes the whole configuration to a stable JSON form for hashing.

The YAML loader is a restricted PyYAML SafeLoader subclass: safe scalar,
mapping, sequence, and block-scalar types only; no custom tags, anchors,
aliases, merge keys, or duplicate mapping keys; no implicit timestamps; only
``true`` and ``false`` carry boolean semantics. These restrictions are
enforced here, not assumed from ``safe_load``.

Python 3.9-compatible. Requires the pinned PyYAML dependency (see
requirements-rules.txt); everything else is standard library.
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

from review_contract import CATEGORIES, COMPILED_RULE_ID_RE  # noqa: E402


class RuleLoaderError(ValueError):
    """A deterministic rule-configuration failure."""


# --- Contract limits ---------------------------------------------------------

RULE_DOCUMENT_VERSION = 1
MAX_RULE_FILE_BYTES = 512 * 1024
MAX_REPO_RULE_TOTAL_BYTES = 2 * 1024 * 1024
MAX_PACKS_PER_LAYER = 200
MAX_CHECKS_PER_PACK = 50
MAX_PATTERNS_PER_LIST = 50
MAX_PATTERN_LENGTH = 400
MAX_GUIDANCE_LENGTH = 8000
MAX_DESCRIPTION_LENGTH = 2000
MAX_BRACE_EXPANSIONS = 256
MAX_YAML_DEPTH = 40

LAYERS = ("builtin", "repo")
MODES = ("merge", "override")

ID_RE = re.compile(r"^[a-z0-9](?:[a-z0-9.-]{0,62}[a-z0-9])?$")
COMPILED_ID_RE = COMPILED_RULE_ID_RE

# The default pack applies only to files no other built-in pack matches, and
# the repository-instructions pack applies to every file. Both behaviors are
# engine rules keyed by these IDs, mirroring OCR's default_rule semantics.
DEFAULT_PACK_ID = "default"
INSTRUCTIONS_PACK_ID = "repository.instructions"
# The ".m" extension is shared by MATLAB and Objective-C; a deterministic
# content sniff selects between these two built-in packs.
MATLAB_PACK_ID = "language.matlab"
OBJC_PACK_ID = "language.objective-c"

REPO_ENTRYPOINT = ".fabro/rules.yaml"
REPO_RULES_PREFIX = ".fabro/rules/"

# Relative to the workflow root (.fabro/workflows/code-review).
BUILTIN_RULES_DIR = "rules/builtin"
BUILTIN_MANIFEST = "rules/builtin-manifest.json"


# --- Restricted YAML loading -------------------------------------------------

_ALLOWED_TAGS = frozenset(
    {
        "tag:yaml.org,2002:str",
        "tag:yaml.org,2002:int",
        "tag:yaml.org,2002:float",
        "tag:yaml.org,2002:bool",
        "tag:yaml.org,2002:null",
        "tag:yaml.org,2002:seq",
        "tag:yaml.org,2002:map",
    }
)
_STRIPPED_IMPLICIT_TAGS = frozenset(
    {
        "tag:yaml.org,2002:bool",
        "tag:yaml.org,2002:timestamp",
        "tag:yaml.org,2002:value",
        "tag:yaml.org,2002:merge",
    }
)


class _RestrictedLoader(yaml.SafeLoader):
    """SafeLoader minus YAML 1.1 surprises and document-shaping features."""

    def compose_node(self, parent: Any, index: Any) -> Any:
        if self.check_event(yaml.events.AliasEvent):
            raise RuleLoaderError("YAML aliases are not allowed in rule files")
        event = self.peek_event()
        if getattr(event, "anchor", None) is not None:
            raise RuleLoaderError("YAML anchors are not allowed in rule files")
        depth = getattr(self, "_restricted_depth", 0)
        if depth >= MAX_YAML_DEPTH:
            raise RuleLoaderError("rule file nesting is too deep")
        self._restricted_depth = depth + 1
        try:
            return super().compose_node(parent, index)
        finally:
            self._restricted_depth = depth

    def construct_object(self, node: Any, deep: bool = False) -> Any:
        if node.tag not in _ALLOWED_TAGS:
            raise RuleLoaderError(
                f"YAML tag is not allowed in rule files: {node.tag}"
            )
        return super().construct_object(node, deep=deep)

    def construct_mapping(self, node: Any, deep: bool = False) -> Any:
        if not isinstance(node, yaml.MappingNode):
            raise RuleLoaderError("expected a YAML mapping")
        seen = set()
        for key_node, _value_node in node.value:
            if key_node.tag != "tag:yaml.org,2002:str":
                raise RuleLoaderError(
                    "YAML mapping keys must be plain strings"
                )
            if key_node.value == "<<":
                raise RuleLoaderError(
                    "YAML merge keys are not allowed in rule files"
                )
            if key_node.value in seen:
                raise RuleLoaderError(
                    f"duplicate YAML mapping key: {key_node.value!r}"
                )
            seen.add(key_node.value)
        return super().construct_mapping(node, deep=deep)


# Drop the YAML 1.1 implicit resolvers (yes/no/on/off booleans, timestamps,
# "=" values, "<<" merges), then resolve exactly ``true``/``false`` as
# booleans, YAML 1.2 style.
_RestrictedLoader.yaml_implicit_resolvers = {
    key: [
        (tag, regexp)
        for tag, regexp in resolvers
        if tag not in _STRIPPED_IMPLICIT_TAGS
    ]
    for key, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}
_RestrictedLoader.add_implicit_resolver(
    "tag:yaml.org,2002:bool",
    re.compile(r"^(?:true|false)$"),
    list("tf"),
)


def _construct_bool(loader: Any, node: Any) -> bool:
    value = loader.construct_scalar(node)
    if value == "true":
        return True
    if value == "false":
        return False
    raise RuleLoaderError(
        "only lowercase true and false carry boolean semantics"
    )


_RestrictedLoader.add_constructor("tag:yaml.org,2002:bool", _construct_bool)


def parse_rule_yaml(text: str, source: str) -> Any:
    """Parse one rule file with the restricted loader."""
    try:
        return yaml.load(text, Loader=_RestrictedLoader)
    except RuleLoaderError as error:
        raise RuleLoaderError(f"{source}: {error}") from error
    except yaml.YAMLError as error:
        raise RuleLoaderError(
            f"{source}: not valid YAML: {type(error).__name__}"
        ) from error


# --- Glob matching -----------------------------------------------------------


def brace_expand(pattern: str, source: str = "pattern") -> List[str]:
    """Expand ``{a,b}`` alternatives, depth-first, with a hard output cap."""

    def find_brace(text: str) -> Optional[Tuple[int, int, List[str]]]:
        start = text.find("{")
        if start < 0:
            if "}" in text:
                raise RuleLoaderError(f"{source}: unbalanced '}}' in glob")
            return None
        depth = 0
        alternatives: List[str] = []
        piece_start = start + 1
        for index in range(start, len(text)):
            character = text[index]
            if character == "{":
                depth += 1
            elif character == "}":
                depth -= 1
                if depth == 0:
                    alternatives.append(text[piece_start:index])
                    return start, index, alternatives
            elif character == "," and depth == 1:
                alternatives.append(text[piece_start:index])
                piece_start = index + 1
        raise RuleLoaderError(f"{source}: unbalanced '{{' in glob")

    results: List[str] = []
    queue = [pattern]
    while queue:
        text = queue.pop()
        found = find_brace(text)
        if found is None:
            results.append(text)
            continue
        start, end, alternatives = found
        for alternative in alternatives:
            queue.append(text[:start] + alternative + text[end + 1 :])
        if len(queue) + len(results) > MAX_BRACE_EXPANSIONS:
            raise RuleLoaderError(
                f"{source}: glob brace expansion exceeds "
                f"{MAX_BRACE_EXPANSIONS} alternatives"
            )
    return sorted(set(results))


def _translate_segment(segment: str, source: str) -> str:
    out: List[str] = []
    index = 0
    while index < len(segment):
        character = segment[index]
        if character == "*":
            out.append("[^/]*")
        elif character == "?":
            out.append("[^/]")
        elif character == "[":
            end = index + 1
            negate = False
            if end < len(segment) and segment[end] in "!^":
                negate = True
                end += 1
            if end < len(segment) and segment[end] == "]":
                end += 1
            while end < len(segment) and segment[end] != "]":
                end += 1
            if end >= len(segment):
                raise RuleLoaderError(
                    f"{source}: unterminated character class in glob"
                )
            body = segment[index + 1 + (1 if negate else 0) : end]
            if "/" in body or "\\" in body:
                raise RuleLoaderError(
                    f"{source}: character class cannot contain '/' or '\\\\'"
                )
            out.append("[" + ("^" if negate else "") + body + "]")
            index = end
        else:
            out.append(re.escape(character))
        index += 1
    return "".join(out)


def translate_glob(pattern: str, source: str = "pattern") -> str:
    """Translate one brace-free glob into an anchored regex body.

    ``*`` and ``?`` stay within a path segment; ``**`` matches zero or more
    whole segments.
    """
    if not pattern:
        raise RuleLoaderError(f"{source}: glob is empty")
    if pattern.startswith("/"):
        raise RuleLoaderError(
            f"{source}: glob must be repository-relative, not absolute"
        )
    segments = pattern.split("/")
    if any(segment == "" for segment in segments):
        raise RuleLoaderError(f"{source}: glob has an empty path segment")

    runs: List[List[str]] = [[]]
    for segment in segments:
        if segment == "**":
            if runs[-1] or len(runs) == 1:
                runs.append([])
        else:
            runs[-1].append(_translate_segment(segment, source))

    if len(runs) == 1:
        return "/".join(runs[0])
    head, tail = runs[0], runs[1:]
    if head:
        regex = "/".join(head)
        for run in tail:
            if run:
                regex += "(?:/[^/]+)*/" + "/".join(run)
            else:
                regex += "(?:/[^/]+)*"
        return regex
    if not any(tail):
        return ".*"
    regex = "(?:[^/]+/)*"
    started = False
    for run in tail:
        if run:
            if started:
                regex += "(?:/[^/]+)*/"
            regex += "/".join(run)
            started = True
        elif started:
            regex += "(?:/[^/]+)*"
    return regex


def compile_glob(pattern: str, source: str = "pattern") -> "re.Pattern[str]":
    """Compile one glob. Matching is case-insensitive, as in OCR: the
    pattern is lowercased here and paths are lowercased before matching."""
    bodies = [
        translate_glob(expanded, source)
        for expanded in brace_expand(pattern.lower(), source)
    ]
    if len(bodies) == 1:
        combined = bodies[0]
    else:
        combined = "(?:" + "|".join(bodies) + ")"
    try:
        return re.compile("(?:%s)\\Z" % combined)
    except re.error as error:
        raise RuleLoaderError(
            f"{source}: glob does not compile: {error}"
        ) from error


def validate_pattern_text(pattern: Any, source: str) -> str:
    if not isinstance(pattern, str):
        raise RuleLoaderError(f"{source}: glob pattern must be a string")
    if not pattern or len(pattern) > MAX_PATTERN_LENGTH:
        raise RuleLoaderError(
            f"{source}: glob pattern must be 1..{MAX_PATTERN_LENGTH} characters"
        )
    if any(ord(character) < 0x20 or character == "\x7f" for character in pattern):
        raise RuleLoaderError(
            f"{source}: glob pattern contains control characters"
        )
    if "\\" in pattern:
        raise RuleLoaderError(
            f"{source}: glob patterns use '/' separators, never '\\\\'"
        )
    return pattern


# --- Rule file validation and compilation ------------------------------------


def _require_string(
    value: Any,
    source: str,
    cap: int,
    allow_newlines: bool = False,
) -> str:
    if not isinstance(value, str):
        raise RuleLoaderError(f"{source}: must be a string")
    if not value.strip():
        raise RuleLoaderError(f"{source}: is empty")
    if len(value) > cap:
        raise RuleLoaderError(f"{source}: exceeds {cap} characters")
    allowed = "\n\t" if allow_newlines else "\t"
    if any(
        character not in allowed and ord(character) < 0x20
        for character in value
    ):
        raise RuleLoaderError(f"{source}: contains control characters")
    return value


def _require_id(value: Any, source: str) -> str:
    if not isinstance(value, str) or not ID_RE.fullmatch(value):
        raise RuleLoaderError(
            f"{source}: id must match {ID_RE.pattern} (got {value!r})"
        )
    return value


def _reject_unknown_fields(
    mapping: Mapping[str, Any],
    allowed: Sequence[str],
    source: str,
) -> None:
    unknown = sorted(set(mapping) - set(allowed))
    if unknown:
        raise RuleLoaderError(
            f"{source}: unknown field(s): {', '.join(unknown)}"
        )


def validate_rule_file(
    document: Any,
    layer: str,
    source: str,
) -> List[Dict[str, Any]]:
    """Validate one parsed rule document; return its compiled packs."""
    if layer not in LAYERS:
        raise RuleLoaderError(f"unknown rule layer: {layer!r}")
    if not isinstance(document, dict):
        raise RuleLoaderError(f"{source}: document must be a YAML mapping")
    _reject_unknown_fields(document, ("version", "rules"), source)
    version = document.get("version")
    if isinstance(version, bool) or version != RULE_DOCUMENT_VERSION:
        raise RuleLoaderError(
            f"{source}: version must be {RULE_DOCUMENT_VERSION}"
        )
    rules = document.get("rules")
    if not isinstance(rules, list):
        raise RuleLoaderError(f"{source}: rules must be a sequence")

    packs: List[Dict[str, Any]] = []
    for position, raw in enumerate(rules, 1):
        where = f"{source}: rule {position}"
        if not isinstance(raw, dict):
            raise RuleLoaderError(f"{where}: must be a mapping")
        _reject_unknown_fields(
            raw, ("id", "description", "mode", "match", "checks"), where
        )
        pack_id = _require_id(raw.get("id"), f"{where}: id")
        where = f"{source}: rule {pack_id!r}"

        description = ""
        if "description" in raw:
            description = _require_string(
                raw["description"],
                f"{where}: description",
                MAX_DESCRIPTION_LENGTH,
                allow_newlines=True,
            )

        mode = "merge"
        if "mode" in raw:
            mode = raw["mode"]
            if mode not in MODES:
                raise RuleLoaderError(
                    f"{where}: mode must be one of {', '.join(MODES)}"
                )
        if layer == "builtin" and mode != "merge":
            raise RuleLoaderError(
                f"{where}: built-in rules cannot declare mode {mode!r}"
            )

        match = raw.get("match")
        if not isinstance(match, dict):
            raise RuleLoaderError(f"{where}: match must be a mapping")
        _reject_unknown_fields(match, ("paths", "except"), f"{where}: match")
        raw_paths = match.get("paths")
        if not isinstance(raw_paths, list) or not raw_paths:
            raise RuleLoaderError(
                f"{where}: match.paths must be a non-empty sequence"
            )
        if len(raw_paths) > MAX_PATTERNS_PER_LIST:
            raise RuleLoaderError(
                f"{where}: match.paths exceeds {MAX_PATTERNS_PER_LIST} patterns"
            )
        raw_except = match.get("except", [])
        if not isinstance(raw_except, list):
            raise RuleLoaderError(f"{where}: match.except must be a sequence")
        if len(raw_except) > MAX_PATTERNS_PER_LIST:
            raise RuleLoaderError(
                f"{where}: match.except exceeds "
                f"{MAX_PATTERNS_PER_LIST} patterns"
            )
        paths = [
            validate_pattern_text(item, f"{where}: match.paths")
            for item in raw_paths
        ]
        excepts = [
            validate_pattern_text(item, f"{where}: match.except")
            for item in raw_except
        ]
        path_matchers = [
            compile_glob(item, f"{where}: match.paths") for item in paths
        ]
        except_matchers = [
            compile_glob(item, f"{where}: match.except") for item in excepts
        ]

        raw_checks = raw.get("checks")
        if not isinstance(raw_checks, list) or not raw_checks:
            raise RuleLoaderError(
                f"{where}: checks must be a non-empty sequence"
                + (
                    " (an override with no usable checks is invalid)"
                    if mode == "override"
                    else ""
                )
            )
        if len(raw_checks) > MAX_CHECKS_PER_PACK:
            raise RuleLoaderError(
                f"{where}: checks exceeds {MAX_CHECKS_PER_PACK} entries"
            )
        checks: List[Dict[str, Any]] = []
        seen_check_ids = set()
        for check_position, raw_check in enumerate(raw_checks, 1):
            check_where = f"{where}: check {check_position}"
            if not isinstance(raw_check, dict):
                raise RuleLoaderError(f"{check_where}: must be a mapping")
            _reject_unknown_fields(
                raw_check, ("id", "category", "guidance"), check_where
            )
            check_id = _require_id(raw_check.get("id"), f"{check_where}: id")
            if check_id in seen_check_ids:
                raise RuleLoaderError(
                    f"{where}: duplicate check id {check_id!r}"
                )
            seen_check_ids.add(check_id)
            category = raw_check.get("category")
            if category not in CATEGORIES:
                raise RuleLoaderError(
                    f"{check_where}: category must be one of "
                    f"{', '.join(CATEGORIES)}"
                )
            guidance = _require_string(
                raw_check.get("guidance"),
                f"{check_where}: guidance",
                MAX_GUIDANCE_LENGTH,
                allow_newlines=True,
            )
            checks.append(
                {
                    "id": check_id,
                    "compiled_id": f"{layer}:{pack_id}/{check_id}",
                    "category": category,
                    "guidance": guidance,
                }
            )

        packs.append(
            {
                "layer": layer,
                "pack_id": pack_id,
                "description": description,
                "mode": mode,
                "source_path": source,
                "order": position,
                "match": {"paths": paths, "except": excepts},
                "checks": checks,
                "_path_matchers": path_matchers,
                "_except_matchers": except_matchers,
            }
        )
    return packs


def load_rule_layer(
    files: Sequence[Tuple[str, bytes]],
    layer: str,
) -> List[Dict[str, Any]]:
    """Parse and validate an ordered list of (path, bytes) rule files.

    The caller supplies the files in their authoritative discovery order;
    pack order across the layer follows it.
    """
    packs: List[Dict[str, Any]] = []
    seen_pack_ids: Dict[str, str] = {}
    total_bytes = 0
    for path, raw in files:
        if len(raw) > MAX_RULE_FILE_BYTES:
            raise RuleLoaderError(
                f"{path}: rule file exceeds {MAX_RULE_FILE_BYTES} bytes"
            )
        total_bytes += len(raw)
        if layer == "repo" and total_bytes > MAX_REPO_RULE_TOTAL_BYTES:
            raise RuleLoaderError(
                "repository rule files exceed "
                f"{MAX_REPO_RULE_TOTAL_BYTES} bytes in total"
            )
        try:
            text = raw.decode("utf-8")
        except UnicodeError as error:
            raise RuleLoaderError(
                f"{path}: rule file is not valid UTF-8"
            ) from error
        document = parse_rule_yaml(text, path)
        for pack in validate_rule_file(document, layer, path):
            previous = seen_pack_ids.get(pack["pack_id"])
            if previous is not None:
                raise RuleLoaderError(
                    f"{path}: duplicate {layer} rule id "
                    f"{pack['pack_id']!r} (first declared in {previous})"
                )
            seen_pack_ids[pack["pack_id"]] = path
            packs.append(pack)
    if len(packs) > MAX_PACKS_PER_LAYER:
        raise RuleLoaderError(
            f"{layer} rules exceed {MAX_PACKS_PER_LAYER} packs"
        )
    return packs


def discover_repo_rule_paths(paths: Iterable[str]) -> List[str]:
    """Order candidate repository paths: entrypoint first, then lexical."""
    entrypoint: List[str] = []
    extras: List[str] = []
    for path in paths:
        if path == REPO_ENTRYPOINT:
            entrypoint = [path]
        elif path.startswith(REPO_RULES_PREFIX) and path.endswith(".yaml"):
            extras.append(path)
    return entrypoint + sorted(extras)


def builtin_files_with_hashes(
    workflow_root: Path,
) -> List[Tuple[str, str, bytes]]:
    """Read and hash every built-in YAML file in lexical path order."""
    rules_dir = workflow_root / BUILTIN_RULES_DIR
    entries: List[Tuple[str, str, bytes]] = []
    for path in sorted(rules_dir.rglob("*.yaml")):
        relative = path.relative_to(workflow_root).as_posix()
        contents = path.read_bytes()
        digest = hashlib.sha256(contents).hexdigest()
        entries.append((relative, digest, contents))
    return entries


def builtin_manifest_entries(workflow_root: Path) -> List[Dict[str, str]]:
    """Hash every built-in YAML file on disk, in lexical path order."""
    return [
        {"path": path, "sha256": digest}
        for path, digest, _contents in builtin_files_with_hashes(workflow_root)
    ]


def load_builtin_files(
    workflow_root: Path,
    manifest: Any,
) -> List[Tuple[str, bytes]]:
    """Verify the built-in rule files against their manifest, then read them.

    The manifest is graph-pinned workflow control data. Missing, extra, or
    altered built-in YAML files are all deterministic failures.
    """
    if not isinstance(manifest, dict) or manifest.get("version") != 1:
        raise RuleLoaderError("built-in rule manifest has an unknown version")
    raw_entries = manifest.get("files")
    if not isinstance(raw_entries, list) or not raw_entries:
        raise RuleLoaderError("built-in rule manifest lists no files")
    expected: Dict[str, str] = {}
    for entry in raw_entries:
        if (
            not isinstance(entry, dict)
            or not isinstance(entry.get("path"), str)
            or not isinstance(entry.get("sha256"), str)
        ):
            raise RuleLoaderError("built-in rule manifest entry is malformed")
        expected[entry["path"]] = entry["sha256"]

    actual = builtin_files_with_hashes(workflow_root)
    actual_paths = {path for path, _digest, _contents in actual}
    missing = sorted(set(expected) - actual_paths)
    extra = sorted(actual_paths - set(expected))
    if missing:
        raise RuleLoaderError(
            "built-in rule files are missing: " + ", ".join(missing)
        )
    if extra:
        raise RuleLoaderError(
            "unexpected built-in rule files: " + ", ".join(extra)
        )
    files: List[Tuple[str, bytes]] = []
    for path, digest, contents in actual:
        if expected[path] != digest:
            raise RuleLoaderError(
                f"built-in rule file does not match its manifest hash: "
                f"{path}"
            )
        files.append((path, contents))
    return files


# --- The ".m" content sniff --------------------------------------------------

# First-line signals for Objective-C, ported from OCR's sniffer. MATLAB
# comments start with "%" and a MATLAB file cannot legally begin with "/", so
# a C-style comment opener is itself a reliable Objective-C signal.
# Deliberately not widened to a bare "#": Octave, which also uses ".m", treats
# "#" as a comment character.
OBJC_SNIFF_PREFIXES = (
    "#import",
    "#include",
    "#pragma",
    "#if",
    "#define",
    "@import",
    "@interface",
    "@implementation",
    "@class",
    "@protocol",
    "//",
    "/*",
)


def sniff_m_language(content: Optional[bytes]) -> Tuple[str, str]:
    """Classify one ".m" file's bytes as ("matlab"|"objc", source).

    Missing, binary, undecodable, or blank content keeps the deterministic
    default MATLAB mapping with source "default"; an examined first line
    reports source "content-sniff".
    """
    if content is None or b"\0" in content:
        return "matlab", "default"
    try:
        text = content.decode("utf-8")
    except UnicodeError:
        return "matlab", "default"
    first_line = ""
    for line in text.split("\n"):
        stripped = line.strip()
        if stripped:
            first_line = stripped
            break
    if not first_line:
        return "matlab", "default"
    for prefix in OBJC_SNIFF_PREFIXES:
        if first_line.startswith(prefix):
            return "objc", "content-sniff"
    return "matlab", "content-sniff"


# --- Composition -------------------------------------------------------------


def pack_matches(pack: Mapping[str, Any], path: str) -> Optional[str]:
    """Return the first declared path pattern that selects ``path``, if any.

    Matching is case-insensitive on both sides, following OCR.
    """
    lowered = path.lower()
    for matcher in pack["_except_matchers"]:
        if matcher.match(lowered):
            return None
    for pattern, matcher in zip(pack["match"]["paths"], pack["_path_matchers"]):
        if matcher.match(lowered):
            return pattern
    return None


def effective_checks_for_path(
    path: str,
    builtin_packs: Sequence[Mapping[str, Any]],
    repo_packs: Sequence[Mapping[str, Any]],
    m_language: Optional[str] = None,
) -> Dict[str, Any]:
    """Compose the effective checks for one repository-relative path.

    ``m_language`` carries the ".m" sniff result ("matlab" or "objc") when
    the path needed one; it selects between the MATLAB and Objective-C
    built-in packs.
    """
    matched_builtin: List[Tuple[Mapping[str, Any], str]] = []
    default_match: Optional[Tuple[Mapping[str, Any], str]] = None
    instruction_match: Optional[Tuple[Mapping[str, Any], str]] = None
    for pack in builtin_packs:
        if m_language == "objc" and pack["pack_id"] == MATLAB_PACK_ID:
            continue
        if m_language == "matlab" and pack["pack_id"] == OBJC_PACK_ID:
            continue
        if m_language is None and pack["pack_id"] == OBJC_PACK_ID:
            continue
        pattern = pack_matches(pack, path)
        if pattern is None:
            continue
        if pack["pack_id"] == DEFAULT_PACK_ID:
            default_match = (pack, pattern)
        elif pack["pack_id"] == INSTRUCTIONS_PACK_ID:
            instruction_match = (pack, pattern)
        else:
            matched_builtin.append((pack, pattern))
    # OCR semantics: the default pack applies only when no specific built-in
    # pack matched. The repository-instructions pack applies alongside either.
    if not matched_builtin and default_match is not None:
        matched_builtin.append(default_match)
    if instruction_match is not None:
        matched_builtin.append(instruction_match)

    matched_repo: List[Tuple[Mapping[str, Any], str]] = []
    override = False
    for pack in repo_packs:
        pattern = pack_matches(pack, path)
        if pattern is None:
            continue
        matched_repo.append((pack, pattern))
        if pack["mode"] == "override":
            override = True

    checks: List[Dict[str, Any]] = []
    overridden: List[str] = []
    if override:
        for pack, _pattern in matched_builtin:
            overridden.extend(
                check["compiled_id"] for check in pack["checks"]
            )
    else:
        for pack, pattern in matched_builtin:
            for check in pack["checks"]:
                checks.append(_check_descriptor(pack, check, pattern))
    for pack, pattern in matched_repo:
        for check in pack["checks"]:
            checks.append(_check_descriptor(pack, check, pattern))
    return {"checks": checks, "overridden": overridden}


def _check_descriptor(
    pack: Mapping[str, Any],
    check: Mapping[str, Any],
    pattern: str,
) -> Dict[str, Any]:
    return {
        "id": check["compiled_id"],
        "category": check["category"],
        "guidance": check["guidance"],
        "source": pack["layer"],
        "pack": pack["pack_id"],
        "pack_description": pack["description"],
        "mode": pack["mode"],
        "pattern": pattern,
    }


# --- Canonical form and hashing ----------------------------------------------


def canonical_pack(pack: Mapping[str, Any]) -> Dict[str, Any]:
    return {
        "layer": pack["layer"],
        "id": pack["pack_id"],
        "description": pack["description"],
        "mode": pack["mode"],
        "source_path": pack["source_path"],
        "match": {
            "paths": list(pack["match"]["paths"]),
            "except": list(pack["match"]["except"]),
        },
        "checks": [
            {
                "id": check["id"],
                "compiled_id": check["compiled_id"],
                "category": check["category"],
                "guidance": check["guidance"],
            }
            for check in pack["checks"]
        ],
    }


def canonical_rule_config(
    builtin_packs: Sequence[Mapping[str, Any]],
    repo_packs: Sequence[Mapping[str, Any]],
) -> str:
    """Serialize the composed configuration to sorted, length-stable JSON."""
    payload = {
        "version": RULE_DOCUMENT_VERSION,
        "builtin": sorted(
            (canonical_pack(pack) for pack in builtin_packs),
            key=lambda pack: pack["id"],
        ),
        "repo": sorted(
            (canonical_pack(pack) for pack in repo_packs),
            key=lambda pack: pack["id"],
        ),
    }
    return json.dumps(
        payload, ensure_ascii=True, sort_keys=True, separators=(",", ":")
    )


def rule_config_sha256(
    builtin_packs: Sequence[Mapping[str, Any]],
    repo_packs: Sequence[Mapping[str, Any]],
) -> str:
    return hashlib.sha256(
        canonical_rule_config(builtin_packs, repo_packs).encode("utf-8")
    ).hexdigest()
