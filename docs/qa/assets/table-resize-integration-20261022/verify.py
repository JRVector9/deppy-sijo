#!/usr/bin/env python3
"""Check archived hashes and unchanged tested sources; does not rerun tests."""
from pathlib import Path
import gzip
import hashlib
import json


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    here = Path(__file__).resolve().parent
    root = here.parents[3]
    manifest = json.loads((here / "manifest.json").read_text())
    for record in manifest["source_files"]:
        assert digest((root / record["path"]).read_bytes()) == record["sha256"], record
    for record in manifest["logs"] + manifest["installed_cli_files"]:
        data = (here / record["path"]).read_bytes()
        assert digest(data) == record["sha256"], record
        if record.get("compression") == "gzip":
            assert digest(gzip.decompress(data)) == record["uncompressed_sha256"], record
        if "raw_archive" in record:
            raw = (here / record["raw_archive"]).read_bytes()
            assert digest(raw) == record["raw_archive_sha256"], record
            assert digest(gzip.decompress(raw)) == record["original_sha256"], record

    app = (here / "app-full.log").read_text()
    # A child test re-executes one assertion; never add that nested result to totals.
    assert "3518 passed; 0 failed; 40 ignored;" in app
    for count in (4, 5, 15):
        assert f"{count} passed; 0 failed; 0 ignored;" in app
    affected = (here / "affected-full.log").read_text()
    for summary in (
        "428 passed; 0 failed; 1 ignored;",
        "82 passed; 0 failed; 0 ignored;",
        "2 passed; 0 failed; 1 ignored;",
        "140 passed; 0 failed; 4 ignored;",
    ):
        assert summary in affected, summary
    cli = json.loads((here / "codex-inline-history/manifest.json").read_text())
    assert cli["analysis"]["validity"] == "VALID"
    assert cli["analysis"]["resize"] == "PASS"
    assert [s["border_width"] for s in cli["analysis"]["stages"]] == [78, 40, 118, 78]
    print(
        f"PASS: {len(manifest['logs'])} logs, "
        f"{len(manifest['source_files'])} source hashes, exact compressed CLI evidence; "
        "App3557PASS and affected652PASS (ignored not counted)"
    )


if __name__ == "__main__":
    main()
