#!/usr/bin/env python3
"""verify_task_result.py -- the Machine Verifier for `verifier.task-result`.

Article 8: only a verifier may declare that verification passed. This script is therefore
deliberately unforgiving — it reports what it could actually check and refuses to infer
anything it could not.

What it checks, all mechanically:
  1. the result is a JSON object
  2. every key named in `require_keys` is present and not null
  3. every key named in `non_empty` is present and not empty
  4. every declared type in `types` matches
  5. `expect` equality constraints hold
  6. the result does not itself claim failure (`status`, `ok`, `valid`, `error`)

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    result        object    the document under verification (required)
    require_keys  [str]     keys that must be present and non-null
    non_empty     [str]     keys that must be present and non-empty
    types         {k: t}    t in string|number|boolean|array|object
    expect        {k: v}    exact expected values
  stdout: a verification-result-v1 document on the last line
  stderr: diagnostics

Exit codes:
  0 = pass
  1 = fail (checks ran and something did not hold)
  2 = input error (verification could not be performed at all)

Note the difference between 1 and 2: a failed check is a verdict, an input error is the
absence of one. Article 2 forbids treating the second as success.
"""

import json
import os
import sys
import time

TYPE_NAMES = {
    "str": "string",
    "int": "number",
    "float": "number",
    "bool": "boolean",
    "list": "array",
    "dict": "object",
    "NoneType": "null",
}

FAILURE_MARKERS = {
    "status": {"fail", "failed", "error", "false"},
    "outcome": {"fail", "failed", "error"},
}


VERIFIER_ID = "verifier.task-result"
STARTED_AT = time.monotonic()


def elapsed_ms():
    return int((time.monotonic() - STARTED_AT) * 1000)


def emit(status, checks, code):
    """Write a verification-result-v1 document and exit.

    The shape is fixed by schemas/verification-result-v1.json, including the requirement
    that at least one check be reported: an empty check list cannot establish anything, so
    the schema refuses to represent one.
    """
    print(
        json.dumps(
            {
                "status": status,
                "verifier": VERIFIER_ID,
                "checks": checks,
                "duration_ms": elapsed_ms(),
            }
        )
    )
    sys.exit(code)


def input_error(detail):
    print(f"input error: {detail}", file=sys.stderr)
    # status "error", never "fail": the verifier reached no verdict, and the schema keeps
    # those two distinguishable so a caller cannot read one as the other.
    emit("error", [{"id": "input", "status": "fail", "detail": detail}], 2)


def check(checks, check_id, ok, detail=None):
    entry = {"id": check_id, "status": "pass" if ok else "fail"}
    if detail:
        entry["detail"] = detail
    checks.append(entry)
    return ok


def type_of(value):
    return TYPE_NAMES.get(type(value).__name__, type(value).__name__)


def is_empty(value):
    if value is None:
        return True
    if isinstance(value, (str, list, dict, tuple, set)):
        return len(value) == 0
    return False


def main():
    raw = os.environ.get("PEARL_INPUT", "") or sys.stdin.read()
    if not raw.strip():
        input_error("no input provided")

    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        input_error(f"could not parse input: {exc}")

    if not isinstance(payload, dict):
        input_error("input must be a JSON object")
    if "result" not in payload:
        input_error("input is missing the 'result' document to verify")

    result = payload["result"]
    checks = []

    if not check(checks, "result_is_object", isinstance(result, dict),
                 None if isinstance(result, dict) else f"result is {type_of(result)}"):
        emit("fail", checks, 1)

    ok = True

    for key in payload.get("require_keys", []) or []:
        present = key in result and result[key] is not None
        ok &= check(checks, f"require_key:{key}", present,
                    None if present else f"'{key}' is missing or null")

    for key in payload.get("non_empty", []) or []:
        filled = key in result and not is_empty(result[key])
        ok &= check(checks, f"non_empty:{key}", filled,
                    None if filled else f"'{key}' is missing or empty")

    for key, expected in (payload.get("types") or {}).items():
        if key not in result:
            ok &= check(checks, f"type:{key}", False, f"'{key}' is missing")
            continue
        actual = type_of(result[key])
        matches = actual == expected
        ok &= check(checks, f"type:{key}", matches,
                    None if matches else f"'{key}' expected {expected}, got {actual}")

    for key, expected in (payload.get("expect") or {}).items():
        actual = result.get(key)
        matches = actual == expected
        ok &= check(checks, f"expect:{key}", matches,
                    None if matches else f"'{key}' expected {expected!r}, got {actual!r}")

    # A result that reports its own failure must not be verified as a success, even if
    # every declared key is present. This is the case that makes "no checks configured"
    # safe rather than vacuous.
    for key, bad_values in FAILURE_MARKERS.items():
        if isinstance(result.get(key), str) and result[key].lower() in bad_values:
            ok &= check(checks, f"self_reported:{key}", False,
                        f"result declares {key}={result[key]!r}")
    for key in ("ok", "valid", "success"):
        if result.get(key) is False:
            ok &= check(checks, f"self_reported:{key}", False,
                        f"result declares {key}=false")
    if not is_empty(result.get("error")):
        ok &= check(checks, "self_reported:error", False,
                    f"result carries error={result['error']!r}")
    if not is_empty(result.get("errors")):
        ok &= check(checks, "self_reported:errors", False,
                    f"result carries {len(result['errors'])} error(s)")

    print(
        f"{sum(1 for c in checks if c['status'] == 'pass')}/{len(checks)} checks passed",
        file=sys.stderr,
    )
    emit("pass" if ok else "fail", checks, 0 if ok else 1)


if __name__ == "__main__":
    main()
