"""Aggregate three executed repetitions; raw logs remain the source of truth."""
from pathlib import Path
import json
import re
import statistics

ROOT = Path(__file__).resolve().parent


def read_probes(directory, pattern):
    values = {}
    for path in sorted(directory.glob(pattern)):
        for line in path.read_text().splitlines():
            if line.startswith("footprint history=") and " us=" in line:
                name = "footprint-" + re.search(r"history=(\d+)", line)[1]
            elif line.startswith("snapshot scenario="):
                name = "snapshot-" + re.search(r"scenario=(\w+)", line)[1]
            elif "cached render+tess:" in line:
                name = "renderer-" + line.split()[0]
                row = dict(re.findall(r"(\w+(?:/frame)?)=([\d.]+)", line))
                row["ms"] = re.search(r"tess: ([\d.]+)ms", line)[1]
                for key, value in row.items():
                    values.setdefault(name, {}).setdefault(key, []).append(float(value))
                continue
            else:
                continue
            for key, value in re.findall(r"(\w+)=([\d.]+)", line):
                if key in ["us", "calls", "bytes", "backend_resident_before", "backend_resident_after"]:
                    values.setdefault(name, {}).setdefault(key, []).append(float(value))
    return {
        name: {key: {"samples": samples, "median": statistics.median(samples)}
               for key, samples in fields.items()}
        for name, fields in values.items()
    }


report = {}
for stage in ["before", "stage6", "after"]:
    directory = ROOT / stage
    if directory.exists():
        report[stage] = {
            **read_probes(directory, "public-probe-*.log"),
            **read_probes(directory, "probe-*.log"),
        }
report["scope"] = (
    "Real release backend/egui APIs, System allocator, three runs. "
    "Snapshot feed cost excluded; requested allocation bytes are not RSS. "
    "Renderer draw+tessellation CPU is not GPU time; shape count is not draw calls. "
    "Footprint O(1) query times approach the benchmark floor. "
    "No native macOS IME or isolated Deppy GUI execution was performed."
)
(ROOT / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report, indent=2))
