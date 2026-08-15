#!/usr/bin/env python3
"""gmail_collector.py -- P0 mechanical script: collects email summaries for the daily digest.

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "label": Gmail label to scan (default: "INBOX")
    - "max_messages": maximum messages to summarize (default: 20)
    - "since_hours": only messages from the last N hours (default: 24)
    - "categories": list of categories to include (default: ["primary", "updates"])
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = collection successful
  1 = authentication/connection error
  2 = input error

Note: This is a stub that returns mock data. Complete with Gmail OAuth
per docs/production-completion-guide.md.
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
        payload = {}
    else:
        try:
            payload = json.loads(raw_input)
        except json.JSONDecodeError as e:
            print(f"Failed to parse input: {e}", file=sys.stderr)
            print(json.dumps({"success": False, "error": f"parse error: {str(e)}"}))
            sys.exit(2)

    label = payload.get("label", "INBOX")
    max_messages = payload.get("max_messages", 20)
    since_hours = payload.get("since_hours", 24)
    categories = payload.get("categories", ["primary", "updates"])

    print(f"Scanning Gmail label '{label}' for last {since_hours}h", file=sys.stderr)
    print(f"Categories: {categories}, max: {max_messages}", file=sys.stderr)

    # Mock data -- in production, would use Gmail API with OAuth2.
    messages = _mock_messages(label, max_messages)
    print(f"  Found {len(messages)} message(s)", file=sys.stderr)

    result = {
        "success": True,
        "label": label,
        "total_messages": len(messages),
        "messages": messages,
        "categories_scanned": categories,
        "collected_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0)


def _mock_messages(label, max_messages):
    """Generate mock email messages."""
    messages = []
    for i in range(min(max_messages, 5)):
        messages.append({
            "id": f"msg_{i+1:04d}",
            "subject": f"[MOCK] Email message #{i+1}",
            "sender": f"sender{i+1}@example.com",
            "received_at": datetime.now(timezone.utc).isoformat(),
            "snippet": f"This is a mock email snippet for message {i+1}...",
            "category": "primary" if i % 2 == 0 else "updates",
            "is_read": i > 0,
        })
    return messages


if __name__ == "__main__":
    main()
