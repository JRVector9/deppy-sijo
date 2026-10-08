#!/usr/bin/env python3
"""Embed opt-in native menu capture evidence in the comparison HTML.

Render first with `cargo test -p deppy-sijo context_menu_audit -- --ignored`.
This script does not run tests or claim that they passed.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-summary", default="테스트 결과 미기록")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    page = root / "docs/mockups/context-menus-compare-2026-10-08.html"
    assets = root / "docs/mockups/context-menu-audit-assets"
    menus = {}
    for path in sorted(assets.glob("*.json")):
        data = json.loads(path.read_text())
        if path.stem != data["id"]:
            raise ValueError(f"ID mismatch: {path}")
        if not (root / "docs/mockups" / data["image"]).is_file():
            raise ValueError(f"Missing image: {path}")
        x, y, w, h = data["rect"]
        for row in data["rows"]:
            rx, ry, rw, rh = row["rect"]
            if not (x <= rx and y <= ry and rx + rw <= x + w and ry + rh <= y + h):
                raise ValueError(f"Row outside captured popup: {path}: {row['label']}")
        menus[data["id"]] = data
    if len(menus) != 17:
        raise ValueError(f"Expected 17 rendered menu screens, found {len(menus)}")
    sources = [
        "crates/app/src/app.rs", "crates/app/src/theme.rs", "crates/app/src/fonts.rs",
        "crates/app/src/ui/context_menu.rs", "crates/app/src/ui/context_menu_audit.rs",
        "crates/app/src/ui/workspace.rs", "crates/app/src/ui/file_tree.rs",
        "crates/app/src/ui/fleet.rs", "crates/app/src/ui/notes.rs",
        "crates/i18n/locales/ko-KR/messages.txt",
    ]
    hashes = {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources}
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    dirty = subprocess.check_output(
        ["git", "status", "--porcelain", "--", *sources], cwd=root, text=True
    ).strip()
    suffix = " + 미커밋 변경" if dirty else ""
    payload = {
        "menus": menus,
        "tests": args.test_summary,
        "source": f"2026-10-08 소스 대조 · HEAD {head[:8]}{suffix} · 실행 중 앱 캡처 아님",
        "source_files_sha256": hashes,
        "embedded_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    encoded = json.dumps(payload, ensure_ascii=False, indent=2).replace("<", "\\u003c")
    old = page.read_text()
    pattern = r'(<script id="evidence-data" type="application/json">).*?(</script>)'
    new, count = re.subn(pattern, lambda m: m[1] + encoded + m[2], old, count=1, flags=re.S)
    if count != 1:
        raise ValueError("Missing evidence-data block")
    page.write_text(new)
    print(f"Embedded {len(menus)} native menu screens into {page.relative_to(root)}")


if __name__ == "__main__":
    main()
