#!/usr/bin/env python3
"""verify_digest.py -- Verifier: checks digest output structure.

Validates that a daily digest output conforms to the expected structure:
- Has required top-level keys
- Contains task entries with required fields
- Score values are within valid ranges
- Timestamps are parseable

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "digest": the digest object to verify
    - "strict": bool (if true, require all optional fields too)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = digest is valid
  1 = digest has structural errors
  2 = input error
"""

import json
import os
import sys
from datetime import datetime


REQUIRED_DIGEST_KEYS = ["date", "tasks", "metadata"]
REQUIRED_TASK_KEYS = ["task_id", "title", "score", "priority"]
OPTIONAL_TASK_KEYS = ["description", "labels", "due_date", "skill_match", "breakdown"]
REQUIRED_METADATA_KEYS = ["generated_at", "total_tasks", "version"]


def validate_digest(digest, strict=False):
    """Validate digest structure. Returns list of errors."""
    errors = []

    if not isinstance(digest, dict):
        return ["digest must be a JSON object"]

    # Check top-level keys.
    for key in REQUIRED_DIGEST_KEYS:
        if key not in digest:
            errors.append(f"missing required key: {key}")

    if errors:
        return errors

    # Validate date.
    date_str = digest.get("date", "")
    if date_str:
        try:
            datetime.strptime(date_str, "%Y-%m-%d")
        except ValueError:
            errors.append(f"invalid date format: '{date_str}' (expected YYYY-MM-DD)")

    # Validate tasks array.
    tasks = digest.get("tasks", [])
    if not isinstance(tasks, list):
        errors.append("'tasks' must be an array")
    else:
        for i, task in enumerate(tasks):
            if not isinstance(task, dict):
                errors.append(f"tasks[{i}]: must be an object")
                continue

            for key in REQUIRED_TASK_KEYS:
                if key not in task:
                    errors.append(f"tasks[{i}]: missing required key '{key}'")

            if strict:
                for key in OPTIONAL_TASK_KEYS:
                    if key not in task:
                        errors.append(f"tasks[{i}]: missing optional key '{key}' (strict mode)")

            # Validate score range.
            score = task.get("score")
            if score is not None:
                if not isinstance(score, (int, float)):
                    errors.append(f"tasks[{i}]: score must be a number, got {type(score).__name__}")
                elif score < 0:
                    errors.append(f"tasks[{i}]: score must be non-negative, got {score}")

            # Validate priority range.
            priority = task.get("priority")
            if priority is not None:
                if not isinstance(priority, int) or priority < 1 or priority > 4:
                    errors.append(f"tasks[{i}]: priority must be 1-4, got {priority}")

    # Validate metadata.
    metadata = digest.get("metadata", {})
    if not isinstance(metadata, dict):
        errors.append("'metadata' must be an object")
    else:
        for key in REQUIRED_METADATA_KEYS:
            if key not in metadata:
                errors.append(f"metadata: missing required key '{key}'")

        total = metadata.get("total_tasks")
        if total is not None and isinstance(tasks, list):
            if total != len(tasks):
                errors.append(
                    f"metadata.total_tasks ({total}) does not match actual task count ({len(tasks)})"
                )

    return errors


def main():
    raw_input = os.environ.get("PEARL_INPUT", "")
    if not raw_input:
        raw_input = sys.stdin.read()

    if not raw_input.strip():
        print("No input provided", file=sys.stderr)
        print(json.dumps({"valid": False, "error": "no input provided"}))
        sys.exit(2)

    try:
        payload = json.loads(raw_input)
    except json.JSONDecodeError as e:
        print(f"Failed to parse input: {e}", file=sys.stderr)
        print(json.dumps({"valid": False, "error": f"parse error: {str(e)}"}))
        sys.exit(2)

    digest = payload.get("digest")
    strict = payload.get("strict", False)

    if digest is None:
        print("Missing 'digest' field in input", file=sys.stderr)
        print(json.dumps({"valid": False, "error": "missing 'digest' field"}))
        sys.exit(2)

    errors = validate_digest(digest, strict=strict)

    if errors:
        print(f"Digest validation failed: {len(errors)} error(s)", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        print(json.dumps({"valid": False, "errors": errors, "error_count": len(errors)}))
        sys.exit(1)

    task_count = len(digest.get("tasks", []))
    print(f"Digest valid: {task_count} task(s)", file=sys.stderr)
    print(json.dumps({"valid": True, "task_count": task_count}))
    sys.exit(0)


if __name__ == "__main__":
    main()
