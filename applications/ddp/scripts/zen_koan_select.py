#!/usr/bin/env python3
"""zen_koan_select.py -- the mechanical selection behind `script.zen-koan-select`.

Picks the next koan from a catalogue, never repeating one until the catalogue is exhausted.

Ported from DDP's `tools/zen_koan_select.py`, with two changes that are corrections rather
than translations:

1. **Selection is idempotent per day.** The original appended to its history on every call, so
   a retried task consumed a second koan and the first was pushed to nobody. Here the history
   records the date, and a second call on the same date returns the same koan. That is what
   makes the capability safe to retry -- and retry safety is not optional for a step whose
   successor sends a notification.

2. **Order, not chance.** The original used `random.choice`. The contract it was serving is
   "do not repeat until exhausted", which catalogue order satisfies just as well, and order
   makes the run reproducible: same catalogue, same history, same date, same koan. A caller
   who wants a shuffled sequence passes `seed`, which shuffles reproducibly. Unseeded
   randomness gave variety at the cost of never being able to reproduce a run.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    catalog      str  Markdown file of `N. <title>` lines (required)
    history      str  where rotation state is kept (required)
    source       str  attribution recorded with the choice (default 禪宗公案選集)
    date         str  YYYY-MM-DD; defaults to today in the catalogue's timezone
    seed         int  shuffle the unpushed candidates reproducibly (optional)
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = a koan was selected (or today's was returned again)
  2 = selection could not be performed (missing catalogue, empty catalogue, bad input)

The history file is the one thing this writes, and it is its own rotation state rather than
anything externally visible -- hence `side_effect: false` on the manifest. The write is
idempotent per date, which is what keeps that declaration true under retry.
"""

import json
import os
import random
import re
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

# The catalogue is written in Taipei time and a "daily" koan means a day there.
TZ = timezone(timedelta(hours=8))

DEFAULT_SOURCE = "禪宗公案選集"

# `1. 趙州狗子` -- a numbered Markdown list, which is how the catalogue is written.
ENTRY = re.compile(r"^\s*\d+\.\s+(.+?)\s*$")


def fail(detail, code=2):
    print(detail, file=sys.stderr)
    print(json.dumps({"selected": False, "error": detail}, ensure_ascii=False))
    sys.exit(code)


def load_catalog(path):
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        fail(f"catalogue could not be read: {exc}")
    topics = []
    seen = set()
    for line in lines:
        match = ENTRY.match(line)
        if not match:
            continue
        title = match.group(1).strip()
        # Deduplicated: a catalogue listing one koan twice would otherwise let it be chosen
        # twice per cycle, which is the one thing rotation exists to prevent.
        if title and title not in seen:
            seen.add(title)
            topics.append(title)
    return topics


def load_history(path):
    """Rotation state: which koans have been used this cycle, and on what date each was."""
    if not path.is_file():
        return {"cycle": [], "by_date": {}}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        # A corrupt history is not a reason to refuse to run, but it is a reason to say so:
        # starting a fresh cycle may repeat a koan, and that is worth knowing.
        print(f"history unreadable, starting a fresh cycle: {exc}", file=sys.stderr)
        return {"cycle": [], "by_date": {}}
    if not isinstance(data, dict):
        return {"cycle": [], "by_date": {}}
    cycle = data.get("cycle")
    by_date = data.get("by_date")
    return {
        "cycle": cycle if isinstance(cycle, list) else [],
        "by_date": by_date if isinstance(by_date, dict) else {},
    }


def require_path(payload, key):
    raw = payload.get(key)
    if not isinstance(raw, str) or not raw.strip():
        fail(f"'{key}' is required and must be a non-empty string")
    return Path(raw).expanduser()


def main():
    raw = os.environ.get("PEARL_INPUT", "") or sys.stdin.read()
    if not raw.strip():
        fail("no input provided")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        fail(f"input parse error: {exc}")
    if not isinstance(payload, dict):
        fail("input must be a JSON object")

    catalog_path = require_path(payload, "catalog")
    if not catalog_path.is_file():
        fail(f"'catalog' is not a file: {catalog_path}")
    history_path = require_path(payload, "history")

    source = payload.get("source", DEFAULT_SOURCE)
    if not isinstance(source, str) or not source.strip():
        fail("'source' must be a non-empty string when given")

    date = payload.get("date")
    if date is None:
        date = datetime.now(TZ).strftime("%Y-%m-%d")
    if not isinstance(date, str) or not re.match(r"^\d{4}-\d{2}-\d{2}$", date):
        fail(f"'date' must be YYYY-MM-DD, got {date!r}")

    seed = payload.get("seed")
    if seed is not None and (isinstance(seed, bool) or not isinstance(seed, int)):
        fail(f"'seed' must be an integer when given, got {seed!r}")

    catalog = load_catalog(catalog_path)
    if not catalog:
        fail(f"catalogue has no entries: {catalog_path}")

    history = load_history(history_path)

    # Already chosen today: return it unchanged. This is what makes a retry harmless.
    already = history["by_date"].get(date)
    if isinstance(already, str) and already:
        print(f"{date} already selected {already}; returning it", file=sys.stderr)
        print(
            json.dumps(
                {
                    "selected": True,
                    "topic": already,
                    "source": source,
                    "date": date,
                    "reused": True,
                    "cycle_used": len(history["cycle"]),
                    "cycle_total": len(catalog),
                },
                ensure_ascii=False,
            )
        )
        sys.exit(0)

    used = set(history["cycle"])
    candidates = [t for t in catalog if t not in used]
    cycle_reset = False
    if not candidates:
        # Every koan has been used; begin again. Reported rather than silent, because a reader
        # seeing a repeat should be able to tell it was a new cycle and not a rotation bug.
        cycle_reset = True
        history["cycle"] = []
        candidates = list(catalog)

    if seed is not None:
        random.Random(seed).shuffle(candidates)
    topic = candidates[0]

    history["cycle"].append(topic)
    history["by_date"][date] = topic

    try:
        history_path.parent.mkdir(parents=True, exist_ok=True)
        history_path.write_text(
            json.dumps(history, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    except OSError as exc:
        # Refuse rather than proceed. An unrecorded selection means tomorrow picks the same
        # koan, and a retry today picks a different one -- both of which break rotation.
        fail(f"rotation state could not be written, so the selection is not safe to use: {exc}")

    print(
        f"selected {topic} for {date} "
        f"({len(history['cycle'])}/{len(catalog)} used{', cycle reset' if cycle_reset else ''})",
        file=sys.stderr,
    )
    print(
        json.dumps(
            {
                "selected": True,
                "topic": topic,
                "source": source,
                "date": date,
                "reused": False,
                "cycle_reset": cycle_reset,
                "cycle_used": len(history["cycle"]),
                "cycle_total": len(catalog),
            },
            ensure_ascii=False,
        )
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
