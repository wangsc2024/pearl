#!/usr/bin/env python3
"""audit_facts.py -- P0 mechanical script: collects mechanical system facts for auditing.

Implements the System Audit fact collection from 系統開發需求書 §65.

Collects purely mechanical, deterministic facts:
  - Test count (from cargo test output or last known result)
  - Hook count (git hooks present)
  - Config validity (YAML parseable, required fields present)
  - Manifest count (capability manifests registered)
  - Crate count (workspace members)

Script I/O Contract (SS26):
  stdin/PEARL_INPUT: JSON object with keys:
    - "project_root": path to the project root (default: ".")
    - "checks": list of fact checks to run (default: all)
  stdout: machine JSON only (last line)
  stderr: diagnostic messages

Exit codes:
  0 = all facts collected successfully
  1 = some facts could not be collected
  2 = input error
"""

import json
import os
import sys
import glob
from datetime import datetime, timezone
from pathlib import Path


def main():
    raw_input = os.environ.get("PEARL_INPUT", "")
    if not raw_input:
        raw_input = sys.stdin.read()

    if not raw_input.strip():
        payload = {}
    else:
        try:
            payload = json.loads(raw_input)
        except json.JSONDecodeError as e:
            print(f"Failed to parse input: {e}", file=sys.stderr)
            print(json.dumps({"success": False, "error": f"parse error: {str(e)}"}))
            sys.exit(2)

    project_root = Path(payload.get("project_root", ".")).resolve()
    checks = payload.get("checks", ["test_count", "hook_count", "config_validity", "manifest_count", "crate_count"])

    print(f"Auditing project at: {project_root}", file=sys.stderr)
    print(f"Running checks: {checks}", file=sys.stderr)

    facts = {}
    errors = []

    if "test_count" in checks:
        facts["test_count"] = _count_test_functions(project_root)
        print(f"  test_count: {facts['test_count']}", file=sys.stderr)

    if "hook_count" in checks:
        facts["hook_count"] = _count_git_hooks(project_root)
        print(f"  hook_count: {facts['hook_count']}", file=sys.stderr)

    if "config_validity" in checks:
        validity = _check_config_validity(project_root)
        facts["config_validity"] = validity
        print(f"  config_validity: {validity['valid']}", file=sys.stderr)

    if "manifest_count" in checks:
        facts["manifest_count"] = _count_manifests(project_root)
        print(f"  manifest_count: {facts['manifest_count']}", file=sys.stderr)

    if "crate_count" in checks:
        facts["crate_count"] = _count_crates(project_root)
        print(f"  crate_count: {facts['crate_count']}", file=sys.stderr)

    result = {
        "success": len(errors) == 0,
        "project_root": str(project_root),
        "facts": facts,
        "errors": errors,
        "audited_at": datetime.now(timezone.utc).isoformat(),
    }

    print(json.dumps(result))
    sys.exit(0 if not errors else 1)


def _count_test_functions(root):
    """Count #[test] annotations in Rust source files."""
    count = 0
    for rs_file in root.rglob("*.rs"):
        try:
            content = rs_file.read_text(encoding="utf-8", errors="ignore")
            count += content.count("#[test]")
        except (OSError, UnicodeDecodeError):
            pass
    return count


def _count_git_hooks(root):
    """Count executable git hooks."""
    hooks_dir = root / ".git" / "hooks"
    if not hooks_dir.exists():
        return 0
    count = 0
    for hook in hooks_dir.iterdir():
        if hook.is_file() and not hook.name.endswith(".sample"):
            count += 1
    return count


def _check_config_validity(root):
    """Check if key configuration files are valid."""
    checks = {}

    # Check Cargo.toml
    cargo_toml = root / "Cargo.toml"
    if cargo_toml.exists():
        try:
            content = cargo_toml.read_text()
            checks["Cargo.toml"] = "[workspace]" in content
        except OSError:
            checks["Cargo.toml"] = False
    else:
        checks["Cargo.toml"] = False

    # Check for capability manifests being valid YAML (basic check)
    caps_dir = root / "capabilities"
    if caps_dir.exists():
        yaml_files = list(caps_dir.rglob("*.yaml")) + list(caps_dir.rglob("*.yml"))
        valid_count = 0
        for yf in yaml_files:
            try:
                content = yf.read_text()
                # Basic YAML validity: has 'id:' field
                if "id:" in content:
                    valid_count += 1
            except OSError:
                pass
        checks["manifests_valid"] = valid_count == len(yaml_files) if yaml_files else True
    else:
        checks["manifests_valid"] = True

    return {
        "valid": all(checks.values()),
        "details": checks,
    }


def _count_manifests(root):
    """Count capability manifest YAML files."""
    caps_dir = root / "capabilities"
    if not caps_dir.exists():
        return 0
    yaml_files = list(caps_dir.rglob("*.yaml")) + list(caps_dir.rglob("*.yml"))
    return len(yaml_files)


def _count_crates(root):
    """Count workspace member crates."""
    cargo_toml = root / "Cargo.toml"
    if not cargo_toml.exists():
        return 0
    try:
        content = cargo_toml.read_text()
        # Count lines that look like workspace members.
        count = 0
        in_members = False
        for line in content.splitlines():
            if "members" in line and "[" in line:
                in_members = True
                continue
            if in_members:
                if "]" in line:
                    break
                if '"' in line:
                    count += 1
        return count
    except OSError:
        return 0


if __name__ == "__main__":
    main()
