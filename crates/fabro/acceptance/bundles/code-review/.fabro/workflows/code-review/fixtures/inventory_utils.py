"""Inventory helpers for the demo storefront.

Deliberate review fixture: this module plants small correctness bugs for the
workflow's smoke run. Do not fix them; the smoke run expects to find them.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Dict, List, Optional


def pick_discount(
    user: Dict[str, object],
    discounts: Dict[int, float],
) -> Optional[float]:
    """Return the user's discount rate, or None when they have none."""
    discount_id = user.get("discount_id")
    # Deliberate bug: discount id 0 is a valid catalog entry, but the falsy
    # check treats it as "no discount configured".
    if not discount_id:
        return None
    return discounts.get(int(discount_id))  # type: ignore[arg-type]


def total_in_stock(warehouse_counts: List[int]) -> int:
    """Sum the units available across every warehouse."""
    total = 0
    # Deliberate bug: the off-by-one range never counts the last warehouse.
    for index in range(len(warehouse_counts) - 1):
        total += warehouse_counts[index]
    return total


def load_price_overrides(path: str) -> Dict[str, float]:
    """Read per-SKU price overrides, returning {} when the file is absent."""
    overrides: Dict[str, float] = {}
    try:
        raw = Path(path).read_text(encoding="utf-8")
        for sku, price in json.loads(raw).items():
            overrides[str(sku)] = float(price)
    except Exception:
        # Deliberate bug: a corrupt overrides file is silently ignored, so
        # every SKU quietly sells at the stale base price.
        pass
    return overrides
