#!/usr/bin/env python3
"""렌더러 A/B 벤치 집계 — JSONL(앱 내부 계측) + CSV(외부 ps 샘플러) → summary.md / summary.csv.

입력: artifacts/render-bench/{glow,wgpu}/<scenario>-ws<N>.{jsonl,csv,json}
출력: artifacts/render-bench/summary.md, summary.csv  (오케스트레이터가 보고서에 붙인다)

원칙
- 값이 없으면 조용히 0으로 만들지 않는다. 전부 `UNKNOWN(사유)` 문자열로 남긴다.
- 앱 내부 RSS와 외부 샘플러 RSS를 **교차 검증**해 불일치를 표로 드러낸다.
- 누수 판정은 "RSS가 안 내려감"이 아니라 **후반 50회 선형회귀 기울기**로 한다
  (macOS allocator는 free된 페이지를 즉시 OS에 반환하지 않는다 — 계획서 §6-8).
"""

import argparse
import csv
import json
import os
import subprocess
import time

MB = 1024 * 1024
RENDERERS = ["glow", "wgpu"]
WS_SCALE = [1, 5, 10, 20]

# 누수 판정 임계값 — 후반 50회 회귀 기울기와 꼬리 증가율
SLOPE_LIMIT_KB_PER_ITER = 128.0
TAIL_GROWTH_LIMIT = 0.02  # 2%


# ── 통계 헬퍼 ────────────────────────────────────────────────────────────────
def pct(values, q):
    if not values:
        return None
    s = sorted(values)
    idx = max(int(len(s) * q + 0.999999) - 1, 0)
    return s[idx]


def linreg_slope(ys):
    """y = a + b*i 의 b (i는 0,1,2,... 등간격 인덱스)."""
    n = len(ys)
    if n < 2:
        return None
    mean_x = (n - 1) / 2
    mean_y = sum(ys) / n
    num = sum((i - mean_x) * (y - mean_y) for i, y in enumerate(ys))
    den = sum((i - mean_x) ** 2 for i in range(n))
    return num / den if den else None


def unknown(reason):
    return f"UNKNOWN({reason})"


def is_unknown(v):
    return isinstance(v, str) and v.startswith("UNKNOWN")


def fmt(v, digits=1, unit=""):
    if v is None:
        return unknown("값 없음")
    if isinstance(v, str):  # UNKNOWN(...) 또는 "Metal / Apple M3" 같은 텍스트 값
        return v
    if isinstance(v, int):  # 개수(atlas_px, alloc count 등)는 정수 그대로
        return f"{v}{unit}"
    return f"{v:.{digits}f}{unit}"


