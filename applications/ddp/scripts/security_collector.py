#!/usr/bin/env python3
"""security_collector.py -- P0 mechanical script: collects security advisories for the daily digest.

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "feeds": list of advisory feed identifiers (default: ["github", "cve", "rust-sec"])
    - "severity_threshold": minimum severity to include (default: "medium")
    - "packages": list of package names to monitor (optional)
    - "since_hours": only advisories from the last N hours (default: 24)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = collection successful (may have zero advisories)
  1 = feed unavailable
  2 = input error

Note: This is a stub that returns mock data. Complete with real security feeds
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

    feeds = payload.get("feeds", ["github", "cve", "rust-sec"])
    severity_threshold = payload.get("severity_threshold", "medium")
    packages = payload.get("packages", [])
    since_hours = payload.get("since_hours", 24)

    print(f"Scanning {len(feeds)} security feed(s)", file=sys.stderr)
    print(f"Severity threshold: {severity_threshold}", file=sys.stderr)
    if packages:
        print(f"Monitoring packages: {packages}", file=sys.stderr)

    # Mock data -- in production, would query GitHub Advisory DB, NVD, RustSec.
    advisories = _mock_advisories(feeds, severity_threshold)
    print(f"  Found {len(advisories)} advisory(ies) above threshold", file=sys.stderr)

    result = {
        "success": True,
        "feeds_queried": feeds,
        "severity_threshold": severity_threshold,
        "total_advisories": len(advisories),
        "advisories": advisories,
        "collected_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0)


def _mock_advisories(feeds, severity_threshold):
    """Generate mock security advisories."""
    severity_levels = ["critical", "high", "medium", "low"]
    threshold_idx = severity_levels.index(severity_threshold) if severity_threshold in severity_levels else 2

    advisories = []
    for i, feed in enumerate(feeds):
        sev_idx = i % len(severity_levels)
        if sev_idx <= threshold_idx:
            advisories.append({
                "id": f"ADV-2024-{i+1:04d}",
                "feed": feed,
                "title": f"[MOCK] Security advisory from {feed}",
                "severity": severity_levels[sev_idx],
                "affected_package": f"example-pkg-{i+1}",
                "published_at": datetime.now(timezone.utc).isoformat(),
                "url": f"https://example.com/advisory/{i+1}",
            })
    return advisories


if __name__ == "__main__":
    main()
