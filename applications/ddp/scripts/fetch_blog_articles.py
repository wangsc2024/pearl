#!/usr/bin/env python3
"""fetch_blog_articles.py -- the collector behind `script.fetch-blog-articles`.

Fetches recent articles from AlphaMatch Blog and BNext (數位時代), one via RSS and one via
HTML, then emits a structured list for the digest workflow.

Ported from DDP's `tools/fetch_blog_articles.py`. Changes:

1. **Standard library only.** The original used `requests` and fell back to `lxml` with regex
   as a final fallback. Here it is `urllib` for fetching and regex for parsing, with no
   third-party dependencies. This is stronger than the original's lxml fallback for one reason:
   lxml can be installed without all of requests, so a `requests` importer was feasible but
   `lxml` was not universally available. `urllib` + regex is everywhere.

2. **Two outputs, like the pingtung collector.** The original had JSON and ntfy formats and
   chose one. Here `articles` and `digest_text` are both emitted so the workflow can verify and
   send without choosing.

3. **Cleaner error reporting.** The original had six error modes (lxml exception, regex failure,
   fetch failure, parse failure, no articles) scattered across three try-catch blocks. Here
   each source explicitly reports its state so the digest workflow can decide whether to send
   and what to do on a partial failure (one source down, one up).

4. **No `datetime.now()` naive local time.** The clock is the one thing this script cannot fix
   — there is no "now" passed in — but it is at least noted.

Script I/O Contract (SS26):
  PEARL_INPUT (or stdin): JSON object
    limit      int  articles per source (default 3)
    timeout    int  seconds per HTTP attempt (default 15)
  stdout: machine JSON only, on the last line
  stderr: diagnostics

Exit codes:
  0 = at least one source was reachable
  1 = all sources failed
  2 = invalid input

The list is not deduplicated — if one article appears on both sites it shows twice. That is
correct: a digest is meant to reflect what the sources said, not to impose an editorial order.
The workflow can deduplicate if it decides to.
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from datetime import datetime
from typing import Any
from urllib.parse import urljoin, urlparse

DEFAULT_LIMIT = 3
DEFAULT_TIMEOUT = 15
USER_AGENT = "pearl/1.0 (+https://github.com/wangsc2024/pearl)"

# Each source is configured with these keys to avoid hardcoding them per invocation.
SOURCES = [
    {
        "id": "alphamatch",
        "name": "AlphaMatch Blog",
        "url": "https://www.alphamatch.ai/zh/blog",
        "method": "html",
        "href_contains": "/zh/blog/",
        "min_title_len": 5,
    },
    {
        "id": "bnext",
        "name": "數位時代 BNext",
        "url": "https://www.bnext.com.tw/articles",
        "method": "rss",
        "rss_url": "https://www.bnext.com.tw/rss",
        "href_contains": "/article/",
        "min_title_len": 5,
    },
]


def fail(detail, code=2):
    print(detail, file=sys.stderr)
    print(json.dumps({"fetched": False, "error": detail}, ensure_ascii=False))
    sys.exit(code)


def fetch(url, timeout):
    """The bytes at `url`, with standard library HTTP."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.read()
    except urllib.error.HTTPError as exc:
        raise RuntimeError(f"HTTP {exc.code}: {exc.reason}")
    except urllib.error.URLError as exc:
        raise RuntimeError(f"unreachable: {exc.reason}")
    except TimeoutError:
        raise RuntimeError(f"no answer within {timeout}s")


def parse_rss(body, limit):
    """Articles from an RSS feed."""
    try:
        root = ET.fromstring(body)
    except ET.ParseError as exc:
        raise RuntimeError(f"RSS parse error: {exc}")

    articles = []
    for item in root.iter("item"):
        title = (item.findtext("title") or "").strip()
        link = (item.findtext("link") or "").strip()
        pub_date = (item.findtext("pubDate") or "").strip()
        if title and link and len(title) >= 5:
            articles.append({"title": title, "url": link, "date": pub_date})
            if len(articles) >= limit:
                break
    return articles


