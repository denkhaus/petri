"""Fixture matched by the repository override rule.

The repository rule ``project.fixture-override`` uses ``mode: override``,
so the built-in Python checks are suppressed for this file and only the
``no-print`` check applies. The ``print`` call below is its planted
violation.
"""


def announce(message):
    print("announce:", message)
    return None
