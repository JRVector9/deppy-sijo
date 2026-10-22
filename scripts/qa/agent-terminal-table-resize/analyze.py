#!/usr/bin/env python3
"""Conservative checks for synthetic assistant tables, never a VT rewrite."""
import argparse
import csv
import json
from pathlib import Path
import re
import unicodedata

COLS = (80, 42, 120, 80)
MODALS = ("Make auto mode your default permission", "Do you trust the contents",
          "Do you want to use this API key", "Choose the text style", "Select login method")
# Measured margins, not a universal table layout specification. Review on CLI updates.
MARGINS = {("grok", "minimal"): 0, ("grok", "fullscreen"): 19,
           ("codex", "inline"): 2, ("codex", "default"): 2,
           ("claude", "classic"): 4, ("claude", "fullscreen"): 4}


def cell_width(text):
    return sum(0 if unicodedata.combining(char) else
               2 if unicodedata.east_asian_width(char) in "WF" else 1 for char in text)


def compact(text):
    return re.sub(r"\s+", "", text)


def cell_slice(text, start, end):
    result, column = [], 0
    for char in text:
        width = cell_width(char)
        if start <= column < end:
            result.append(char)
        column += width
    return "".join(result)


def inspect_snapshot(text):
    lines = [line.rstrip() for line in text.splitlines()]
    tops = [i for i, line in enumerate(lines) if "┌" in line and "┬" in line and "┐" in line]
    rules = [i for i, line in enumerate(lines) if "━" * 3 in line]
    complete = False
    border_width = None
    content = []
    if tops:
        start = tops[-1]
        top = lines[start].strip()
        border_width = cell_width(top[top.index("┌"):top.index("┐") + 1])
        for line in lines[start + 1:]:
            if "└" in line and "┘" in line:
                bottom = line.strip()
                complete = cell_width(bottom[bottom.index("└"):bottom.index("┘") + 1]) == border_width
                break
            if "│" in line:
                content.append(line.split("│")[1:-1])
    elif rules:
        start = rules[-1]
        border_width = cell_width(lines[start].strip())
        spans = [(match.start(), match.end()) for match in re.finditer("━+", lines[start])]
        for line in lines[start + 1:]:
            if not line.strip():
                break
            content.append([cell_slice(line, left, right) for left, right in spans])
    columns = ["".join(row[index] for row in content if len(row) > index)
               for index in range(max((len(row) for row in content), default=0))]
    semantic = " ".join(compact(column) for column in columns)
    required = ("ROW_ALPHA", "ROW_BETA", "ROW_OMEGA", "ready", "waiting", "done",
                "한글표", "원문과복사내용을보존합니다.")
    semantic_complete = all(token in semantic for token in required)
    if not tops and rules:
        complete = semantic_complete
    return {"border_width": border_width, "complete": complete,
            "semantic_complete": semantic_complete,
            "visibility": "ROW_ALPHA" in semantic and "ready" in semantic,
            "missing_tokens": [token for token in required if token not in semantic],
            "modal": next((marker for marker in MODALS if marker in text), None)}


def analyze_case(root, *, provider, mode, history):
    root = Path(root)
    result = {"validity": "INVALID", "visibility": "INVALID", "resize": "INVALID",
              "semantics": "INVALID", "exit_code": 2, "stages": [], "reasons": []}
    metadata = root / "metadata.tsv"
    if not metadata.is_file():
        result["reasons"].append("missing metadata.tsv")
        return result
    try:
        with metadata.open() as file:
            rows = list(csv.DictReader(file, delimiter="\t"))
        if [int(row["cols"]) for row in rows] != list(COLS):
            raise ValueError("incomplete resize sequence")
        margin = MARGINS[(provider, mode)]
        for stage, (row, cols) in enumerate(zip(rows, COLS)):
            if int(row["stage"]) != stage or int(row["rows"]) != 40 or row["alt"] not in ("true", "false"):
                raise ValueError("invalid stage geometry or screen mode")
            source = "application-top" if history and row["alt"] == "true" else "history" if history else "stage"
            path = root / f"{source}-{stage}-{cols}.txt"
            if not path.is_file():
                result["reasons"].append(f"missing {path.name}; no history-source fallback")
                result["stages"].append({"cols": cols, "source": path.name, "valid": False})
                continue
            observed = inspect_snapshot(path.read_text())
            observed.update({"cols": cols, "source": path.name, "expected_width": cols - margin})
            observed["valid"] = observed["complete"] and observed["semantic_complete"] and not observed["modal"]
            if not observed["valid"]:
                result["reasons"].append(f"{path.name}: cropped/missing table semantics, unsupported shape or modal")
            result["stages"].append(observed)
    except (ValueError, KeyError, OSError) as error:
        result["reasons"].append(str(error))
        return result
    stages = result["stages"]
    result["visibility"] = "PASS" if all(stage.get("visibility", False) for stage in stages) else "INVALID"
    result["semantics"] = "PASS" if all(stage.get("semantic_complete", False) for stage in stages) else "INVALID"
    measurements = [stage for stage in stages if stage.get("border_width") is not None]
    if any(stage["border_width"] != stage["expected_width"] for stage in measurements):
        result["resize"] = "FAIL"
    elif len(measurements) == 4 and all(stage["valid"] for stage in stages):
        result["resize"] = "PASS"
    if len(stages) == 4 and all(stage["valid"] for stage in stages):
        result["validity"] = "VALID"
        result["exit_code"] = 0 if result["resize"] == "PASS" else 1
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--provider", required=True, choices=("claude", "codex", "grok"))
    parser.add_argument("--mode", required=True)
    parser.add_argument("--history", action="store_true")
    args = parser.parse_args()
    result = analyze_case(args.root, provider=args.provider, mode=args.mode, history=args.history)
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return result["exit_code"]


if __name__ == "__main__":
    raise SystemExit(main())
