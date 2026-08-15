#!/usr/bin/env python3
"""news_collector.py -- P0 mechanical script: collects news items for the daily digest.

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "sources": list of news source identifiers (optional, defaults to ["hackernews", "arxiv"])
    - "max_items": maximum items to collect per source (default: 10)
    - "since_hours": only items from the last N hours (default: 24)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = collection successful
  1 = partial failure (some sources unavailable)
  2 = input error

Note: This is a stub that returns mock data. Complete with real API keys
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
        # Default parameters if no input provided.
        payload = {}
    else:
        try:
            payload = json.loads(raw_input)
        except json.JSONDecodeError as e:
            print(f"Failed to parse input: {e}", file=sys.stderr)
            print(json.dumps({"success": False, "error": f"parse error: {str(e)}"}))
            sys.exit(2)

    sources = payload.get("sources", ["hackernews", "arxiv"])
    max_items = payload.get("max_items", 10)
    since_hours = payload.get("since_hours", 24)

    print(f"Collecting news from {len(sources)} source(s), max {max_items} items each", file=sys.stderr)
    print(f"Window: last {since_hours} hours", file=sys.stderr)

    # Mock data -- in production, each source would hit a real API.
    collected_items = []
    for source in sources:
        items = _mock_items_for_source(source, max_items)
        collected_items.extend(items)
        print(f"  {source}: {len(items)} item(s) collected", file=sys.stderr)

    result = {
        "success": True,
        "sources_queried": sources,
        "total_items": len(collected_items),
        "items": collected_items,
        "collected_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0)


def _mock_items_for_source(source, max_items):
    """Generate mock news items for a source."""
    items = []
    for i in range(min(max_items, 3)):
        items.append({
            "source": source,
            "title": f"[MOCK] {source} item #{i+1}",
            "url": f"https://example.com/{source}/{i+1}",
            "published_at": datetime.now(timezone.utc).isoformat(),
            "relevance_score": 0.8 - (i * 0.1),
        })
    return items


if __name__ == "__main__":
    main()
