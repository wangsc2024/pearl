#!/usr/bin/env python3
"""notify.py -- the side effect behind `effect.notify`.

Publishes to an AgentFlow-Notify hub (https://github.com/wangsc2024/AgentFlow-Notify),
which fans one notification out to its own claim/ack queue plus any Discord / LINE /
webhook downstreams bound to the topic. PEARL talks to the hub and nothing else: the
last mile is the hub's problem, and it is the hub that knows which channels a topic
reaches.

Article 4: the output is evidence of what was *accepted*, not of what a human read.
The hub answers `202 {id}` when it has taken responsibility for the message; whether
Discord then delivered it lives in `GET /notifications/{id}` -> `deliveries[]`. So this
script reports `accepted`, never `delivered` -- the earlier ntfy version claimed
`delivered: true` for what was only ever an HTTP acknowledgement.

Article 5: retry safety is the *ledger's* job, not this script's. The hub performs no
deduplication of its own, so posting twice sends twice. `StateStore::request_effect`
is the only gate, and it is a gate with a real seam: if the POST succeeds and the
process dies before the effect is committed, a retry will duplicate. The idempotency
key therefore also travels as a tag, so a duplicate is recognisable by eye in the hub
UI rather than only in our ledger.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    title            str   notification title (required)
    message          str   body (required)
    idempotency_key  str   the key the ledger issued (required)
    topic            str   hub topic; defaults to $AGENTFLOW_NOTIFY_TOPIC
    priority         str   min | low | default | high | urgent
    tags             [str]
    task_result      str   success | failed | partial | running | other
  Environment:
    AGENTFLOW_NOTIFY_URL              hub base URL, e.g. https://x.onrender.com (required)
    AGENTFLOW_NOTIFY_TOKEN            publish token (required unless ALLOW_ANON)
    AGENTFLOW_NOTIFY_ALLOW_ANON       '1' to post without a token (local hub only)
    AGENTFLOW_NOTIFY_TOPIC            default topic
    AGENTFLOW_NOTIFY_TIMEOUT_SECONDS  default 120
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = the hub accepted the notification
  1 = the hub did not accept it (network, timeout, auth, 5xx)
  2 = input or configuration error

There is no "skip quietly when unconfigured" path. A side effect that silently does
nothing is indistinguishable from one that worked, and Article 4 does not allow that.
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request

# The hub's own vocabulary is 1-5. PEARL callers use names, so that a task spec does not
# have to encode another system's numbering -- and so that "urgent" keeps meaning urgent
# if the hub ever renumbers.
PRIORITIES = {"min": 1, "low": 2, "default": 3, "high": 4, "urgent": 5}

# Mirrors AgentFlow-Notify's `TaskResult`. Sent through so the hub UI shows what the task
# actually did, rather than only whether the message got out.
TASK_RESULTS = {"success", "failed", "partial", "running", "other"}

# The project's topic. A topic is deployment configuration, so the environment wins; this
# is the value PEARL uses when nothing says otherwise.
DEFAULT_TOPIC = "PEARL_kiro"

# The hub's Topic type: 1-64 characters of A-Za-z0-9, '-' or '_'. Checked here as well so
# a bad topic costs no network round trip and the error names the rule.
TOPIC_PATTERN = re.compile(r"^[A-Za-z0-9_-]{1,64}$")

# Render's free tier sleeps after ~15 idle minutes and cold-starts in 30-60s, so the first
# request of the day is slow rather than broken.
DEFAULT_TIMEOUT_SECONDS = 120


def fail(detail, code):
    print(detail, file=sys.stderr)
    print(json.dumps({"accepted": False, "error": detail}))
    sys.exit(code)


def require_text(payload, name):
    value = payload.get(name)
    if not isinstance(value, str) or not value.strip():
        fail(f"'{name}' is required and must be a non-empty string", 2)
    return value


def check_tag(tag):
    """The hub rejects empty tags, commas and control characters."""
    if not isinstance(tag, str) or not tag:
        fail("'tags' entries must be non-empty strings", 2)
    if "," in tag:
        fail(f"tag {tag!r} contains a comma, which the hub uses as a separator", 2)
    if any(c.isprintable() is False for c in tag):
        fail(f"tag {tag!r} contains a control character", 2)
    return tag


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

    title = require_text(payload, "title")
    message = require_text(payload, "message")
    key = require_text(payload, "idempotency_key")

    if any(not c.isprintable() and c not in " \t" for c in title):
        fail("'title' must not contain control characters", 2)

    base_url = (os.environ.get("AGENTFLOW_NOTIFY_URL") or "").rstrip("/")
    if not base_url:
        fail("AGENTFLOW_NOTIFY_URL is not set; the effect cannot be performed", 2)

    token = os.environ.get("AGENTFLOW_NOTIFY_TOKEN") or ""
    anon = os.environ.get("AGENTFLOW_NOTIFY_ALLOW_ANON") == "1"
    if not token and not anon:
        fail(
            "AGENTFLOW_NOTIFY_TOKEN is not set. Set it, or set "
            "AGENTFLOW_NOTIFY_ALLOW_ANON=1 when the hub runs without auth locally",
            2,
        )

    # An absent topic falls back; a topic that is *present and empty* does not. The two
    # are different mistakes: the first is "you did not choose", which the default answers,
    # and the second is "you chose nothing", which is a bug in the caller. Collapsing them
    # would publish to the default topic on behalf of a task that meant something else.
    if "topic" in payload:
        topic = payload["topic"]
        if not isinstance(topic, str) or not topic.strip():
            fail("'topic' was given but is empty; omit it to use the default", 2)
    else:
        topic = os.environ.get("AGENTFLOW_NOTIFY_TOPIC") or DEFAULT_TOPIC
    if not TOPIC_PATTERN.match(topic):
        fail(
            f"topic {topic!r} is not usable: the hub allows 1-64 characters of "
            "A-Za-z0-9, '-' or '_'",
            2,
        )

    name = payload.get("priority", "default")
    if name not in PRIORITIES:
        fail(f"'priority' must be one of {sorted(PRIORITIES)}, got {name!r}", 2)
    priority = PRIORITIES[name]

    tags = payload.get("tags") or []
    if not isinstance(tags, list):
        fail("'tags' must be a list of strings", 2)
    tags = [check_tag(t) for t in tags]
    # The key on the wire, so a duplicate is visible where the messages are, not only in
    # our ledger. The hub does not act on it; a human comparing two notifications can.
    tags.append(check_tag(f"idem:{key}"))

    task_result = payload.get("task_result")
    if task_result is not None and task_result not in TASK_RESULTS:
        fail(
            f"'task_result' must be one of {sorted(TASK_RESULTS)}, got {task_result!r}",
            2,
        )

    try:
        timeout = int(
            os.environ.get("AGENTFLOW_NOTIFY_TIMEOUT_SECONDS")
            or DEFAULT_TIMEOUT_SECONDS
        )
    except ValueError:
        fail("AGENTFLOW_NOTIFY_TIMEOUT_SECONDS must be an integer", 2)

    body = {
        "topic": topic,
        "title": title,
        "body": message,
        "priority": priority,
        "tags": tags,
    }
    if task_result is not None:
        body["task_result"] = task_result

    url = f"{base_url}/notifications"
    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    if token:
        request.add_header("Authorization", f"Bearer {token}")

    print(f"POST {url} topic={topic} priority={priority} (key={key})", file=sys.stderr)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            status = response.status
            raw_body = response.read(4096).decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        detail = exc.read(1024).decode("utf-8", errors="replace").strip()
        fail(f"hub rejected the notification with HTTP {exc.code}: {detail or exc.reason}", 1)
    except urllib.error.URLError as exc:
        fail(f"hub unreachable: {exc.reason}", 1)
    except TimeoutError:
        fail(
            f"hub did not answer within {timeout}s "
            "(a sleeping free-tier instance cold-starts in 30-60s)",
            1,
        )

    parsed = {}
    try:
        parsed = json.loads(raw_body)
    except json.JSONDecodeError:
        pass

    notification_id = parsed.get("id")
    if not notification_id:
        # A 2xx with no id means the hub took the request but told us nothing we can
        # follow up on, so there is no evidence to record. Article 4: report the gap.
        fail(f"hub answered HTTP {status} without a notification id: {raw_body[:200]}", 1)

    print(f"accepted as {notification_id} (HTTP {status})", file=sys.stderr)
    print(
        json.dumps(
            {
                "accepted": True,
                "notification_id": notification_id,
                "hub_status": parsed.get("status"),
                "http_status": status,
                "topic": topic,
                "priority": priority,
                "idempotency_key": key,
                # Where delivery -- as opposed to acceptance -- can be confirmed.
                "status_url": f"{base_url}/notifications/{notification_id}",
            }
        )
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
