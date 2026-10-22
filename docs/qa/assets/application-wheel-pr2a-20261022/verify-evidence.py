#!/usr/bin/env python3
"""Verify the archived captures and pinned source, without running a provider."""

import hashlib
import json
from pathlib import Path
import subprocess


assets = Path(__file__).resolve().parent
repo = assets.parents[3]
manifest = json.loads((assets / "manifest.json").read_text())
for gate in manifest["gates"]:
    raw = (assets / gate["log"]).read_bytes()
    assert hashlib.sha256(raw).hexdigest() == gate["archived_sha256"], gate["log"]
for source in manifest["source_files"]:
    raw = subprocess.check_output(
        ["git", "show", f"{manifest['test_cleanup_commit']}:{source['path']}"],
        cwd=repo,
    )
    assert hashlib.sha256(raw).hexdigest() == source["sha256"], source["path"]
suite = (assets / "affected-suites-final.log").read_text()
for expected in manifest["full_suite_results"].values():
    if isinstance(expected, dict) and "passed" in expected:
        summary = (
            f"test result: ok. {expected['passed']} passed; 0 failed; "
            f"{expected['ignored']} ignored;"
        )
        assert summary in suite, summary
assert "test result: ok. 8 passed; 0 failed;" in (
    assets / "runtime-final-focused.log"
).read_text()
assert "Finished `dev` profile" in (assets / "clippy-final.log").read_text()
print("PASS: 13 archived logs, 8 pinned source hashes, suite and final gate summaries")
