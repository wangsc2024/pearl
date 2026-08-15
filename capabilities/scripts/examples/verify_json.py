#!/usr/bin/env python3
"""verify_json.py -- P0 mechanical verifier that validates JSON structure.

Script I/O Contract (SS26):
  stdin:  JSON object with keys:
    - "data": the JSON value to validate
    - "schema": optional dict describing expected keys and types
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = valid
  1 = invalid (schema mismatch)
  2 = input error (malformed input)
"""

import json
import os
import sys


def main():
    # Read input from PEARL_INPUT env var (pearl-runtime contract).
    raw_input = os.environ.get("PEARL_INPUT", "")
    if not raw_input:
        # Fall back to stdin if PEARL_INPUT is not set.
        raw_input = sys.stdin.read()

    if not raw_input.strip():
        print("No input provided", file=sys.stderr)
        print(json.dumps({"valid": False, "error": "no input provided"}))
        sys.exit(2)

    try:
        payload = json.loads(raw_input)
    except json.JSONDecodeError as e:
        print(f"Failed to parse input JSON: {e}", file=sys.stderr)
        print(json.dumps({"valid": False, "error": f"input parse error: {str(e)}"}))
        sys.exit(2)

    data = payload.get("data")
    schema = payload.get("schema")

    if data is None:
        print("Missing 'data' field in input", file=sys.stderr)
        print(json.dumps({"valid": False, "error": "missing 'data' field"}))
        sys.exit(2)

    # If no schema provided, just check that data is valid JSON (it already is).
    if schema is None:
        print("No schema provided, data is valid JSON", file=sys.stderr)
        print(json.dumps({"valid": True, "keys": list(data.keys()) if isinstance(data, dict) else None}))
        sys.exit(0)

    # Validate against simple schema: check required keys and types.
    errors = []
    required_keys = schema.get("required_keys", [])
    type_checks = schema.get("types", {})

    if isinstance(data, dict):
        for key in required_keys:
            if key not in data:
                errors.append(f"missing required key: {key}")

        for key, expected_type in type_checks.items():
            if key in data:
                actual_type = type(data[key]).__name__
                # Normalize type names.
                type_map = {"str": "string", "int": "number", "float": "number", "bool": "boolean", "list": "array", "dict": "object"}
                normalized = type_map.get(actual_type, actual_type)
                if normalized != expected_type:
                    errors.append(f"key '{key}' expected {expected_type}, got {normalized}")
    else:
        if required_keys:
            errors.append("data is not an object but schema requires keys")

    if errors:
        print(f"Validation failed: {len(errors)} error(s)", file=sys.stderr)
        print(json.dumps({"valid": False, "errors": errors}))
        sys.exit(1)

    print("Validation passed", file=sys.stderr)
    print(json.dumps({"valid": True}))
    sys.exit(0)


if __name__ == "__main__":
    main()
