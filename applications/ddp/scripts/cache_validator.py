#!/usr/bin/env python3
"""cache_validator.py -- P0 mechanical script: checks if cache files are stale.

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "cache_dir": path to the cache directory to validate
    - "max_age_seconds": maximum allowed age in seconds (default: 3600)
    - "required_files": optional list of filenames that must exist
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = cache is valid (all files fresh and present)
  1 = cache is stale or missing required files
  2 = input error
"""

import json
import os
import sys
import time


def main():
    # Read input from PEARL_INPUT env var (pearl-runtime contract).
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

    cache_dir = payload.get("cache_dir")
    max_age_seconds = payload.get("max_age_seconds", 3600)
    required_files = payload.get("required_files", [])

    if not cache_dir:
        print("Missing 'cache_dir' in input", file=sys.stderr)
        print(json.dumps({"valid": False, "error": "missing 'cache_dir'"}))
        sys.exit(2)

    now = time.time()
    stale_files = []
    missing_files = []
    fresh_files = []

    # Check if directory exists.
    if not os.path.isdir(cache_dir):
        print(f"Cache directory does not exist: {cache_dir}", file=sys.stderr)
        print(json.dumps({
            "valid": False,
            "error": "cache directory not found",
            "cache_dir": cache_dir
        }))
        sys.exit(1)

    # Check required files.
    for filename in required_files:
        filepath = os.path.join(cache_dir, filename)
        if not os.path.exists(filepath):
            missing_files.append(filename)
            print(f"Required file missing: {filename}", file=sys.stderr)

    # Check file ages.
    for entry in os.listdir(cache_dir):
        filepath = os.path.join(cache_dir, entry)
        if not os.path.isfile(filepath):
            continue
        mtime = os.path.getmtime(filepath)
        age = now - mtime
        if age > max_age_seconds:
            stale_files.append({"file": entry, "age_seconds": round(age, 1)})
            print(f"Stale: {entry} (age: {age:.0f}s > {max_age_seconds}s)", file=sys.stderr)
        else:
            fresh_files.append(entry)

    is_valid = len(stale_files) == 0 and len(missing_files) == 0

    result = {
        "valid": is_valid,
        "cache_dir": cache_dir,
        "total_files": len(fresh_files) + len(stale_files),
        "fresh_count": len(fresh_files),
        "stale_count": len(stale_files),
        "missing_count": len(missing_files),
    }

    if stale_files:
        result["stale_files"] = stale_files
    if missing_files:
        result["missing_files"] = missing_files

    if is_valid:
        print(f"Cache valid: {len(fresh_files)} fresh file(s)", file=sys.stderr)
    else:
        print(f"Cache invalid: {len(stale_files)} stale, {len(missing_files)} missing", file=sys.stderr)

    print(json.dumps(result))
    sys.exit(0 if is_valid else 1)


if __name__ == "__main__":
    main()
