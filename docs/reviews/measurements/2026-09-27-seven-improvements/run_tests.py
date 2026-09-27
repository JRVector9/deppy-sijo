"""Execute source-specific functional gates. Deppy GUI is never launched."""
from pathlib import Path
import json
import os
import subprocess

ROOT = Path(__file__).resolve().parents[4]
OUT = Path(__file__).resolve().parent / "tests"
OUT.mkdir(exist_ok=True)
commands = {
    "vendor": ["cargo", "test", "--manifest-path", "third_party/alacritty_terminal-0.26.0/Cargo.toml", "--lib"],
    "terminal": ["cargo", "test", "-p", "terminal"],
    "session": ["cargo", "test", "-p", "session"],
    "runtime": ["cargo", "test", "-p", "runtime", "--features", "secret/test-keyring-core", "--", "--test-threads=4"],
    "web": ["cargo", "test", "-p", "web-remote", "--lib"],
    "web-canvas": ["cargo", "test", "-p", "web-remote", "--test", "viewer_core_chrome", "--test", "viewer_grapheme_chrome", "--", "--ignored"],
    "app-workspace": ["cargo", "test", "-p", "deppy-sijo", "--bin", "deppy-sijo", "ui::workspace::tests::", "--", "--test-threads=4"],
    "app-hangul": ["cargo", "test", "-p", "deppy-sijo", "--bin", "deppy-sijo", "한글"],
    "app-ime": ["cargo", "test", "-p", "deppy-sijo", "--bin", "deppy-sijo", "ime"],
    "app-cloud": ["cargo", "test", "-p", "deppy-sijo", "--bin", "deppy-sijo", "cloud_agent"],
    "boundary": ["cargo", "run", "-p", "xtask", "--", "check-boundary"],
    "dependencies": ["cargo", "run", "-p", "xtask", "--", "check-deps"],
    "format": ["cargo", "fmt", "--all", "--", "--check"],
    "whitespace": ["git", "diff", "--check"],
    "release": ["cargo", "build", "-p", "deppy-sijo", "--release"],
}
env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"))
env.pop("DOCS_RS", None)
summary = {"source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(), "results": []}
for name, command in commands.items():
    with (OUT / f"{name}.log").open("w") as log:
        process = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    summary["results"].append({"gate": name, "command": command, "exit_code": process.returncode})
    (OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(f"{name}: exit {process.returncode}", flush=True)
# Retain all failures rather than stopping early or silently retrying them.
