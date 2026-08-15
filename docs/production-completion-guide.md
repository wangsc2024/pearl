# Production Completion Guide

This document describes how to complete the three sections that require external production
credentials or API access. Each section has working stub implementations that return mock
data; this guide explains what is needed to connect them to real services.

---

## 1. SS63 - Daily Digest: Real API Integration

**Current state:** Stub collector scripts exist at `applications/ddp/scripts/` that return
mock data in the correct Script I/O Contract format.

### What you need

| Collector | API Key / Token | Environment Variable |
|-----------|----------------|---------------------|
| `news_collector.py` | News API key | `NEWS_API_KEY` |
| `gmail_collector.py` | Gmail OAuth2 credentials | `GMAIL_OAUTH_JSON` (path to credentials.json) |
| `security_collector.py` | GitHub PAT (advisory access) | `GITHUB_TOKEN` |

### Steps to complete

1. **News collector** (`news_collector.py`):
   - Register at https://newsapi.org/ and obtain an API key
   - Set `NEWS_API_KEY` in the environment
   - Replace `_mock_items_for_source()` with actual HTTP calls to `/v2/top-headlines`
   - Add `requests` to the script dependencies
   - Keep the same output JSON schema (the downstream digest consumer expects it)

2. **Gmail collector** (`gmail_collector.py`):
   - Create a Google Cloud project and enable the Gmail API
   - Create OAuth2 credentials (Desktop app type)
   - Run the initial auth flow to generate `token.json`
   - Set `GMAIL_OAUTH_JSON` pointing to `credentials.json`
   - Replace `_mock_messages()` with Gmail API calls using `google-api-python-client`
   - Scope needed: `https://www.googleapis.com/auth/gmail.readonly`
   - The script must handle token refresh automatically