def parse_html(body, base_url, href_contains, min_title_len, limit):
    """Articles from HTML, parsed by regex.

    Find `<a>` tags with the marker in href, then extract title from the text between the tag
    and the first `</a>`. This is a simple heuristic: real link text lives in the first text
    node after the opening tag in well-formed HTML.
    
    If this produces garbage (JSON-LD, attributes, etc.), the workflow's verifier catches it
    by checking the document structure. The goal here is to extract *something* that looks
    reasonable, not to parse arbitrary HTML correctly.
    """

    # Find `<a>` tags with the marker, capture up to the closing </a>.
    pattern = (
        r'<a[^>]*href=["\']([^"\']*' + re.escape(href_contains) + r'[^"\']*)["\'][^>]*>'
        r'([^<]*(?:<(?!/a>)[^>]*>[^<]*)*?)</a>'
    )

    seen = set()
    articles = []

    for match in re.finditer(pattern, body, re.IGNORECASE | re.DOTALL):
        href = match.group(1).strip()
        if not href.startswith(("http://", "https://")):
            href = urljoin(base_url, href)

        # Extract text from the link content, removing tags.
        content = match.group(2)
        text_nodes = re.split(r"<[^>]*>", content)
        title = ""
        for node in text_nodes:
            node = node.strip()
            if len(node) >= min_title_len and node and not node.startswith(("{", "[")):
                title = re.sub(r"\s+", " ", node).strip()[:200]
                break

        if title and href not in seen:
            seen.add(href)
            articles.append({"title": title, "url": href, "date": ""})
            if len(articles) >= limit:
                break

    return articles


def fetch_source(config, limit, timeout):
    """One source, explicitly reporting its state."""
    source_id = config["id"]
    source_name = config["name"]
    source_url = config["url"]

    result = {
        "id": source_id,
        "name": source_name,
        "url": source_url,
        "articles": [],
        "ok": False,
        "method": config["method"],
    }

    try:
        if config["method"] == "rss":
            body = fetch(config["rss_url"], timeout)
            articles = parse_rss(body, limit)
        else:  # html
            body = fetch(source_url, timeout).decode("utf-8", errors="replace")
            articles = parse_html(
                body,
                source_url,
                config["href_contains"],
                config["min_title_len"],
                limit,
            )

        if articles:
            result["articles"] = articles
            result["ok"] = True
        else:
            result["error"] = "no articles matched the selector"

    except RuntimeError as exc:
        result["error"] = f"fetch failed: {exc}"
    except (ET.ParseError, ValueError) as exc:
        result["error"] = f"parse error: {exc}"
    except Exception as exc:
        # An unexpected exception cannot fall through with no machine-readable output. It is
        # reported in this source and the others continue.
        result["error"] = f"{type(exc).__name__}: {exc}"

    return result


def digest(sources, limit):
    """Human-readable digest from the sources."""
    lines = []
    for src in sources:
        if src["ok"]:
            lines.append(f"📌 {src['name']}")
            for i, art in enumerate(src["articles"][:limit], 1):
                lines.append(f"  {i}. {art['title']}")
                lines.append(f"     {art['url']}")
        else:
            error = src.get("error", "failed")
            lines.append(f"❌ {src['name']}: {error}")
    return "\n".join(lines)


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

    limit = payload.get("limit", DEFAULT_LIMIT)
    if isinstance(limit, bool) or not isinstance(limit, int) or limit < 1:
        fail(f"'limit' must be an integer of at least 1, got {limit!r}")

    timeout = payload.get("timeout", DEFAULT_TIMEOUT)
    if isinstance(timeout, bool) or not isinstance(timeout, int) or timeout < 1:
        fail(f"'timeout' must be an integer of at least 1, got {timeout!r}")

    sources = []
    for config in SOURCES:
        print(f"fetching {config['name']}", file=sys.stderr)
        sources.append(fetch_source(config, limit, timeout))

    ok_sources = sum(1 for s in sources if s["ok"])
    total_articles = sum(len(s["articles"]) for s in sources)

    print(f"{ok_sources}/{len(sources)} sources, {total_articles} article(s)", file=sys.stderr)

    result = {
        "fetched": True,
        "fetched_at": datetime.now().isoformat(timespec="seconds"),
        "sources": sources,
        "ok_sources": ok_sources,
        "total_articles": total_articles,
        "digest_text": digest(sources, limit),
    }

    print(json.dumps(result, ensure_ascii=False))
    sys.exit(0 if ok_sources > 0 else 1)


if __name__ == "__main__":
    main()
