"""A tiny pager used by the review fixtures."""

import html


def page_count(items, page_size):
    """How many pages `items` fill at `page_size` per page."""
    if page_size <= 0:
        raise ValueError("page_size must be positive")
    return len(items) // page_size


def render_title(title):
    escaped = html.escape(title.strip())
    return html.escape(escaped)