3. **Security collector** (`security_collector.py`):
   - Ensure `GITHUB_TOKEN` has `read:advisory` scope
   - Replace `_mock_advisories()` with GraphQL queries to GitHub Advisory Database:
     ```graphql
     query {
       securityAdvisories(first: 20, orderBy: {field: PUBLISHED_AT, direction: DESC}) {
         nodes { ghsaId severity summary publishedAt }
       }
     }
     ```
   - Optionally integrate NVD API (https://nvd.nist.gov/developers/vulnerabilities)
   - Optionally integrate RustSec advisory-db (local git clone of https://github.com/rustsec/advisory-db)

### Todoist integration for digest

- Obtain a Todoist API token from https://todoist.com/app/settings/integrations/developer
- Set `TODOIST_API_TOKEN` in the environment
- Update `todoist_scorer.py` to fetch real tasks via `https://api.todoist.com/rest/v2/tasks`
- The scorer and router scripts already implement the deterministic formula; only the
  data source changes

### Verification

After connecting real APIs, verify each collector by running:
```bash
PEARL_INPUT='{}' python3 applications/ddp/scripts/news_collector.py
```
The output should be valid JSON with real data items. Exit code 0 confirms success.

---

## 2. SS64 - Todoist Integration: Full Task Lifecycle

**Current state:** `todoist_scorer.py` and `todoist_router.py` implement the deterministic
scoring formula and routing thresholds. They operate on task data passed via PEARL_INPUT.

### What you need

- Todoist API token (REST API v2)
- Optionally: a Todoist webhook endpoint for real-time updates

### Steps to complete

1. **API token setup**:
   - Get token from https://todoist.com/app/settings/integrations/developer
   - Set `TODOIST_API_TOKEN` environment variable

2. **Task fetching**:
   - Create `todoist_fetcher.py` that calls `GET https://api.todoist.com/rest/v2/tasks`
   - Map Todoist fields to the scorer input format:
     - `priority` (1-4, where 1 is p4 in Todoist API, invert: API priority 4 = highest)
     - `due.date` -> `due_date`
     - `duration.amount` -> `estimated_hours` (if available)
   - Pipe the output to `todoist_scorer.py`

3. **Webhook setup** (optional, for real-time routing):
   - Set up an HTTPS endpoint that Todoist can POST events to
   - Subscribe to `item:added`, `item:updated`, `item:completed` events
   - On each event, re-score and re-route the affected task

4. **Task completion lifecycle**:
   - After a task is routed to "immediate" and executed, mark it complete:
     ```
     POST https://api.todoist.com/rest/v2/tasks/{id}/close
     ```
   - Record the completion as evidence in pearl-evidence

5. **End-to-end pipeline**:
   ```bash
   # Fetch -> Score -> Route -> Execute
   python3 todoist_fetcher.py | python3 todoist_scorer.py | python3 todoist_router.py
   ```

### Verification

```bash
# Test with real API
export TODOIST_API_TOKEN="your-token-here"
python3 todoist_fetcher.py | python3 todoist_scorer.py
```
Verify that real tasks appear with meaningful scores based on their due dates and priorities.

---

## 3. SS67 - State Consolidation: Migrating 904 State Files

**Current state:** The system uses SQLite-backed event-sourced state via pearl-state.
Category separation is defined by marker traits (StateData, MemoryData, CacheData,
ArtifactData, EvidenceData) in `pearl-state/src/records.rs`.

### Strategy for migration

The 904 state files referenced in the spec are JSON/YAML files that predate the
event-sourced architecture. Migration proceeds category by category:

#### Phase 1: Classify existing files

Run the audit script to categorize all state files:
```bash
PEARL_INPUT='{"project_root": "."}' python3 applications/ddp/scripts/audit_facts.py
```

Manually classify each file into one of five categories:
- **State**: task records, run records, configuration applied state
- **Memory**: temporary computation results, in-flight request tracking
- **Cache**: derived indexes, lookup tables, computed schedules
- **Artifact**: build outputs, generated reports, collected data
- **Evidence**: test results, verification reports, hash proofs

#### Phase 2: Migrate State files to SQLite

For each file classified as State:
1. Parse the file content
2. Create a corresponding event (TaskCreated, RunStarted, etc.)
3. Append to the event ledger via `StateStore`
4. Verify the projection matches the original file content
5. Archive the original file (do not delete until verified)

```rust
// Example migration for a task state file
let submission = TaskSubmission {
    task_id: TaskId::parse(file_content.id)?,
    task_type: file_content.task_type,
    precision_class: infer_precision(&file_content),
    quality: infer_quality(&file_content),
};
store.create_task(submission, file_content.created_at)?;
```

#### Phase 3: Migrate Artifacts to content-addressed store

For each file classified as Artifact:
1. Compute SHA-256 of the file content
2. Store in `pearl-evidence` EvidenceStore (content-addressed)
3. Record the mapping: original path -> digest
4. Verify retrieval by digest matches original content

#### Phase 4: Migrate Evidence

For each file classified as Evidence:
1. Parse the evidence data
2. Store via `pearl-evidence::EvidenceStore::store()`
3. Link to the corresponding task via task_id

#### Phase 5: Discard Cache and Memory

- **Cache files**: simply delete after verifying the system can reconstruct them
  (e.g., via `StateStore::rebuild_from_ledger()`)
- **Memory files**: these should not exist on disk at all in the new architecture;
  delete after confirming nothing depends on their presence at startup

### Migration script template

```python
#!/usr/bin/env python3
"""migrate_state_files.py -- one-time migration of legacy state files."""

import json
import os
import hashlib
from pathlib import Path

STATE_DIR = Path(".")
ARCHIVE_DIR = Path(".archive")

def classify_file(path):
    """Classify a state file into a category."""
    content = path.read_text()
    if "task_id" in content and "state" in content:
        return "state"
    if "evidence" in content or "verification" in content:
        return "evidence"
    if "output" in content or "artifact" in content:
        return "artifact"
    if "cache" in content or "index" in content:
        return "cache"
    return "memory"

def migrate():
    categories = {"state": [], "evidence": [], "artifact": [], "cache": [], "memory": []}
    
    for state_file in STATE_DIR.rglob("*.json"):
        category = classify_file(state_file)
        categories[category].append(state_file)
    
    print(f"Classification complete:")
    for cat, files in categories.items():
        print(f"  {cat}: {len(files)} files")
    
    # ... proceed with category-specific migration

if __name__ == "__main__":
    migrate()
```

### Verification

After migration:
1. Run `pearl event replay` to rebuild all projections
2. Compare task counts: `pearl task list | wc -l` should match pre-migration count
3. Run the full test suite: `cargo test`
4. Verify no state files remain in the legacy locations
5. Archive originals for 30 days before permanent deletion

### Rollback plan

Keep the original state files in `.archive/` for 30 days. If issues are discovered:
1. Stop the daemon: signal the AtomicBool stop handle
2. Restore files from `.archive/` to their original locations
3. Drop and recreate the SQLite database
4. Re-run migration after fixing the issue
