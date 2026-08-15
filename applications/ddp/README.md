# DDP - Daily Digest Pipeline

The Daily Digest Pipeline application built on the PEARL constitutional kernel.

## Migration Phases

### Phase 1: Task Specification
- Define task specs in YAML for daily-digest, todoist-ingress, and system-audit workflows
- Declare capability manifests for each workflow
- No runtime execution yet -- framework-level declarations only

### Phase 2: Workflow Integration
- Wire task specs into the pearl-workflow engine
- Implement capability adapters for each tool
- Add assurance checks for output quality

### Phase 3: Full Execution
- Connect to external services (Todoist, news APIs, notification channels)
- Enable checkpoint/resume for crash recovery
- Activate policy engine for side-effect governance

## Directory Structure

```
applications/ddp/
  tasks/               # YAML task specifications
    daily-digest.yaml
    todoist-ingress.yaml
    system-audit.yaml
  capabilities/        # Capability manifests for DDP workflows
    digest.yaml
    todoist.yaml
    audit.yaml
  README.md            # This file
```

## Architecture

The DDP application is structured as a set of declarative workflows that produce
a daily briefing document. Each workflow step is governed by the PEARL Constitution:

- **Article 1**: Deterministic steps (fetch, parse, format) are routed to scripts
- **Article 4**: Success requires machine-verifiable evidence
- **Article 5**: Side effects (sending notifications) require idempotency keys
- **Article 11**: Autonomy level derived from verification coverage
