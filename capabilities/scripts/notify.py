#!/usr/bin/env python3
"""notify.py -- the side effect behind `effect.notify`.

Article 5: this script performs a real, externally visible action, so it must be safe to
retry. Deduplication itself lives in the ledger (`StateStore::request_effect`), not here —
a script cannot know whether an earlier attempt already committed. What this script does
guarantee is that the idempotency key it was given travels with the notification, so the
receiving side can also recognise a repeat.

Article 4: the output carries the evidence of what was sent, including the response status,
so "notification sent" is a claim with something behind it.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    title            str  notification title (required)
    message          str  body (required)
    idempotency_key  str  the key the ledger issued (required)
    topic            str  ntfy topic; defaults to $NTFY_TOPIC
    priority         str  min | low | default | high | urgent
    tags             [str]
  Environment:
    NTFY_BASE_URL    e.g. https://ntfy.sh   (required)
    NTFY_TOPIC       default topic          (required unless `topic` is given)
    NTFY_TOKEN       optional bearer token
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = delivered
  1 = delivery failed
  2 = input or configuration error

Note there is no "skip quietly when unconfigured" path. A side effect that silently does
nothing is indistinguishable from one that worked, and Article 4 does not allow that.
"""

import json
import os
import sys
import urllib.error
import urllib.request

VALID_PRIORITIES = {"min", "low", "default", "high", "urgent"}
TIMEOUT_SECONDS = 20


def fail(detail, code):
    print(detail, file=sys.stderr)
    print(json.dumps({"delivered": False, "error": detail}))
    sys.exit(code)


def main():
    raw = os.environ.get("PEARL_INPUT", "") or sys.stdin.read()
    if not raw.strip():
        fail("no input provided", 2)

    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        fail(f"input parse error: {exc}", 2)
    if not isinstance(payload, dict):
        fail("input must be a JSON object", 2)

    title = payload.get("title")
    message = payload.get("message")
    key = payload.get("idempotency_key")

    for name, value in (("title", title), ("message", message), ("idempotency_key", key)):
        if not isinstance(value, str) or not value.strip():
            fail(f"'{name}' is required and must be a non-empty string", 2)

    base_url = (os.environ.get("NTFY_BASE_URL") or "").rstrip("/")
    topic = payload.get("topic") or os.environ.get("NTFY_TOPIC") or ""
    if not base_url:
        fail("NTFY_BASE_URL is not set; the effect cannot be performed", 2)
    if not topic:
        fail("no topic given and NTFY_TOPIC is not set", 2)

    priority = payload.get("priority", "default")
    if priority not in VALID_PRIORITIES:
        fail(f"'priority' must be one of {sorted(VALID_PRIORITIES)}, got {priority!r}", 2)

    tags = payload.get("tags") or []
    if not isinstance(tags, list) or any(not isinstance(t, str) for t in tags):
        fail("'tags' must be a list of strings", 2)

    url = f"{base_url}/{topic}"
    request = urllib.request.Request(url, data=message.encode("utf-8"), method="POST")
    request.add_header("Title", title)
    request.add_header("Priority", priority)
    if tags:
        request.add_header("Tags", ",".join(tags))
    # The key goes on the wire so a duplicate is recognisable at the receiving end too,
    # not only in our own ledger.
    request.add_header("X-Pearl-Idempotency-Key", key)
    token = os.environ.get("NTFY_TOKEN")
    if token:
        request.add_header("Authorization", f"Bearer {token}")

    print(f"POST {url} (key={key})", file=sys.stderr)
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            status = response.status
            body = response.read(2048).decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        fail(f"notification rejected with HTTP {exc.code}: {exc.reason}", 1)
    except urllib.error.URLError as exc:
        fail(f"notification could not be delivered: {exc.reason}", 1)
    except TimeoutError:
        fail(f"notification timed out after {TIMEOUT_SECONDS}s", 1)

    print(f"delivered with HTTP {status}", file=sys.stderr)
    print(
        json.dumps(
            {
                "delivered": True,
                "http_status": status,
                "url": url,
                "idempotency_key": key,
                "response": body.strip()[:512],
            }
        )
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
