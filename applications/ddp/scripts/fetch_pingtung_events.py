#!/usr/bin/env python3
"""fetch_pingtung_events.py -- the collector behind `script.fetch-pingtung-events`.

Fetches Pingtung County's public event calendar (RSS 2.0), parses the structured fields the
county packs into each item's `description`, keeps what has not finished yet, and formats a
digest body.

Ported from DDP's `tools/fetch_pthg_calendar.py`. Three changes:

1. **Standard library only.** The original used `requests`. `urllib.request` does the same job
   here, and a capability with no third-party dependency runs wherever Python does -- which
   matters because the worker spawns it directly rather than through a project environment.

2. **Times are `+08:00`, not naive local.** The original compared naive `datetime.now()`
   against naive parsed times, which is correct only while the machine sits in Taiwan. The
   calendar is published in Taiwan time, so that is what the parse says explicitly.

3. **One shape, both consumers.** The original had `--format json|summary` and the schedule
   used `summary` as the notification body. Here the output carries `events` *and*
   `digest_text`: the verify step needs the structured list to check, and the push step needs
   the prose. A capability that could only emit one of them forced the workflow to choose
   between verifying and sending.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    url        str  RSS endpoint (default the county's)
    days       int  only events starting within this many days (optional)
    limit      int  at most this many events (optional)
    district   str  keep only this 活動地區, substring match (optional)
    retries    int  network attempts before giving up (default 3)
    timeout    int  seconds per attempt (default 15)
    now        str  ISO 8601 instant to treat as now (optional; for tests)
    header     str  first line of the digest (default 屏東近期活動)
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = fetched and parsed
  1 = the source could not be reached or read -- a retry may succeed
  2 = the request was malformed -- a retry will not help

Exit 0 with an empty `events` list is a successful fetch of nothing, which is a real outcome
for a calendar. Whether that is worth sending is the workflow's decision, not this script's:
`ddp.pingtung-event-digest` asserts `non_empty: [events]` so a quiet week does not become a
daily notification saying so.
"""

import html
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta, timezone

DEFAULT_URL = "https://calendar.pthg.gov.tw/Home/RSS"
DEFAULT_RETRIES = 3
DEFAULT_TIMEOUT_SECONDS = 15
RETRY_BACKOFF_SECONDS = 2.0
# Conventional form: product/version plus a URL. Not decoration -- the county's WAF closes the
# connection on a User-Agent whose parenthetical contains space-separated path-like tokens, and
# it does so by dropping the socket rather than answering, so the symptom is
# `RemoteDisconnected` on every attempt and nothing that looks like a rejection. Verified: this
# string and DDP's original both fetch; `pearl/1.0 (+applications/ddp script.fetch-...)` never
# does.
USER_AGENT = "pearl/1.0 (+https://github.com/wangsc2024/pearl)"
DEFAULT_HEADER = "屏東近期活動"

# The calendar is published in Taiwan time and its item times carry no offset.
TZ = timezone(timedelta(hours=8))

# The county packs these into `description`, one per line, with a full-width colon (U+FF1A).
LABELS = ("活動時間", "主辦單位", "活動地區", "活動地點", "活動內容")

# `2026/01/17 星期六 14:30` -- the weekday character is present and carries no information the
# date does not, so it is matched and discarded.
EVENT_TIME = re.compile(r"(\d{4})/(\d{1,2})/(\d{1,2})\s*星期.\s*(\d{1,2}):(\d{2})")

TAG = re.compile(r"<[^>]+>")
WHITESPACE = re.compile(r"\s+")


def fail(detail, code=2):
    print(detail, file=sys.stderr)
    print(json.dumps({"fetched": False, "error": detail}, ensure_ascii=False))
    sys.exit(code)


def text_of(element):
    if element is None or element.text is None:
        return ""
    return element.text.strip()


def parse_description(description):
    """Splits `description` into its labelled fields.

    Sliced by position rather than by line, and each label is located by its *first*
    occurrence only: `活動內容` is HTML written by a human, and it can quite legitimately
    contain the words `活動地點`. Matching every occurrence would let the content cut itself
    into pieces.
    """
    fields = {label: "" for label in LABELS}
    if not description:
        return fields
    found = []
    for label in LABELS:
        index = description.find(label + "：")
        if index != -1:
            found.append((index, label))
    found.sort()
    for position, (index, label) in enumerate(found):
        start = index + len(label) + 1  # past the label and its colon
        end = found[position + 1][0] if position + 1 < len(found) else len(description)
        fields[label] = description[start:end].strip()
    return fields


def parse_event_time(value):
    """`(start, end)` from the 活動時間 field, or `(None, None)`.

    A single time means a moment rather than a span, so `end` stays `None` and callers treat
    the start as the end. Anything past the second time is ignored: the field is written by
    hand and a third timestamp has no defined meaning.
    """
    if not value:
        return None, None
    matches = EVENT_TIME.findall(value)

    def moment(match):
        year, month, day, hour, minute = (int(part) for part in match)
        try:
            return datetime(year, month, day, hour, minute, tzinfo=TZ)
        except ValueError:
            # A malformed date (13th month, 32nd day) is one bad item, not a bad feed.
            return None

    start = moment(matches[0]) if matches else None
    end = moment(matches[1]) if len(matches) > 1 else None
    return start, end


def strip_html(raw):
    """Tags out, entities decoded, whitespace collapsed."""
    if not raw:
        return ""
    return WHITESPACE.sub(" ", html.unescape(TAG.sub(" ", raw))).strip()


