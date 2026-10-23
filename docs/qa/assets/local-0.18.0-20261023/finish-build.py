"""Wait only for this task's two build results, then package; never launch."""
import json, os, subprocess, time
from pathlib import Path
ROOT = Path(__file__).resolve().parents[4]
EV = Path(__file__).resolve().parent
start = time.monotonic()
for name in ["native-build-result.json", "helpers-build-result.json"]:
    while not (EV / name).exists():
        if time.monotonic() - start > 3600:
            raise SystemExit("build wait deadline")
        time.sleep(1)
    result = json.loads((EV / name).read_text())
    if result["exit_code"] != 0:
        raise SystemExit(name + " failed; no bundle created")
cmd = ["python3", str(EV / "local-bundle.py")]
start = time.monotonic()
with (EV / "package.log").open("w") as output:
    result = subprocess.run(cmd, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
record = {"command": cmd, "exit_code": result.returncode, "elapsed_seconds": round(time.monotonic() - start, 3)}
(EV / "package-command-result.json").write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps(record), flush=True)
print("\n".join((EV / "package.log").read_text().splitlines()[-35:]), flush=True)
raise SystemExit(result.returncode)
