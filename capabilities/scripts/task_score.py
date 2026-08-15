#!/usr/bin/env python3
"""task_score.py -- P0 mechanical scoring for `script.task-score`.

Article 1: the formula is fully specified, so there is nothing here for a model to
decide. The weights are the single source of truth for task scoring at framework level;
an application that needs different weights declares its own capability rather than
asking an agent to adjust these.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    priority                    int 1..4   (4 = highest)
    confidence_tier             str        tier1 | tier2 | tier3
    has_description             bool
    time_proximity              str        overdue | today | tomorrow | this_week | no_due
    label_count                 int >= 0
    same_label_completed_today  int >= 0
    hour                        int 0..23
    is_complex                  bool
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = scored
  2 = input error (malformed or out-of-domain input)
"""

import json
import os
import sys

PRIORITY_SCORES = {4: 4.0, 3: 3.0, 2: 2.0, 1: 1.0}
CONFIDENCE_MULTIPLIERS = {"tier1": 1.0, "tier2": 0.8, "tier3": 0.6}
DESCRIPTION_BONUS = {True: 1.2, False: 1.0}
TIME_PROXIMITY_BONUS = {
    "overdue": 1.5,
    "today": 1.3,
    "tomorrow": 1.1,
    "this_week": 1.0,
    "no_due": 0.9,
}
LABEL_COUNT_BONUS = {0: 1.0, 1: 1.05, 2: 1.1}
LABEL_COUNT_BONUS_MANY = 1.15
RECENCY_PENALTY_NONE = 1.0
RECENCY_PENALTY_SOME = 0.85
RECENCY_PENALTY_MANY = 0.7
FATIGUE_BANDS = (
    ((8, 12), 1.0),
    ((13, 16), 0.85),
    ((17, 20), 0.95),
    ((21, 23), 0.80),
    ((0, 7), 0.90),
)

FORMULA = (
    "priority * confidence * description * time_proximity "
    "* label_count * recency * fatigue"
)


def fail(message, code=2):
    """Emit a machine-readable failure and stop.

    Diagnostics go to stderr and the verdict to stdout, so a caller parsing stdout never
    has to separate prose from data (SS26).
    """
    print(message, file=sys.stderr)
    print(json.dumps({"error": message}))
    sys.exit(code)


def label_bonus(count):
    if count >= 3:
        return LABEL_COUNT_BONUS_MANY
    return LABEL_COUNT_BONUS[count]


def recency_penalty(overlap):
    if overlap <= 1:
        return RECENCY_PENALTY_NONE
    if overlap == 2:
        return RECENCY_PENALTY_SOME
    return RECENCY_PENALTY_MANY


def fatigue_weight(hour, is_complex):
    # Simple work is not affected by time of day; only complex work is discounted, so a
    # trivial task does not lose priority merely for being scheduled late.
    if not is_complex:
        return 1.0
    for (start, end), weight in FATIGUE_BANDS:
        if start <= hour <= end:
            return weight
    return 1.0


def read_payload():
    raw = os.environ.get("PEARL_INPUT", "")
    if not raw:
        raw = sys.stdin.read()
    if not raw.strip():
        fail("no input provided")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        fail(f"input parse error: {exc}")
    if not isinstance(payload, dict):
        fail("input must be a JSON object")
    return payload


def require_int(payload, key, default, low, high):
    value = payload.get(key, default)
    if isinstance(value, bool) or not isinstance(value, int):
        fail(f"'{key}' must be an integer, got {value!r}")
    if not low <= value <= high:
        fail(f"'{key}' must be between {low} and {high}, got {value}")
    return value


def require_bool(payload, key, default):
    value = payload.get(key, default)
    if not isinstance(value, bool):
        fail(f"'{key}' must be a boolean, got {value!r}")
    return value


def require_choice(payload, key, default, allowed):
    value = payload.get(key, default)
    if value not in allowed:
        fail(f"'{key}' must be one of {sorted(allowed)}, got {value!r}")
    return value


def main():
    payload = read_payload()

    priority = require_int(payload, "priority", 1, 1, 4)
    tier = require_choice(payload, "confidence_tier", "tier3", CONFIDENCE_MULTIPLIERS)
    has_description = require_bool(payload, "has_description", False)
    proximity = require_choice(payload, "time_proximity", "no_due", TIME_PROXIMITY_BONUS)
    labels = require_int(payload, "label_count", 0, 0, 1000)
    overlap = require_int(payload, "same_label_completed_today", 0, 0, 1000)
    hour = require_int(payload, "hour", 12, 0, 23)
    is_complex = require_bool(payload, "is_complex", False)

    breakdown = {
        "priority_score": PRIORITY_SCORES[priority],
        "confidence": CONFIDENCE_MULTIPLIERS[tier],
        "description_bonus": DESCRIPTION_BONUS[has_description],
        "time_proximity_bonus": TIME_PROXIMITY_BONUS[proximity],
        "label_count_bonus": label_bonus(labels),
        "recency_penalty": recency_penalty(overlap),
        "fatigue_weight": fatigue_weight(hour, is_complex),
    }

    score = 1.0
    for factor in breakdown.values():
        score *= factor
    # Rounding is part of the contract: an unrounded float would make two runs of the same
    # input differ in their last digit across platforms, breaking the determinism test.
    score = round(score, 4)

    print(f"score={score} from {len(breakdown)} factors", file=sys.stderr)
    print(json.dumps({"score": score, "breakdown": breakdown, "formula": FORMULA}))
    sys.exit(0)


if __name__ == "__main__":
    main()
