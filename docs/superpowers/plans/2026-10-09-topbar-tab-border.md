# 탭바 상단·우측 라인 적용 계획

> **For agentic workers:** Execute inline in this session; this small painting change does not require parallel agents. Steps use checkbox syntax for tracking.

**Goal:** 승인한 HTML의 선택100%/비선택24%/hover36% 선 규칙을 세션·보조 탭에 공통 적용한다.

**Architecture:** `workspace.rs`의 헤더 바탕은 배경만 그리고, 각 탭의 실제 rect에 공통 선 함수를 호출한다. 기존 pane 포커스/워크스페이스 색과 픽셀 스냅을 유지한다. 선택 여부는 배치 결과가 아니라 기존 실제 활성 상태를 사용한다.

**Tech Stack:** Rust/egui, 기존 egui_kittest, 직렬 offline Cargo gate.

## 범위와 순서

- [x] `crates/app/src/ui/workspace.rs`의 `paint_pane_header_base`에서 전체 선택선 그리기를 분리하고 `paint_tab_divider`를 공통 탭 선 함수로 대체한다.

```rust
const PANE_HEADER_TAB_INACTIVE_ALPHA: f32 = 0.24;
const PANE_HEADER_TAB_HOVER_ALPHA: f32 = 0.36;
// Selected keeps the original stroke; inactive/hover multiplies its alpha.
// One tab owns its top and right, using existing snapped coordinates.
```

- [x] 세션 없는 스트립·일반 세션·보조 탭·단일 attached 헤더의 실제 탭 rect에 같은 그리기 함수를 사용한다. 세션 X 뒤의 별도 강조선은 제거한다. 제목 hover의 기본 색·닫기·빈 영역·본문 라우팅·높이27pt는 기존 계약을 유지한다.
- [x] 기존 hover 테스트가 첫 선을 선택 선으로 가정하던 부분만 선택 타이틀을 실제로 덮는 상단선으로 찾도록 수정한다. 새 테스트는 추가하지 않는다. 이 낮은 영향도의 스타일 변경은 기존 실제 렌더링·hover·스냅·blank-hit 검사로 검증한다.
- [x] 기존 `tab_title_hover`, `tab_strip`, `designall_pane_header`, 전체 App 검사와 strict App all-target Clippy·fmt·boundary를 직렬 gate에서 실행한다.

```sh
RUST_TEST_THREADS=1 python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","tab_title_hover"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","tab_strip"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","designall_pane_header"]]'
```

- [x] root가 변경된 Rust 소스만 `codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh -s read-only`로 직접 리뷰한다. 결과 CONCLUSION: OK; 추가 수정 지적 없음.
- [x] 승인한 spec·결과·handoff와 옵시디언 작업 일지를 갱신한다. 소스 버전0.8.7 유지: 제품 빌드 전달·재실행·push는 이번 요청 범위에 없다.

## 실제 실행 결과

- `/private/tmp/deppy-topbar-focused-20261009.log`: hover3/tab-strip6/header-style1 모두PASS, gate exit0.
- `/private/tmp/deppy-topbar-full-20261009.log`: App2809PASS/38ignored, strict App all-target Clippy·fmt·boundary PASS, gate exit0.
- `/private/tmp/deppy-topbar-review-20261009.txt`: 변경 Rust에 대한 read-only gpt-6.1-sol/xhigh 리뷰 CONCLUSION: OK. 리뷰 CLI는 테스트를 재실행하지 않았다.
- 소스/캐시 diff check PASS. 소스 커밋 제목 `fix(ui): 탭별 상단과 우측 라인 통일`; 정확한 SHA는 `git log -1 --format='%H %s' --grep='탭별 상단과 우측 라인 통일'`로 확인한다.
