"""Run copied before/after component probes; this never launches Deppy."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binaries", type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parent
manifest = json.loads((root / "manifest.json").read_text())
manifest["comparison_binary_sha256"] = {}
for repeat in range(1, 4):
    for stage in ["before", "after"]:
        (root / stage).mkdir(exist_ok=True)
        for name, stem in [("deppy-seven-public-probe", "public-probe"),
                           ("deppy-terminal-research-probe", "probe")]:
            binary = args.binaries / stage / name
            manifest["comparison_binary_sha256"][f"{stage}/{name}"] = hashlib.sha256(binary.read_bytes()).hexdigest()
            result = subprocess.run([str(binary)], capture_output=True, text=True)
            (root / stage / f"{stem}-{repeat}.log").write_text(result.stdout + result.stderr)
            print(f"{stage}/{stem}/{repeat}: exit {result.returncode}", flush=True)
            if result.returncode:
                raise RuntimeError("Comparison stopped on failed probe")
manifest["paired_comparison_exit_code"] = 0
manifest["repeats"] = 3
(root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
subprocess.run(["python3", str(root / "compare.py")], check=True, stdout=subprocess.DEVNULL)
