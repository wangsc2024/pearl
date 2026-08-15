#!/usr/bin/env python3
"""todoist_router.py -- P0 mechanical script: deterministic task routing.

Implements the routing formula from 系統開發需求書 §64.

Given a scored task list, routes each task to the appropriate execution queue:
  - score >= 80: "immediate" queue (execute now)
  - score >= 50: "scheduled" queue (execute within working hours)
  - score >= 20: "batch" queue (execute in next batch window)
  - score < 20: "backlog" queue (defer until explicitly requested)

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "scored_tasks": list of scored task objects (output from todoist_scorer.py)
    - "thresholds": optional routing threshold overrides
      - "immediate": float (default: 80)
      - "scheduled": float (default: 50)
      - "batch": float (default: 20)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = routing successful
  2 = input error
"""

import json
import os
import sys
from datetime import datetime, timezone


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

    scored_tasks = payload.get("scored_tasks", [])
    if not scored_tasks:
        print("No tasks to route", file=sys.stderr)
        print(json.dumps({"success": True, "routed": {}, "total": 0}))
        sys.exit(0)

    thresholds = payload.get("thresholds", {})
    immediate_threshold = thresholds.get("immediate", 80.0)
    scheduled_threshold = thresholds.get("scheduled", 50.0)
    batch_threshold = thresholds.get("batch", 20.0)

    print(f"Routing {len(scored_tasks)} task(s)", file=sys.stderr)
    print(f"Thresholds: immediate>={immediate_threshold}, scheduled>={scheduled_threshold}, batch>={batch_threshold}", file=sys.stderr)

    routed = {
        "immediate": [],
        "scheduled": [],
        "batch": [],
        "backlog": [],
    }

    for task in scored_tasks:
        score = task.get("score", 0)
        queue = _route_to_queue(score, immediate_threshold, scheduled_threshold, batch_threshold)
        routed[queue].append({
            "id": task.get("id"),
            "title": task.get("title", ""),
            "score": score,
            "queue": queue,
        })

    for queue, items in routed.items():
        if items:
            print(f"  {queue}: {len(items)} task(s)", file=sys.stderr)

    result = {
        "success": True,
        "routed": routed,
        "total": len(scored_tasks),
        "counts": {q: len(items) for q, items in routed.items()},
        "thresholds_used": {
            "immediate": immediate_threshold,
            "scheduled": scheduled_threshold,
            "batch": batch_threshold,
        },
        "routed_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0)


def _route_to_queue(score, immediate, scheduled, batch):
    """Deterministic routing based on score thresholds."""
    if score >= immediate:
        return "immediate"
    elif score >= scheduled:
        return "scheduled"
    elif score >= batch:
        return "batch"
    else:
        return "backlog"


if __name__ == "__main__":
    main()
