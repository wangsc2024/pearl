#!/usr/bin/env python3
"""todoist_scorer.py -- P0 mechanical script: deterministic task priority scoring.

Implements the mechanical scoring formula from 系統開發需求書 §64.

Score = (urgency_weight * urgency) + (importance_weight * importance) + (effort_penalty * effort_hours)

Where:
  urgency    = days_until_due <= 0 ? 100 : max(0, 100 - (days_until_due * 10))
  importance = priority_level * 25 (Todoist p1=100, p2=75, p3=50, p4=25)
  effort_penalty = -5 * estimated_hours (longer tasks score slightly lower)

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "tasks": list of task objects, each with:
      - "id": task identifier
      - "title": task title
      - "priority": 1-4 (Todoist priority, 1=highest)
      - "due_date": ISO date string (optional)
      - "estimated_hours": float (optional, default: 1.0)
    - "weights": optional weight overrides
      - "urgency": float (default: 0.4)
      - "importance": float (default: 0.5)
      - "effort": float (default: 0.1)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = scoring successful
  2 = input error
"""

import json
import os
import sys
from datetime import datetime, date, timezone


def main():
    raw_input = os.environ.get("PEARL_INPUT", "")
    if not raw_input:
        raw_input = sys.stdin.read()

    if not raw_input.strip():
        print("No input provided", file=sys.stderr)
        print(json.dumps({"success": False, "error": "no input provided"}))
        sys.exit(2)

    try:
        payload = json.loads(raw_input)
    except json.JSONDecodeError as e:
        print(f"Failed to parse input: {e}", file=sys.stderr)
        print(json.dumps({"success": False, "error": f"parse error: {str(e)}"}))
        sys.exit(2)

    tasks = payload.get("tasks", [])
    if not tasks:
        print("No tasks to score", file=sys.stderr)
        print(json.dumps({"success": True, "scored_tasks": [], "total": 0}))
        sys.exit(0)

    weights = payload.get("weights", {})
    urgency_weight = weights.get("urgency", 0.4)
    importance_weight = weights.get("importance", 0.5)
    effort_weight = weights.get("effort", 0.1)

    print(f"Scoring {len(tasks)} task(s) with weights: urgency={urgency_weight}, importance={importance_weight}, effort={effort_weight}", file=sys.stderr)

    today = date.today()
    scored_tasks = []

    for task in tasks:
        score = _compute_score(task, today, urgency_weight, importance_weight, effort_weight)
        scored_tasks.append(score)

    # Sort by score descending.
    scored_tasks.sort(key=lambda t: t["score"], reverse=True)

    # Assign rank.
    for i, task in enumerate(scored_tasks):
        task["rank"] = i + 1

    result = {
        "success": True,
        "scored_tasks": scored_tasks,
        "total": len(scored_tasks),
        "weights_used": {
            "urgency": urgency_weight,
            "importance": importance_weight,
            "effort": effort_weight,
        },
        "scored_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0)


def _compute_score(task, today, urgency_w, importance_w, effort_w):
    """Compute the deterministic priority score for a task."""
    task_id = task.get("id", "unknown")
    title = task.get("title", "")
    priority = task.get("priority", 4)
    due_date_str = task.get("due_date")
    estimated_hours = task.get("estimated_hours", 1.0)

    # Urgency: based on days until due.
    if due_date_str:
        try:
            due = date.fromisoformat(due_date_str)
            days_until_due = (due - today).days
            if days_until_due <= 0:
                urgency = 100.0
            else:
                urgency = max(0.0, 100.0 - (days_until_due * 10.0))
        except ValueError:
            urgency = 50.0  # Default for unparseable dates.
    else:
        urgency = 30.0  # No due date: moderate urgency.

    # Importance: from Todoist priority (p1=highest=100, p4=lowest=25).
    importance = (5 - min(max(priority, 1), 4)) * 25.0

    # Effort penalty: longer tasks score slightly lower.
    effort_penalty = -5.0 * max(0.0, estimated_hours)

    # Final score.
    score = (urgency_w * urgency) + (importance_w * importance) + (effort_w * effort_penalty)

    return {
        "id": task_id,
        "title": title,
        "score": round(score, 2),
        "components": {
            "urgency": round(urgency, 2),
            "importance": round(importance, 2),
            "effort_penalty": round(effort_penalty, 2),
        },
    }


if __name__ == "__main__":
    main()
