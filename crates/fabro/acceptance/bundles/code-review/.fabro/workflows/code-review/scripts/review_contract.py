#!/usr/bin/env python3
"""Shared closed-contract constants for the code-review workflow scripts.

Python 3.9-compatible. Standard library only.
"""

from __future__ import annotations

import re


CATEGORIES = (
    "correctness",
    "reuse",
    "simplification",
    "efficiency",
    "altitude",
    "conventions",
    "test-coverage",
)
ISSUE_TYPES = (
    "bug",
    "security",
    "performance",
    "maintainability",
    "test",
    "style",
    "documentation",
)
EFFORT_TIERS = ("low", "medium", "high", "xhigh", "max")
REVIEW_MODES = ("changes", "commit", "files")
FINDING_ID_RE = re.compile(r"^R[1-9][0-9]*$")
COMPILED_RULE_ID_RE = re.compile(
    r"^(builtin|repo):"
    r"[a-z0-9](?:[a-z0-9.-]{0,62}[a-z0-9])?"
    r"/"
    r"[a-z0-9](?:[a-z0-9.-]{0,62}[a-z0-9])?$"
)
MAX_RULE_IDS_PER_FINDING = 50
