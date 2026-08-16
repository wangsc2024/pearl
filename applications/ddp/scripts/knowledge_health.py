#!/usr/bin/env python3
"""knowledge_health.py -- the mechanical scan behind `script.knowledge-health`.

Article 1: this is arithmetic over file timestamps and a Markdown index. There is nothing
here for a model to decide, so routing it to one would be a violation rather than a choice.

Ported from DDP's `tools/knowledge_health.py`. Three things changed on the way in, each
because PEARL already owns the concern the original had to solve for itself:

1. **The tree is an input, not a constant.** The original hardcoded its own repository root,
   which made the scan a fact about one checkout. Here the caller names the directory, so the
   same capability can scan any knowledge tree and a task spec says which.

2. **No state file is written.** The original wrote `state/knowledge-health.json` because that
   was how the next task read the result. In PEARL the *output* is how the next step reads it
   (`input_from: steps.scan.output`), and the ledger keeps it. A second copy on disk would be
   a second truth with no one responsible for reconciling them (Article 10).

3. **No freshness gate.** The original skipped if the report was under 7 days old, because
   DDP's scheduler fired opportunistically many times a day. PEARL's scheduler states its
   cadence, so "weekly" belongs in the schedule and not in the script. A scan that decides
   for itself whether to run cannot be asked to run.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    context_dir        str   directory of *.json knowledge files (required)
    skills_index       str   a Markdown index naming skills/<name>/SKILL.md (optional)
    skills_dir         str   where those skill directories live; defaults beside the index
    stale_days         int   age at which a file is stale (default 7)
    critical_days      int   age at which a file is critical (default 30)
    min_health_score   num   the bar this tree is expected to clear (optional, 0..1)
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = scanned
  2 = the scan could not be performed (bad input, unreadable directory)

Note what exit 0 does *not* mean: a tree can be scanned successfully and be in poor health.
The verdict is `healthy`, derived from `min_health_score` when the caller declares one, and a
task asserts it with `verifier.task-result` (`expect: {healthy: true}`). Failing the script on
a low score would conflate "the scan broke" with "the news is bad", and Article 2 keeps those
apart for the same reason it keeps a broken verifier apart from a failed check.
"""

import json
import os
import re
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

# The original's timezone. Ages are whole days, so the offset only matters at a boundary --
# but it matters there, and a scan that reported different numbers depending on where it ran
# would not be the deterministic capability this claims to be.
TZ = timezone(timedelta(hours=8))

DEFAULT_STALE_DAYS = 7
DEFAULT_CRITICAL_DAYS = 30

# How much each missing skill costs. Preserved from the original: a missing skill directory is
# a smaller problem than a stale file, but it is not free.
MISSING_SKILL_PENALTY = 0.05

# An age for a file whose timestamp cannot be read at all. Large enough to be classified
# critical by any sane threshold, and recognisable in output as "we could not tell".
UNKNOWN_AGE_DAYS = 999

SKILL_REFERENCE = re.compile(r"skills/([a-zA-Z0-9_-]+)/SKILL\.md")


def fail(detail, code=2):
    print(detail, file=sys.stderr)
    print(json.dumps({"scanned": False, "error": detail}, ensure_ascii=False))
    sys.exit(code)


def now():
    return datetime.now(TZ)


def file_age_days(path, reference):
    """A file's age in whole days.

    `generated_at` wins over the filesystem: a file copied or checked out has a fresh mtime
    and stale contents, and it is the contents this is asking about. mtime is the fallback
    for files that do not declare when they were made.
    """
    try:
        if path.suffix == ".json":
            data = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                declared = data.get("generated_at", "")
                if declared:
                    moment = datetime.fromisoformat(declared)
                    if moment.tzinfo is None:
                        moment = moment.replace(tzinfo=TZ)
                    return (reference - moment).days
    except (json.JSONDecodeError, OSError, ValueError):
        # An unparseable file still has an age; the scan reports the file rather than
        # abandoning the tree because one entry is malformed.
        pass

    try:
        mtime = datetime.fromtimestamp(path.stat().st_mtime, tz=TZ)
        return (reference - mtime).days
    except OSError:
        return UNKNOWN_AGE_DAYS


def severity_of(age, stale_days, critical_days):
    if age >= critical_days:
        return "critical"
    if age >= stale_days:
        return "stale"
    return "ok"


def scan_context(context_dir, root, stale_days, critical_days):
    reference = now()
    files = []
    for path in sorted(context_dir.glob("*.json")):
        age = file_age_days(path, reference)
        files.append(
            {
                "path": str(path.relative_to(root)).replace("\\", "/"),
                "age_days": age,
                "severity": severity_of(age, stale_days, critical_days),
            }
        )
    return files


