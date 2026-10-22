#!/usr/bin/env python3
"""Run one offline, disposable installed-provider Session/PTY table experiment."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time

import analyze
import fixtures

ROOT = Path(__file__).resolve().parents[3]


class MacBsdInfo(ctypes.Structure):
    # Same proc_bsdinfo ABI used by crates/core/src/process_identity.rs.
    _fields_ = [(name, ctypes.c_uint32) for name in (
        "flags", "status", "xstatus", "pid", "ppid", "uid", "gid", "ruid", "rgid", "svuid", "svgid", "rfu")]
    _fields_ += [("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32)]
    _fields_ += [(name, ctypes.c_uint32) for name in ("nfiles", "pgid", "jobc", "tdev", "tpgid", "nice")]
    _fields_ += [("start_sec", ctypes.c_uint64), ("start_usec", ctypes.c_uint64)]


def process_birth(pid):
    if sys.platform == "darwin":
        info = MacBsdInfo()
        proc = ctypes.CDLL("/usr/lib/libproc.dylib").proc_pidinfo
        proc.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        proc.restype = ctypes.c_int
        size = ctypes.sizeof(info)
        if proc(pid, 3, 0, ctypes.byref(info), size) != size or info.pid != pid or not info.start_sec or info.start_usec >= 1000000:
            return None
        return (info.start_sec, info.start_usec)
    if sys.platform == "linux":
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
            return (int(fields[19]), 0)
        except (OSError, ValueError, IndexError):
            return None
    raise ValueError("process identity unsupported on this platform")


def process_table():
    output = subprocess.check_output(["/bin/ps", "-axo", "pid=,ppid="], text=True, timeout=2)
    return {int(fields[0]): int(fields[1]) for line in output.splitlines() if len(fields := line.split()) == 2}


def collect_descendants(root_pid, known):
    rows = process_table()
    roots = {root_pid} | {pid for pid, birth in known.items() if process_birth(pid) == birth}
    pending = True
    while pending:
        pending = False
        for pid, parent in rows.items():
            if parent in roots and pid not in roots:
                birth = process_birth(pid)
                if birth is not None:
                    roots.add(pid)
                    known[pid] = birth
                    pending = True
    return known


def kill_test_tree(process, known):
    # PTY children call setsid(), so a wrapper group alone does not contain them.
    # Revalidate the kernel birth identity before signalling detached descendants.
    collect_descendants(process.pid, known)
    killed = []
    for pid, birth in reversed(list(known.items())):
        if process_birth(pid) == birth:
            try:
                os.kill(pid, signal.SIGKILL)
                killed.append(pid)
            except ProcessLookupError:
                pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    return killed


def run_child(command, *, env, cwd, log, timeout, output_limit=2 * 1024 * 1024):
    process = subprocess.Popen(command, env=env, cwd=cwd, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, start_new_session=True)
    result = {"pid": process.pid, "timed_out": False, "output_capped": False}
    deadline, written = time.monotonic() + timeout, 0
    descendants, next_scan = {}, 0.0
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        with Path(log).open("wb") as output:
            while selector.get_map():
                if time.monotonic() >= next_scan:
                    collect_descendants(process.pid, descendants)
                    next_scan = time.monotonic() + .1
                if time.monotonic() >= deadline:
                    result["timed_out"] = True
                    break
                for key, _ in selector.select(min(.05, max(0, deadline - time.monotonic()))):
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    output.write(data[:max(0, output_limit - written)])
                    written += len(data)
                    if written > output_limit:
                        result["output_capped"] = True
                        break
                if result["output_capped"]:
                    break
            if result["timed_out"] or result["output_capped"]:
                result["terminated_descendants"] = kill_test_tree(process, descendants)
            # A child may close stdout before exiting. The same deadline still applies.
            try:
                code = process.wait(timeout=max(.01, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                result["timed_out"] = True
                result["terminated_descendants"] = kill_test_tree(process, descendants)
                code = process.wait()
    finally:
        selector.close()
        process.stdout.close()
        if process.poll() is None:
            result["terminated_descendants"] = kill_test_tree(process, descendants)
            process.wait()
    result["exit_code"] = 124 if result["timed_out"] else 125 if result["output_capped"] else code
    return result


def fixture_env(root, source):
    # Allowlist, not a provider-prefix denylist. Preserve real HOME; dedicated CLI
    # config variables and sandbox restrictions provide the profile boundary.
    env = {key: source[key] for key in ("PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL") if key in source}
    env.update({"LANG": "en_US.UTF-8", "SHELL": "/bin/sh", "BROWSER": "/usr/bin/false",
        "TMPDIR": str(root / "tmp"), "XDG_CONFIG_HOME": str(root / "xdg"),
        "CLAUDE_CONFIG_DIR": str(root / "profile"), "CODEX_HOME": str(root / "profile"),
        "GROK_HOME": str(root / "profile"), "ANTHROPIC_API_KEY": fixtures.KEY,
        "ANTHROPIC_BASE_URL": "http://127.0.0.1:9", "XAI_API_KEY": fixtures.KEY,
        "DEPPY_TABLE_FIXTURE_KEY": fixtures.KEY,
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "CLAUDE_CODE_DISABLE_AUTO_UPDATE": "1",
        "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        "GROK_DISABLE_AUTOUPDATER": "1", "GROK_TELEMETRY_ENABLED": "false",
        "GROK_FEEDBACK_ENABLED": "false", "GROK_ERROR_REPORTING": "false",
        "GROK_CLAUDE_HOOKS_ENABLED": "false", "GROK_CLAUDE_MCPS_ENABLED": "false",
        "GROK_CURSOR_HOOKS_ENABLED": "false", "GROK_CURSOR_MCPS_ENABLED": "false"})
    return env


def digest(path):
    with Path(path).open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def sandbox_policy(root, binary, source):
    # Never fall back to unsandboxed execution. Explicit binary exception is
    # necessary for Codex installations stored within their real config tree.
    literal = lambda value: json.dumps(str(value))
    lines = ["(version 1)", "(allow default)", '(deny network-outbound (remote ip "*:*"))',
        f'(deny file-write* (require-all (require-not (subpath {literal(root)})) (require-not (subpath "/dev"))))',
        '(deny mach-lookup (global-name "com.apple.securityd"))']
    home = source.get("HOME")
    if not home:
        raise ValueError("HOME must identify the real user home; it is never replaced")
    for name in (".claude", ".claude.json", ".codex", ".grok", ".config", ".agents", ".ssh", ".aws", ".netrc", ".gitconfig", "Library/Keychains"):
        path = Path(home) / name
        lines.append(f'(deny file-read* (require-all (subpath {literal(path)}) (require-not (literal {literal(binary)}))))')
    return "\n".join(lines) + "\n"


def provider_args(provider, mode, fixture):
    sid, project = fixture["session_id"], str(fixture["project"])
    if provider == "claude":
        return ["--bare", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
            "--permission-mode", "default", "--resume", sid, "--settings",
            json.dumps({"tui": "fullscreen" if mode == "fullscreen" else "default", "disableAllHooks": True})]
    if provider == "codex":
        args = ["--no-daemon", "-m", "gpt-6.1-sol", "-c", 'model_reasoning_effort="xhigh"', "resume", sid, "-C", project]
        return args + (["--no-alt-screen"] if mode == "inline" else [])
    args = ["--cwd", project, "--resume", sid, "--no-subagents"]
    return args + (["--minimal"] if mode == "minimal" else ["--fullscreen"])


def build_probe(root, target):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS="2")
    log = root / "build.jsonl"
    result = run_child(["cargo", "test", "--offline", "--locked", "-p", "session", "--test", "terminal_table_resize",
                        "--no-run", "--message-format=json"], env=env, cwd=ROOT, log=log, timeout=600, output_limit=16 * 1024 * 1024)
    if result["exit_code"]:
        raise ValueError(f"probe build failed ({result['exit_code']}): {log}")
    executables = []
    for line in log.read_text().splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("reason") == "compiler-artifact" and value.get("target", {}).get("name") == "terminal_table_resize" and value.get("executable"):
            executables.append(Path(value["executable"]))
    if len(executables) != 1:
        raise ValueError("cargo did not report exactly one test executable")
    return executables[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True, choices=("claude", "codex", "grok"))
    parser.add_argument("--binary", required=True, type=Path, help="explicit installed CLI; resolved and SHA-pinned before running")
    parser.add_argument("--mode", required=True)
    parser.add_argument("--history", action="store_true")
    parser.add_argument("--output-parent", type=Path, default=Path(tempfile.gettempdir()))
    parser.add_argument("--probe", type=Path, help="prebuilt test executable; default builds and discovers via Cargo JSON")
    parser.add_argument("--target-dir", type=Path, default=Path(tempfile.gettempdir()) / "deppy-table-harness-target")
    args = parser.parse_args()
    if (args.provider, args.mode) not in analyze.MARGINS:
        parser.error("unsupported provider/mode")
    if sys.platform != "darwin" or not Path("/usr/bin/sandbox-exec").is_file():
        parser.error("this runner requires macOS sandbox-exec; no network-enabled fallback")
    binary = args.binary.expanduser().resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an executable file")
    parent = args.output_parent.resolve(strict=True)
    if not parent.is_relative_to(Path(tempfile.gettempdir()).resolve()):
        parser.error("output-parent must be inside the OS temporary directory")
    root = Path(tempfile.mkdtemp(prefix=f"deppy-table-{args.provider}-{args.mode}-", dir=parent)).resolve()
    for name in ("tmp", "xdg"):
        (root / name).mkdir()
    env = fixture_env(root, os.environ)
    policy = root / "offline.sb"
    policy.write_text(sandbox_policy(root, binary, os.environ))
    sandbox = ["/usr/bin/sandbox-exec", "-f", str(policy)]
    manifest = {"provider": args.provider, "mode": args.mode, "history": args.history,
        "requested_binary": str(args.binary), "resolved_binary": str(binary), "binary_sha256_before": digest(binary),
        "network": "sandbox denies external IP outbound; dummy fixture auth only", "fixture_root": str(root)}
    code = 2
    try:
        # The connect must fail with EPERM/EACCES, proving policy enforcement.
        check = run_child(sandbox + [sys.executable, "-I", "-c", "import errno,socket,sys\ntry:\n socket.create_connection(('1.1.1.1',443),.5)\nexcept OSError as e:\n sys.exit(0 if e.errno in (errno.EPERM,errno.EACCES) else 3)\nsys.exit(4)"], env=env, cwd=root, log=root / "sandbox.log", timeout=3)
        manifest["sandbox_preflight"] = check
        if check["exit_code"]:
            raise ValueError("external-IP sandbox denial could not be proven")
        version = run_child(sandbox + [str(binary), "--version"], env=env, cwd=root, log=root / "version.txt", timeout=10)
        if version["exit_code"]:
            raise ValueError("sandboxed CLI version failed")
        manifest["version"] = (root / "version.txt").read_text().strip()
        fixture = fixtures.generate(root, args.provider, manifest["version"], args.history)
        manifest["transcript_sha256_before"] = digest(fixture["transcript"])
        probe = args.probe.resolve(strict=True) if args.probe else build_probe(root, args.target_dir)
        manifest["probe"] = str(probe)
        manifest["probe_sha256"] = digest(probe)
        command = sandbox + [str(binary)] + provider_args(args.provider, args.mode, fixture)
        env.update({"DEPPY_TABLE_PROBE_PROGRAM": command[0], "DEPPY_TABLE_PROBE_ARGS": "\n".join(command[1:]),
            "DEPPY_TABLE_PROBE_ROOT": str(root), "DEPPY_TABLE_PROBE_CWD": str(fixture["project"]),
            "DEPPY_TABLE_PROBE_BOOTSTRAP": "1", "DEPPY_TABLE_PROBE_SCROLL_TOP": "1" if args.history else "0",
            "CLAUDE_CODE_NO_FLICKER": "1" if args.mode == "fullscreen" else "0"})
        manifest["command"] = command
        manifest["probe_result"] = run_child([str(probe), "--exact", "installed_agent_table_resize_probe", "--ignored", "--nocapture"], env=env, cwd=ROOT, log=root / "probe.log", timeout=45)
        result = analyze.analyze_case(root, provider=args.provider, mode=args.mode, history=args.history)
        manifest["analysis"] = result
        code = manifest["probe_result"]["exit_code"] or result["exit_code"]
        manifest["transcript_sha256_after"] = digest(fixture["transcript"])
    except (OSError, ValueError) as error:
        manifest["error"] = str(error)
    finally:
        manifest["binary_sha256_after"] = digest(binary)
        if manifest["binary_sha256_after"] != manifest["binary_sha256_before"]:
            manifest["error"] = "resolved binary changed during run"
            code = 2
        manifest["source_dirty"] = bool(subprocess.check_output(["git", "status", "--porcelain", "--", "crates/session/tests/terminal_table_resize.rs", "scripts/qa/agent-terminal-table-resize"], cwd=ROOT, text=True).strip())
        manifest["source_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        manifest["source_sha256"] = {str(path.relative_to(ROOT)): digest(path) for path in [ROOT / "crates/session/tests/terminal_table_resize.rs", *Path(__file__).parent.glob("*.py")]}
        manifest["artifacts"] = {path.name: digest(path) for path in root.iterdir() if path.is_file()}
        manifest["exit_code"] = code
        fixtures.write_json(root / "manifest.json", manifest)
    print(json.dumps({"output": str(root), "exit_code": code, "analysis": manifest.get("analysis"), "error": manifest.get("error")}, ensure_ascii=False))
    return code


if __name__ == "__main__":
    raise SystemExit(main())
