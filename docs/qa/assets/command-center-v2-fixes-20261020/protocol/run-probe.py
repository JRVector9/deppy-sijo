#!/usr/bin/env python3
"""Compile actual provider/controller modules with independent scope regressions.

Uses existing isolated Cargo artifacts; never starts Deppy or a provider session.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--repo", type=Path, required=True)
parser.add_argument("--deps", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
parser.add_argument("--revision", help="Read committed modules; omit for working tree")
parser.add_argument("--pattern", default="independent_")
args = parser.parse_args()
args.out.mkdir(parents=True, exist_ok=True)
sources = {}
for module in ("agent_session", "codex_app_server"):
    relative = f"crates/app/src/{module}.rs"
    if args.revision:
        content = subprocess.check_output(
            ["git", "show", f"{args.revision}:{relative}"], cwd=args.repo
        )
    else:
        content = (args.repo / relative).read_bytes()
    sources[relative] = hashlib.sha256(content).hexdigest()
    if module == "codex_app_server":
        content += Path(__file__).with_name("scope-regressions.rs").read_bytes()
    (args.out / f"{module}.rs").write_bytes(content)
(args.out / "lib.rs").write_text(
    '#![allow(dead_code, unused_imports)]\n'
    'mod agent_session;\nmod codex_app_server;\n'
    'mod llm_proxy { pub struct LlmProxyHandle { pub port: u16 } '
    'pub fn spawn(_: String, _: Option<secret::SecretString>) '
    '-> anyhow::Result<LlmProxyHandle> { '
    'panic!("provider spawn is outside this probe") } }\n'
)
command = [
    "rustc", "--edition=2024", "--test", str(args.out / "lib.rs"),
    "-L", f"dependency={args.deps}", "-o", str(args.out / "scope-probe"),
]
dependencies = {}
for name in ("anyhow", "serde_json", "egui", "secret", "sha2", "libc", "uuid"):
    artifact = max(args.deps.glob(f"lib{name}-*.rlib"), key=lambda p: p.stat().st_mtime)
    dependencies[name] = str(artifact)
    command += ["--extern", f"{name}={artifact}"]
environment = os.environ.copy()
manifest = tomllib.loads((args.repo / "Cargo.toml").read_text())
environment["CARGO_PKG_VERSION"] = manifest["workspace"]["package"]["version"]
(args.out / "inputs.json").write_text(json.dumps({
    "revision": args.revision or "working tree", "source_sha256": sources,
    "dependencies": dependencies, "compile_command": command,
}, indent=2) + "\n")
compiled = subprocess.run(command, env=environment, capture_output=True, text=True)
(args.out / "compile.log").write_text(compiled.stdout + compiled.stderr)
if compiled.returncode:
    print(compiled.stdout + compiled.stderr, end="")
    raise SystemExit(compiled.returncode)
tested = subprocess.run(
    [str(args.out / "scope-probe"), args.pattern, "--test-threads=1", "--nocapture"],
    capture_output=True, text=True,
)
(args.out / "result.log").write_text(tested.stdout + tested.stderr)
print(tested.stdout + tested.stderr, end="")
raise SystemExit(tested.returncode)