# ── 실행 결과 로딩 ───────────────────────────────────────────────────────────
class Run:
    def __init__(self, renderer, scenario, ws, path_base):
        self.renderer = renderer
        self.scenario = scenario
        self.ws = ws
        self.events = []   # JSONL 이벤트
        self.samples = []  # 외부 샘플러 CSV 행
        self.meta = {}
        self.tag = f"{scenario}-ws{ws}"

        jsonl = f"{path_base}/{self.tag}.jsonl"
        if os.path.exists(jsonl):
            with open(jsonl) as fh:
                for line in fh:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        self.events.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass  # 부분 기록된 마지막 줄 등 — 무시
        csv_path = f"{path_base}/samples-{self.tag}.csv"
        if os.path.exists(csv_path):
            with open(csv_path) as fh:
                for row in csv.DictReader(fh):
                    try:
                        self.samples.append({k: float(v) for k, v in row.items()})
                    except (TypeError, ValueError):
                        pass
        meta_path = f"{path_base}/meta-{self.tag}.json"
        if os.path.exists(meta_path):
            try:
                with open(meta_path) as fh:
                    self.meta = json.load(fh)
            except json.JSONDecodeError:
                pass

    @property
    def exists(self):
        return bool(self.events)

    def ev(self, name):
        return [e for e in self.events if e.get("ev") == name]

    def rss_stage(self, stage, field="total_bytes"):
        for e in self.ev("rss"):
            if e.get("stage") == stage and field in e:
                return e[field]
        return None

    def frame_p(self, key):
        """frame_summary 우선, 없으면 frame 이벤트 ui_ms에서 직접 계산."""
        for e in self.ev("frame_summary"):
            if key in e:
                return e[key]
        ui = [e["ui_ms"] for e in self.ev("frame") if "ui_ms" in e]
        if not ui:
            return None
        return {"p50": pct(ui, 0.50), "p95": pct(ui, 0.95),
                "p99": pct(ui, 0.99), "max": max(ui)}.get(key)

    def steady_cpu(self, col="tree_cpu"):
        """외부 샘플러 CPU 평균 — 워밍업(앞 1/3) 제외."""
        vals = [s[col] for s in self.samples if col in s]
        if len(vals) < 3:
            return None
        tail = vals[len(vals) // 3:]
        return sum(tail) / len(tail)


def load(art):
    runs = {}
    for r in RENDERERS:
        base = os.path.join(art, r)
        if not os.path.isdir(base):
            continue
        for fname in sorted(os.listdir(base)):
            if not fname.endswith(".jsonl"):
                continue
            tag = fname[: -len(".jsonl")]
            if "-ws" not in tag:
                continue
            scenario, ws = tag.rsplit("-ws", 1)
            try:
                ws = int(ws)
            except ValueError:
                continue
            runs[(r, scenario, ws)] = Run(r, scenario, ws, base)
    return runs


# ── 지표 계산 ────────────────────────────────────────────────────────────────
def metrics_for(runs, r):
    def get(scenario, ws):
        run = runs.get((r, scenario, ws))
        return run if run and run.exists else None

    m = {}
    idle1 = get("idle", 1)

    # 시작 / 렌더러 초기화
    if idle1:
        start = idle1.rss_stage("start")
        m["시작 RSS (MB)"] = start / MB if start else unknown("rss stage=start 없음")
        init = idle1.rss_stage("renderer_init")
        m["renderer 초기화 후 RSS (MB)"] = init / MB if init else unknown("rss stage=renderer_init 없음")
        peaks = [e["total_bytes"] for e in idle1.ev("rss")
                 if e.get("stage") in ("start", "renderer_init", "first_frame") and "total_bytes" in e]
        m["초기화 peak RSS (start~first_frame, MB)"] = max(peaks) / MB if peaks else unknown("rss 이벤트 없음")
        rend = idle1.ev("renderer")
        if rend:
            e = rend[0]
            m["백엔드/어댑터"] = f"{e.get('gpu_backend', '?')} / {e.get('adapter', '?')}"
            m["renderer init_ms"] = e.get("init_ms", unknown("init_ms 필드 없음"))
        else:
            m["백엔드/어댑터"] = unknown("renderer 이벤트 없음")
            m["renderer init_ms"] = unknown("renderer 이벤트 없음")
    else:
        for k in ("시작 RSS (MB)", "renderer 초기화 후 RSS (MB)", "초기화 peak RSS (start~first_frame, MB)",
                  "백엔드/어댑터", "renderer init_ms"):
            m[k] = unknown("idle-ws1 미실행")

    # 워크스페이스 스케일 RSS (idle)
    ws_rss = {}
    for n in WS_SCALE:
        run = get("idle", n)
        val = None
        if run:
            val = run.rss_stage("stable_5s") or run.rss_stage("scenario_end")
        if val:
            ws_rss[n] = val / MB
            m[f"ws {n} RSS (MB)"] = val / MB
        else:
            m[f"ws {n} RSS (MB)"] = unknown(f"idle-ws{n} 미실행/rss 없음")
    if len(ws_rss) >= 2:
        ks = sorted(ws_rss)
        # (ws수, RSS) 최소제곱 기울기 — 등간격이 아니므로 직접 계산
        mx = sum(ks) / len(ks)
        my = sum(ws_rss[k] for k in ks) / len(ks)
        den = sum((k - mx) ** 2 for k in ks)
        slope = sum((k - mx) * (ws_rss[k] - my) for k in ks) / den if den else None
        m["ws당 RSS 증가 (MB/ws)"] = slope if slope is not None else unknown("회귀 불가")
    else:
        m["ws당 RSS 증가 (MB/ws)"] = unknown("idle ws 측정점 2개 미만")

    # GPU 자원 추정
    gpu_ev = [e for e in (idle1.ev("gpu") if idle1 else [])]
    if gpu_ev:
        last = gpu_ev[-1]
        tex, buf = last.get("texture_bytes"), last.get("buffer_bytes")
        if tex is None and buf is None:
            m["GPU 추정 bytes (MB)"] = unknown("gpu 이벤트에 bytes 필드 없음")
        else:
            m["GPU 추정 bytes (MB)"] = ((tex or 0) + (buf or 0)) / MB
        m["GPU 텍스처/버퍼 수"] = f"{last.get('textures', '?')} / {last.get('buffers', '?')}"
        m["glyph atlas (px)"] = last.get("atlas_px", unknown("atlas_px 없음"))
    else:
        m["GPU 추정 bytes (MB)"] = unknown("gpu 이벤트 없음")
        m["GPU 텍스처/버퍼 수"] = unknown("gpu 이벤트 없음")
        m["glyph atlas (px)"] = unknown("gpu 이벤트 없음")

    # CPU (외부 샘플러 — 앱 + 자식 트리). 0.0%도 유효값이므로 None 검사로만 UNKNOWN 처리한다.
    def cpu_of(scenario, ws):
        run = get(scenario, ws)
        if not run:
            return unknown(f"{scenario}-ws{ws} 미실행")
        val = run.steady_cpu()
        return val if val is not None else unknown("샘플 부족")

    m["idle CPU ws1 (%)"] = cpu_of("idle", 1)
    m["idle CPU ws20 (%)"] = cpu_of("idle", 20)
    m["bulk CPU ws1 (%)"] = cpu_of("bulk", 1)

    # 프레임 시간
    for label, scenario, ws in (("dirty1 p95 (ms)", "dirty1", 1),
                                ("fullscreen p95 (ms)", "fullscreen", 1),
                                ("bulk p95 (ms)", "bulk", 1),
                                ("bulk p99 (ms)", "bulk", 1)):
        run = get(scenario, ws)
        key = "p99" if label.endswith("p99 (ms)") else "p95"
        val = run.frame_p(key) if run else None
        m[label] = val if val is not None else unknown(f"{scenario}-ws{ws} frame 데이터 없음")

    sel = get("selection", 1)
    sel_p95 = sel.frame_p("p95") if sel else None
    m["selection p95 (ms)"] = sel_p95 if sel_p95 is not None else unknown("수동 미실행")

    # steady-state allocation (bench-alloc 빌드 필요)
    alloc_src = None
    for scenario in ("dirty1", "idle", "bulk"):
        run = get(scenario, 1)
        if run and run.ev("alloc_summary"):
            alloc_src = run
            break
    if alloc_src:
        a = alloc_src.ev("alloc_summary")[-1]
        m["frame alloc count (p50)"] = a.get("alloc_per_frame_p50", unknown("필드 없음"))
        m["frame alloc bytes (p50)"] = a.get("bytes_per_frame_p50", unknown("필드 없음"))
    else:
        frames = []
        for scenario in ("dirty1", "idle", "bulk"):
            run = get(scenario, 1)
            if run:
                frames += [e for e in run.ev("frame") if "alloc_count" in e]
        if frames:
            m["frame alloc count (p50)"] = pct([e["alloc_count"] for e in frames], 0.5)
            m["frame alloc bytes (p50)"] = pct([e.get("alloc_bytes", 0) for e in frames], 0.5)
        else:
            reason = "bench-alloc 미빌드/미구현"
            m["frame alloc count (p50)"] = unknown(reason)
            m["frame alloc bytes (p50)"] = unknown(reason)

    # 워크스페이스 latency
    m["first workspace latency (ms)"] = first_ws_latency(idle1)
    sw = get("switch", 5)
    if sw:
        steps = [e["ms"] for e in sw.ev("ws_step")
                 if "ms" in e and "switch" in str(e.get("step", "")).lower()]
        if not steps:
            steps = [e["ms"] for e in sw.ev("ws_step") if "ms" in e]  # step 이름 불명 시 전량
        m["ws 전환 latency p50 (ms)"] = pct(steps, 0.5) if steps else unknown("ws_step 없음")
        m["ws 전환 latency p95 (ms)"] = pct(steps, 0.95) if steps else unknown("ws_step 없음")
    else:
        m["ws 전환 latency p50 (ms)"] = unknown("switch-ws5 미실행")
        m["ws 전환 latency p95 (ms)"] = unknown("switch-ws5 미실행")

    # 생성·삭제 반복 → 누수 판정
    leak = leak_verdict(get("createdelete", 1))
    m["생성·삭제 RSS slope (KB/iter, 후반 50)"] = leak["slope"]
    m["생성·삭제 누수 판정"] = leak["verdict"]
    m["생성·삭제 후 잔존 스레드"] = leak["threads"]
    m["생성·삭제 후 잔존 프로세스"] = leak["procs"]
    return m


def first_ws_latency(run):
    """ws_step(create 계열) 우선, 없으면 rss 이벤트 타임스탬프 차이로 대체."""
    if not run:
        return unknown("idle-ws1 미실행")
    for e in run.ev("ws_step"):
        step = str(e.get("step", "")).lower()
        if "ms" in e and ("create" in step or "first" in step):
            return e["ms"]
    begin = next((e["t"] for e in run.ev("rss") if e.get("stage") == "ws_create_begin"), None)
    first = next((e["t"] for e in run.ev("rss") if e.get("stage") == "first_frame"), None)
    if begin and first:
        return float(first - begin)  # 대체 산출: ws_create_begin → first_frame
    return unknown("ws_step/rss 타임스탬프 없음")


def leak_verdict(run):
    """후반 50회 선형회귀 기울기로 누수 판정.

    판정 규칙 (계획서 §6-8: "RSS가 안 내려감"만으로 누수 단정 금지)
      - 기울기 |slope| < 128 KB/iter  AND  꼬리 10회 평균이 후반 첫 10회 대비 +2% 이내
        → "누수 징후 없음"
      - 그 외 → "재확인 필요" (누수 '확정'이 아니다. macOS allocator의 지연 반환,
        scrollback/warm 풀 등 의도된 캐시가 원인일 수 있다 — Instruments로 확인)
    """
    out = {"slope": unknown("createdelete 미실행"), "verdict": unknown("createdelete 미실행"),
           "threads": unknown("createdelete 미실행"), "procs": unknown("createdelete 미실행")}
    if not run:
        return out

    # 1순위: 앱 내부 ws_step(rss 포함) — 반복 경계와 정확히 맞는다
    series = [e["rss"] for e in run.ev("ws_step") if "rss" in e]
    source = "ws_step.rss"
    if len(series) < 10:
        # 2순위: 외부 샘플러(반복 경계 없음 — 시간축 기준 근사)
        series = [s["tree_rss_kb"] * 1024 for s in run.samples if "tree_rss_kb" in s]
        source = "외부 샘플러 tree_rss(근사)"
    if len(series) < 10:
        out["slope"] = unknown("반복 RSS 시계열 없음")
        out["verdict"] = unknown("반복 RSS 시계열 없음")
        return out

    tail = series[-50:] if len(series) >= 50 else series
    slope_bytes = linreg_slope(tail)
    slope_kb = slope_bytes / 1024 if slope_bytes is not None else None
    out["slope"] = slope_kb if slope_kb is not None else unknown("회귀 불가")

    head10 = sum(tail[:10]) / 10
    last10 = sum(tail[-10:]) / 10
    growth = (last10 - head10) / head10 if head10 else 0.0
    if slope_kb is not None and abs(slope_kb) < SLOPE_LIMIT_KB_PER_ITER and growth <= TAIL_GROWTH_LIMIT:
        out["verdict"] = f"누수 징후 없음 (기울기 {slope_kb:.1f}KB/iter, 꼬리증가 {growth * 100:.1f}%, 출처 {source})"
    else:
        out["verdict"] = (f"재확인 필요 (기울기 {fmt(slope_kb)}KB/iter, 꼬리증가 {growth * 100:.1f}%, "
                          f"출처 {source}) — 누수 확정 아님, Instruments 확인 필요")

    if run.samples:
        first, last = run.samples[0], run.samples[-1]
        out["threads"] = f"{int(first.get('threads', 0))} → {int(last.get('threads', 0))}"
        out["procs"] = f"{int(first.get('procs', 0))} → {int(last.get('procs', 0))}"
    else:
        out["threads"] = unknown("샘플 없음")
        out["procs"] = unknown("샘플 없음")
    return out


# ── 교차 검증 ────────────────────────────────────────────────────────────────
def crosscheck_rows(runs):
    rows = []
    for key in sorted(runs):
        run = runs[key]
        if not run.exists:
            continue
        internal = run.rss_stage("scenario_end") or run.rss_stage("stable_5s")
        external = max((s["tree_rss_kb"] * 1024 for s in run.samples if "tree_rss_kb" in s), default=None)
        if internal and external:
            delta = (external - internal) / internal * 100
            rows.append([f"{run.renderer}/{run.tag}", f"{internal / MB:.1f}", f"{external / MB:.1f}",
                         f"{delta:+.1f}%"])
        else:
            rows.append([f"{run.renderer}/{run.tag}",
                         f"{internal / MB:.1f}" if internal else unknown("내부 rss 없음"),
                         f"{external / MB:.1f}" if external else unknown("샘플 없음"),
                         "-"])
    return rows


def status_rows(runs):
    rows = []
    for key in sorted(runs):
        run = runs[key]
        meta = run.meta
        rows.append([
            f"{run.renderer}/{run.tag}",
            meta.get("status", unknown("meta 없음")),
            str(meta.get("elapsed_s", "?")),
            str(len(run.events)),
            str(len(run.samples)),
            "alloc" if meta.get("alloc_build") else "-",
        ])
    return rows


# ── 출력 ────────────────────────────────────────────────────────────────────
def md_table(headers, rows):
    out = ["| " + " | ".join(headers) + " |",
           "|" + "|".join(["---"] * len(headers)) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(c) for c in r) + " |")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--art", required=True)
    args = ap.parse_args()

    runs = load(args.art)
    if not runs:
        print("!! JSONL 산출물이 하나도 없다 — 먼저 run/matrix를 실행하라", flush=True)
        return 1

    per = {r: metrics_for(runs, r) for r in RENDERERS}
    keys = list(per["glow"].keys())

    # summary.csv
    csv_path = os.path.join(args.art, "summary.csv")
    with open(csv_path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["metric", "glow", "wgpu"])
        for k in keys:
            w.writerow([k, fmt(per["glow"][k]), fmt(per["wgpu"][k])])

    commit = "unknown"
    try:
        commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                                capture_output=True, text=True).stdout.strip() or "unknown"
    except OSError:
        pass

    md = [
        "# 렌더러 A/B 벤치 요약 (glow vs wgpu)",
        "",
        f"- 생성: {time.strftime('%Y-%m-%d %H:%M:%S')}  커밋: `{commit}`",
        "- 러너: `scripts/render-bench.sh` (직렬 실행, 실행 간 5초 안정화, release 빌드 강제)",
        "- 내부 계측: 앱 JSONL(`DEPPY_RENDER_BENCH`) / 외부 계측: `ps` 샘플러 CSV(1s)",
        "- 값이 없는 항목은 `UNKNOWN(사유)` — 0으로 대체하지 않는다.",
        "",
        "## 요약 표",
        "",
        md_table(["항목", "glow", "wgpu"],
                 [[k, fmt(per["glow"][k]), fmt(per["wgpu"][k])] for k in keys]),
        "",
        "## 실행 상태",
        "",
        md_table(["run", "status", "elapsed(s)", "JSONL 이벤트", "샘플", "빌드"], status_rows(runs)),
        "",
        "## 교차 검증 — 내부 RSS(JSONL) vs 외부 RSS(ps 트리)",
        "",
        "외부 값은 앱+자식 프로세스 트리의 **peak**다. 내부 계측이 자식을 누락하면 delta가 크게 벌어진다.",
        "",
        md_table(["run", "내부 total (MB)", "외부 트리 peak (MB)", "delta"], crosscheck_rows(runs)),
        "",
        "## 누수 판정 규칙 (생성·삭제 100회)",
        "",
        f"- 후반 50회 구간의 RSS **선형회귀 기울기**로 판정한다. |기울기| < {SLOPE_LIMIT_KB_PER_ITER:.0f} KB/iter",
        f"  이고 꼬리 10회 평균 증가가 +{TAIL_GROWTH_LIMIT * 100:.0f}% 이내면 `누수 징후 없음`.",
        "- **RSS가 즉시 내려가지 않는 것만으로 누수라고 단정하지 않는다** — macOS allocator는 free된",
        "  페이지를 OS에 지연 반환하고, warm 풀/스크롤백 캐시는 의도된 보유다(계획서 §6-8).",
        "- 기준을 넘으면 `재확인 필요`이며 누수 '확정'이 아니다. Instruments(Allocations/Leaks) 추적으로",
        "  확인하고 결과를 `artifacts/render-bench/instruments/`에 남긴다.",
        "",
        "## 매트릭스 (돌린 것 / 뺀 것)",
        "",
        "- 돌린 것: `{glow,wgpu}` × `idle`,`bulk` × ws `{1,5,10,20}` + `dirty1`,`fullscreen`(ws1) +",
        "  `switch`(ws5) + `createdelete`(ws1, 100회) = 렌더러당 12회, 총 24회.",
        "- 뺀 것(의도적): dirty1/fullscreen/switch/createdelete의 ws 스케일 확장 — 조합 폭발 방지.",
        "  스케일 민감도는 idle(정지 비용)과 bulk(출력 비용)로 대표한다.",
        "- `selection`: 드래그 자동화 불가 → **수동 실행**(`run <renderer> selection`) 후 집계.",
        "  미실행이면 표에 `UNKNOWN(수동 미실행)`으로 남는다.",
        "",
        "## 원시 데이터",
        "",
        "- `artifacts/render-bench/{glow,wgpu}/<scenario>-ws<N>.jsonl` — 앱 내부 이벤트",
        "- `artifacts/render-bench/{glow,wgpu}/samples-<scenario>-ws<N>.csv` — 외부 ps 샘플",
        "- `artifacts/render-bench/{glow,wgpu}/meta-<scenario>-ws<N>.json` — 실행 메타(상태/종료코드)",
        "- 원시 파일은 용량 때문에 **git에 커밋하지 않는다**. 이 요약(summary.md/csv)만 추적한다.",
        "",
    ]
    md_path = os.path.join(args.art, "summary.md")
    with open(md_path, "w") as fh:
        fh.write("\n".join(md))
    print(f"summary.md / summary.csv 작성 완료 ({len(runs)}개 실행 집계)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
