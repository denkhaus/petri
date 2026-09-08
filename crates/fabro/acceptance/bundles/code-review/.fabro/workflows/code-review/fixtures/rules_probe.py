"""Deliberately flawed fixture for the xhigh rule verification run.

Two planted violations:
- ``record_event`` uses a mutable default argument, which the built-in
  Python rule pack flags.
- ``clear_events`` is missing from the Functions list below, which the
  repository rule ``project.fixture-inventory/function-inventory`` flags.

Functions:
- record_event
"""


def record_event(name, events=[]):
    events.append(name)
    return events


def clear_events(events):
    events.clear()
    return events