def build_event(item):
    fields = parse_description(text_of(item.find("description")))
    start, end = parse_event_time(fields["活動時間"])
    return {
        "title": text_of(item.find("title")),
        "link": text_of(item.find("link")),
        "guid": text_of(item.find("guid")),
        "start": start.isoformat() if start else None,
        "end": end.isoformat() if end else None,
        "organizer": fields["主辦單位"],
        "district": fields["活動地區"],
        "venue": fields["活動地點"],
        "content": strip_html(fields["活動內容"]),
        "pub_date": text_of(item.find("pubDate")),
    }


def fetch(url, retries, timeout):
    """The feed's bytes, retrying only what a retry could fix.

    The county's site returns 521 from time to time, so a transport failure is worth another
    attempt. A parse failure is not: the same bytes will not parse differently, and retrying
    would turn a broken feed into a slow failure.
    """
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    last = ""
    for attempt in range(1, retries + 1):
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.read()
        except urllib.error.HTTPError as exc:
            last = f"HTTP {exc.code}: {exc.reason}"
        except urllib.error.URLError as exc:
            last = f"unreachable: {exc.reason}"
        except TimeoutError:
            last = f"no answer within {timeout}s"
        # `urlopen` does not wrap everything in `URLError`. The county's site closes the
        # connection outright often enough that `http.client.RemoteDisconnected` escaped the
        # handlers above and became a traceback -- which means no machine-readable line on
        # stdout at all, so the worker saw a crash instead of a failed fetch. Anything the
        # transport can raise is a transport failure.
        except (http.client.HTTPException, OSError) as exc:
            last = f"{type(exc).__name__}: {exc}"
        if attempt < retries:
            print(f"attempt {attempt}/{retries} failed ({last}); retrying", file=sys.stderr)
            time.sleep(RETRY_BACKOFF_SECONDS)
    fail(f"{url} could not be read after {retries} attempt(s): {last}", 1)


def instant(value):
    try:
        moment = datetime.fromisoformat(value)
    except ValueError:
        fail(f"'now' must be an ISO 8601 instant, got {value!r}")
    return moment if moment.tzinfo else moment.replace(tzinfo=TZ)


def keep_upcoming(events, now, days, limit):
    """Events that have not finished, soonest first."""
    horizon = now + timedelta(days=days) if days is not None else None
    kept = []
    for event in events:
        if not event["start"]:
            # No parseable time means it cannot be placed on a calendar, and a digest entry
            # reading "（時間未定）" every day is noise. Counted in `undated` instead.
            continue
        start = datetime.fromisoformat(event["start"])
        end = datetime.fromisoformat(event["end"]) if event["end"] else start
        if end < now:
            continue
        if horizon is not None and start > horizon:
            continue
        kept.append(event)
    kept.sort(key=lambda e: e["start"])
    return kept[:limit] if limit else kept


def when(event):
    start = datetime.fromisoformat(event["start"])
    end = datetime.fromisoformat(event["end"]) if event["end"] else None
    shown = start.strftime("%m/%d %H:%M")
    if end and end.date() != start.date():
        shown += end.strftime("~%m/%d %H:%M")
    elif end:
        shown += end.strftime("~%H:%M")
    return shown


def digest(events, header):
    if not events:
        return f"{header}：目前查無活動"
    lines = [f"{header}（{len(events)} 場）"]
    for event in events:
        place = event.get("venue") or event.get("district") or ""
        line = f"• {when(event)} {event['title']}"
        if place:
            line += f"｜{place}"
        lines.append(line)
    return "\n".join(lines)


def positive_int(payload, key, default):
    value = payload.get(key, default)
    if value is None:
        return None
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

    url = payload.get("url", DEFAULT_URL)
    if not isinstance(url, str) or not url.strip():
        fail("'url' must be a non-empty string when given")
    if not url.startswith(("http://", "https://")):
        fail(f"'url' must be an http(s) URL, got {url!r}")

    days = positive_int(payload, "days", None)
    limit = positive_int(payload, "limit", None)
    retries = positive_int(payload, "retries", DEFAULT_RETRIES)
    timeout = positive_int(payload, "timeout", DEFAULT_TIMEOUT_SECONDS)

    district = payload.get("district")
    if district is not None and (not isinstance(district, str) or not district.strip()):
        fail("'district' must be a non-empty string when given")

    header = payload.get("header", DEFAULT_HEADER)
    if not isinstance(header, str) or not header.strip():
        fail("'header' must be a non-empty string when given")

    now = instant(payload["now"]) if "now" in payload else datetime.now(TZ)

    print(f"GET {url}", file=sys.stderr)
    body = fetch(url, retries, timeout)
    try:
        root = ET.fromstring(body)
    except ET.ParseError as exc:
        # Exit 1, not 2: the request was fine and the source was not, which is the same class
        # of problem as being unreachable even though a retry is unlikely to help.
        fail(f"the feed is not parseable XML: {exc}", 1)

    parsed = [build_event(item) for item in root.iter("item")]
    # An item with no title cannot be shown to anyone, so it is dropped rather than rendered
    # as a blank bullet.
    titled = [event for event in parsed if event["title"]]
    undated = sum(1 for event in titled if not event["start"])

    events = keep_upcoming(titled, now, days, limit)
    if district:
        events = [e for e in events if district in (e.get("district") or "")]
        header = f"{header}（{district}）"

    print(
        f"{len(parsed)} item(s), {len(titled)} titled, {undated} undated, "
        f"{len(events)} upcoming",
        file=sys.stderr,
    )
    print(
        json.dumps(
            {
                "fetched": True,
                "fetched_at": now.isoformat(timespec="seconds"),
                "source": url,
                "count": len(events),
                "events": events,
                "digest_text": digest(events, header),
                # Reported rather than dropped silently: a feed that suddenly has many
                # undated items has changed its format, and this is where that shows.
                "items_seen": len(parsed),
                "items_undated": undated,
            },
            ensure_ascii=False,
        )
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
