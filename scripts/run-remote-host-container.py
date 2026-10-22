#!/usr/bin/env python3
"""Run one task-owned helper container with a host-side deadline."""
import argparse
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--container", required=True)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ["--"]:
        args.command.pop(0)
    if not re.fullmatch(r"deppy-remote-host-(?:build|baseline|storage)-[0-9a-f]{48}", args.container) or not 0 < args.seconds <= 7200 or args.command[:1] != ["docker"]:
        parser.error("invalid helper container deadline")
    try:
        return subprocess.run(args.command, timeout=args.seconds).returncode
    except subprocess.TimeoutExpired:
        # The client process timeout does not remove a daemon-owned container.
        # Names are restricted above to this builder's random task namespace.
        try:
            subprocess.run(
                ["docker", "rm", "--force", args.container],
                timeout=30,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            pass
        parser.exit(124, "remote helper container deadline exceeded\n")


if __name__ == "__main__":
    raise SystemExit(main())
