#!/usr/bin/env python3
"""zen_koan_assemble.py -- the mechanical shaping behind `script.zen-koan-assemble`.

Takes what the model wrote and turns it into the document that gets pushed: a heading, three
named sections, and a count of how much is actually in each.

Ported from DDP's `tools/zen_koan_assemble.py`. What came across is the normalisation and the
measurement. What did not is the result envelope: the original also carried `agent`,
`task_key`, `status`, `ntfy_pending`, `ntfy_sent`, `done_cert` and `quality_lint`, all of which
PEARL already owns and owns better --

  status, done_cert  -> the task's state and its verdict
  ntfy_pending/sent  -> a declared `effect.notify` step, with the ledger recording the effect
  quality_lint       -> the assurance steps the task declares

Carrying them here would mean two records of the same facts, and the one this script wrote was
the one nothing checked: `ntfy_sent: false` sitting in a result file is what let a missed
notification look like a successful run.

The measurement is the point of this step. A local model asked for three sections sometimes
returns one line, which is a successful generation and not a koan. Counting each section's
characters turns "did the model actually write it" into something a verifier can decide.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    topic           str  the koan's title (required)
    koan_markdown   str  what the model wrote (required)
    source          str  attribution (default 禪宗公案選集)
    min_section     int  characters a section needs to count as written (default 20)
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = assembled
  2 = nothing to assemble (missing or empty input)

Exit 0 does not mean the koan is good. `complete` says whether every section was written, and
the workflow's verify step is what insists on it -- the same separation Article 8 draws
between producing a result and judging one.
"""

import json
import os
import re
import sys

DEFAULT_SOURCE = "禪宗公案選集"

# The three sections a koan is written in: the story, the plain-language reading, and the
# one-line turn. Order matters: it is the order they are added in when missing.
SECTIONS = ("典故", "白話釋義", "禪門一語")

# What the original wrote into a section it had to invent. Kept verbatim so a half-written
# koan looks the same here as it did there, and so `complete` can recognise one.
PLACEHOLDER = "（待補）"

# How much each section needs before it counts as written.
#
# Per section rather than one number, because the sections are not the same kind of writing. A
# 禪門一語 is a single turned line and twelve characters of it is complete; twelve characters of
# 典故 is a model that gave up. A uniform floor gets one of those two wrong, and the first
# version of this port got it wrong in the direction that rejects good koans.
#
# The numbers are the lower bounds the prompt asks for (50-120, 50-100, 10-20 characters), so
# `complete` means "the model delivered what it was told to" rather than "the model produced
# something". Inventing floors independently of the prompt is how the first version of this
# port came to reject a real koan.
DEFAULT_MIN_SECTION = {"典故": 50, "白話釋義": 50, "禪門一語": 10}


def fail(detail, code=2):
    print(detail, file=sys.stderr)
    print(json.dumps({"assembled": False, "error": detail}, ensure_ascii=False))
    sys.exit(code)


def resolve_floors(given):
    """Per-section character floors, from an integer, a partial map, or the defaults.

    An integer applies to every section, which is what a caller means by "at least this much
    everywhere". A map overrides only the sections it names, so raising the bar on one section
    does not silently drop the others to zero.
    """
    floors = dict(DEFAULT_MIN_SECTION)
    if isinstance(given, bool):
        fail(f"'min_section' must be an integer or an object, got {given!r}")
    if isinstance(given, int):
        if given < 1:
            fail(f"'min_section' must be at least 1, got {given}")
        return {header: given for header in SECTIONS}
    if isinstance(given, dict):
        for header, value in given.items():
            if header not in SECTIONS:
                fail(
                    f"'min_section' names {header!r}, which is not a section; "
                    f"expected one of {list(SECTIONS)}"
                )
            if isinstance(value, bool) or not isinstance(value, int) or value < 1:
                fail(f"'min_section.{header}' must be an integer of at least 1, got {value!r}")
            floors[header] = value
        return floors
    fail(f"'min_section' must be an integer or an object, got {given!r}")


def normalize(topic, markdown):
    """Ensures the heading and all three sections are present.

    A missing section is added with a placeholder rather than dropped, so the shape is always
    the same and the gap is visible in the output instead of being inferred from its absence.
    """
    text = markdown.strip()
    if not text.startswith("##"):
        text = f"## {topic}\n\n{text}"
    for header in SECTIONS:
        if f"**{header}**" not in text:
            text += f"\n\n**{header}**\n{PLACEHOLDER}"
    return text.strip()


def section_body(markdown, header):
    """The text under one section heading, up to the next heading or the end."""
    match = re.search(
        rf"\*\*{re.escape(header)}\*\*\s*\n(.*?)(?:\n\*\*|\Z)", markdown, re.S
    )
    return match.group(1).strip() if match else ""


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

    topic = payload.get("topic")
    if not isinstance(topic, str) or not topic.strip():
        fail("'topic' is required and must be a non-empty string")
    markdown = payload.get("koan_markdown")
    if not isinstance(markdown, str) or not markdown.strip():
        fail("'koan_markdown' is required and must be a non-empty string")

    source = payload.get("source", DEFAULT_SOURCE)
    if not isinstance(source, str) or not source.strip():
        fail("'source' must be a non-empty string when given")

    floors = resolve_floors(payload.get("min_section", DEFAULT_MIN_SECTION))

    normalized = normalize(topic, markdown)

    bodies = {header: section_body(normalized, header) for header in SECTIONS}
    lengths = {header: len(body) for header, body in bodies.items()}
    # A section is written if it clears its own floor and is not the placeholder. Both checks
    # are needed: the placeholder is short, but a model can also emit something short and real.
    written = {
        header: body != PLACEHOLDER and len(body) >= floors[header]
        for header, body in bodies.items()
    }
    missing = [header for header, ok in written.items() if not ok]

    print(
        f"{topic}: "
        + ", ".join(f"{h} {lengths[h]}" for h in SECTIONS)
        + (f"; incomplete: {', '.join(missing)}" if missing else "; complete"),
        file=sys.stderr,
    )
    print(
        json.dumps(
            {
                "assembled": True,
                "topic": topic,
                "source": source,
                "title": f"禪宗公案｜{topic}",
                "koan_markdown": normalized,
                "sections": lengths,
                "complete": not missing,
                "missing_sections": missing,
                "min_section": floors,
            },
            ensure_ascii=False,
        )
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