def missing_skills(skills_index, skills_dir):
    """Skills the index promises and the filesystem does not have."""
    if skills_index is None or not skills_index.is_file():
        return []
    try:
        content = skills_index.read_text(encoding="utf-8")
    except OSError as exc:
        print(f"skills index unreadable, treating as empty: {exc}", file=sys.stderr)
        return []
    named = [m.group(1) for m in SKILL_REFERENCE.finditer(content)]
    # Deduplicated but order-preserving: an index may reference one skill from several
    # sections, and that is one missing skill, not three.
    seen = set()
    missing = []
    for name in named:
        if name in seen:
            continue
        seen.add(name)
        if not (skills_dir / name).exists():
            missing.append(name)
    return missing


def health_score(files, missing):
    """The share of files that are not stale, less a penalty per missing skill.

    An empty tree scores 1.0. That is the original's behaviour and it is defensible: nothing
    stale is nothing stale. It is also why `total_context_files` is reported alongside -- a
    perfect score over zero files is a fact about the input, and the number is right there
    for a reader to notice.
    """
    total = len(files) or 1
    unhealthy = sum(1 for f in files if f["severity"] in ("stale", "critical"))
    score = round(1.0 - unhealthy / total, 2)
    if missing:
        score = max(0.0, score - MISSING_SKILL_PENALTY * len(missing))
    return round(score, 2)


def require_directory(payload, key, required):
    raw = payload.get(key)
    if raw is None:
        if required:
            fail(f"'{key}' is required")
        return None
    if not isinstance(raw, str) or not raw.strip():
        fail(f"'{key}' must be a non-empty string")
    path = Path(raw).expanduser()
    if not path.is_dir():
        fail(f"'{key}' is not a directory: {path}")
    return path


def positive_int(payload, key, default):
    value = payload.get(key, default)
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        fail(f"'{key}' must be an integer of at least 1, got {value!r}")
    return value


def main():
    raw = os.environ.get("PEARL_INPUT", "") or sys.stdin.read()
    if not raw.strip():
        fail("no input provided")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        fail(f"input parse error: {exc}")
    if not isinstance(payload, dict):
        fail("input must be a JSON object")

    context_dir = require_directory(payload, "context_dir", required=True)
    stale_days = positive_int(payload, "stale_days", DEFAULT_STALE_DAYS)
    critical_days = positive_int(payload, "critical_days", DEFAULT_CRITICAL_DAYS)
    if critical_days < stale_days:
        fail(
            f"'critical_days' ({critical_days}) must be at least 'stale_days' ({stale_days}); "
            "otherwise no file could ever be merely stale"
        )

    index_raw = payload.get("skills_index")
    skills_index = None
    if index_raw is not None:
        if not isinstance(index_raw, str) or not index_raw.strip():
            fail("'skills_index' must be a non-empty string when given")
        skills_index = Path(index_raw).expanduser()
        if not skills_index.is_file():
            fail(f"'skills_index' is not a file: {skills_index}")
    skills_dir = require_directory(payload, "skills_dir", required=False)
    if skills_dir is None and skills_index is not None:
        skills_dir = skills_index.parent

    bar = payload.get("min_health_score")
    if bar is not None:
        if isinstance(bar, bool) or not isinstance(bar, (int, float)):
            fail(f"'min_health_score' must be a number, got {bar!r}")
        if not 0.0 <= float(bar) <= 1.0:
            fail(f"'min_health_score' must be between 0 and 1, got {bar}")

    # Paths are reported relative to the scanned tree, so the output does not leak the
    # absolute layout of the machine that ran it.
    root = context_dir.parent
    files = scan_context(context_dir, root, stale_days, critical_days)
    missing = missing_skills(skills_index, skills_dir) if skills_index else []
    score = health_score(files, missing)

    stale_count = sum(1 for f in files if f["severity"] == "stale")
    critical_count = sum(1 for f in files if f["severity"] == "critical")

    report = {
        "scanned": True,
        "generated_at": now().isoformat(),
        "health_score": score,
        "healthy": True if bar is None else score >= float(bar),
        "min_health_score": bar,
        "total_context_files": len(files),
        "stale_count": stale_count,
        "critical_count": critical_count,
        "missing_skills_count": len(missing),
        "missing_skills": missing,
        # Only the files worth acting on. The full listing would make the output grow without
        # bound in the size of the tree, and every entry of it would say "ok".
        "attention": [f for f in files if f["severity"] != "ok"],
        "thresholds": {"stale_days": stale_days, "critical_days": critical_days},
    }

    print(
        f"health {score} over {len(files)} file(s): "
        f"{stale_count} stale, {critical_count} critical, {len(missing)} missing skill(s)",
        file=sys.stderr,
    )
    print(json.dumps(report, ensure_ascii=False))
    sys.exit(0)


if __name__ == "__main__":
    main()
