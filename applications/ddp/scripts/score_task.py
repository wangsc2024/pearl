#!/usr/bin/env python3
"""score_task.py -- P0 mechanical script: implements the DDP scoring formula.

Based on config/scoring.yaml formula:
  score = priority_score * confidence * description_bonus * time_proximity
          * label_count_bonus * recency_penalty * time_fatigue_weight

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "priority": int (1-4, Todoist priority where 4=p1, 1=p4)
    - "confidence_tier": str ("tier1"|"tier2"|"tier3")
    - "has_description": bool
    - "time_proximity": str ("overdue"|"today"|"tomorrow"|"this_week"|"no_due")
    - "label_count": int
    - "same_label_completed_today": int (recency overlap count)
    - "hour": int (0-23, current hour for fatigue calculation)
    - "is_complex": bool (whether task matches complex labels)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = scoring successful
  2 = input error
"""

import json
import os
import sys


# Scoring parameters from config/scoring.yaml v3
PRIORITY_SCORES = {4: 4, 3: 3, 2: 2, 1: 1}

CONFIDENCE_MULTIPLIERS = {"tier1": 1.0, "tier2": 0.8, "tier3": 0.6}

DESCRIPTION_BONUS = {"has": 1.2, "none": 1.0}

TIME_PROXIMITY_BONUS = {
    "overdue": 1.5,
    "today": 1.3,
    "tomorrow": 1.1,
    "this_week": 1.0,
    "no_due": 0.9,
}

LABEL_COUNT_BONUS = {0: 1.0, 1: 1.05, 2: 1.1}  # 3+ -> 1.15

RECENCY_PENALTY = {"0_1": 1.0, "2": 0.85, "3_plus": 0.7}

TIME_FATIGUE_WEIGHTS = {
    (8, 12): 1.0,
    (13, 16): 0.85,
    (17, 20): 0.95,
    (21, 23): 0.80,
    (0, 7): 0.90,
}


def get_fatigue_weight(hour, is_complex):
    """Get time fatigue weight based on current hour."""
    if not is_complex:
        return 1.0  # Simple tasks bypass fatigue

    for (start, end), weight in TIME_FATIGUE_WEIGHTS.items():
        if start <= hour <= end:
            return weight
    return 1.0


def get_label_bonus(count):
    """Get label count bonus."""
    if count >= 3:
        return 1.15
    return LABEL_COUNT_BONUS.get(count, 1.0)


def get_recency_penalty(overlap_count):
    """Get recency penalty based on same-label completions today."""
    if overlap_count <= 1:
        return RECENCY_PENALTY["0_1"]
    elif overlap_count == 2:
        return RECENCY_PENALTY["2"]
    else:
        return RECENCY_PENALTY["3_plus"]


def main():
    raw_input = os.environ.get("PEARL_INPUT", "")
    if not raw_input:
        raw_input = sys.stdin.read()

    if not raw_input.strip():
        print("No input provided", file=sys.stderr)
        print(json.dumps({"error": "no input provided"}))
        sys.exit(2)

    try:
        payload = json.loads(raw_input)
    except json.JSONDecodeError as e:
        print(f"Failed to parse input: {e}", file=sys.stderr)
        print(json.dumps({"error": f"parse error: {str(e)}"}))
        sys.exit(2)

    # Extract fields with defaults.
    priority = payload.get("priority", 1)
    confidence_tier = payload.get("confidence_tier", "tier3")
    has_description = payload.get("has_description", False)
    time_proximity = payload.get("time_proximity", "no_due")
    label_count = payload.get("label_count", 0)
    same_label_completed = payload.get("same_label_completed_today", 0)
    hour = payload.get("hour", 12)
    is_complex = payload.get("is_complex", False)

    # Validate.
    if priority not in PRIORITY_SCORES:
        print(f"Invalid priority: {priority}", file=sys.stderr)
        print(json.dumps({"error": f"invalid priority: {priority}"}))
        sys.exit(2)

    if confidence_tier not in CONFIDENCE_MULTIPLIERS:
        print(f"Invalid confidence tier: {confidence_tier}", file=sys.stderr)
        print(json.dumps({"error": f"invalid confidence_tier: {confidence_tier}"}))
        sys.exit(2)

    if time_proximity not in TIME_PROXIMITY_BONUS:
        print(f"Invalid time_proximity: {time_proximity}", file=sys.stderr)
        print(json.dumps({"error": f"invalid time_proximity: {time_proximity}"}))
        sys.exit(2)

    # Calculate each factor.
    priority_score = PRIORITY_SCORES[priority]
    confidence = CONFIDENCE_MULTIPLIERS[confidence_tier]
    desc_bonus = DESCRIPTION_BONUS["has"] if has_description else DESCRIPTION_BONUS["none"]
    time_bonus = TIME_PROXIMITY_BONUS[time_proximity]
    label_bonus = get_label_bonus(label_count)
    recency = get_recency_penalty(same_label_completed)
    fatigue = get_fatigue_weight(hour, is_complex)

    # Composite score.
    score = priority_score * confidence * desc_bonus * time_bonus * label_bonus * recency * fatigue
    score = round(score, 4)

    # Build breakdown for transparency.
    breakdown = {
        "priority_score": priority_score,
        "confidence": confidence,
        "description_bonus": desc_bonus,
        "time_proximity_bonus": time_bonus,
        "label_count_bonus": label_bonus,
        "recency_penalty": recency,
        "fatigue_weight": fatigue,
    }

    print(f"Score calculated: {score}", file=sys.stderr)
    print(f"  Factors: {' * '.join(f'{k}={v}' for k, v in breakdown.items())}", file=sys.stderr)

    print(json.dumps({
        "score": score,
        "breakdown": breakdown,
        "formula": "priority * confidence * description * time_proximity * label_count * recency * fatigue"
    }))
    sys.exit(0)


if __name__ == "__main__":
    main()
