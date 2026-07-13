#!/usr/bin/env python3
"""외부 리소스 샘플러 — render-bench.sh 가 실행 중인 앱 프로세스 트리를 관찰한다.

앱 내부 계측(JSONL `rss` 이벤트)과 **독립된 소스**다. 두 값을 report.py가 교차 검증한다
(앱이 자기 RSS를 잘못 보고하거나 자식 프로세스를 누락하면 여기서 드러난다).

- 루트 pid + 자손 전체의 RSS/CPU 합(`ps -axo pid,ppid,rss,%cpu` 1회 스냅샷으로 트리 구성)
- 루트 프로세스의 스레드 수(`ps -M <pid>` 행 수 - 헤더 1)
- 프로세스 수(루트 포함 트리 크기)
- 루트 pid가 사라지면 종료한다.

CSV: ts_ms,elapsed_s,app_rss_kb,app_cpu,tree_rss_kb,tree_cpu,procs,threads
"""

import argparse
import subprocess
import sys
import time


def ps_snapshot():
    """pid -> (ppid, rss_kb, cpu_pct)"""
    out = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,%cpu="], capture_output=True, text=True
    ).stdout
    procs = {}
    for line in out.splitlines():
        f = line.split()
        if len(f) < 4:
            continue
        try:
            procs[int(f[0])] = (int(f[1]), int(f[2]), float(f[3]))
        except ValueError:
            continue
    return procs


def descendants(procs, root):
    """root와 그 자손 pid 목록 (root가 죽었으면 빈 목록)."""
    if root not in procs:
        return []
    children = {}
    for pid, (ppid, _, _) in procs.items():
        children.setdefault(ppid, []).append(pid)
    seen, stack = [], [root]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.append(pid)
        stack.extend(children.get(pid, []))
    return seen


def thread_count(pid):
    """`ps -M <pid>`: 헤더 1줄 + 스레드당 1줄."""
    r = subprocess.run(["ps", "-M", str(pid)], capture_output=True, text=True)
    lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
    return max(len(lines) - 1, 0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--interval", type=float, default=1.0)  # 1~2초 권장
    args = ap.parse_args()

    t0 = time.time()
    with open(args.out, "w", buffering=1) as fh:
        fh.write("ts_ms,elapsed_s,app_rss_kb,app_cpu,tree_rss_kb,tree_cpu,procs,threads\n")
        while True:
            procs = ps_snapshot()
            tree = descendants(procs, args.pid)
            if not tree:
                break  # 앱 종료
            _, app_rss, app_cpu = procs[args.pid]
            tree_rss = sum(procs[p][1] for p in tree)
            tree_cpu = sum(procs[p][2] for p in tree)
            now = time.time()
            fh.write(
                f"{int(now * 1000)},{now - t0:.1f},{app_rss},{app_cpu:.1f},"
                f"{tree_rss},{tree_cpu:.1f},{len(tree)},{thread_count(args.pid)}\n"
            )
            time.sleep(args.interval)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
