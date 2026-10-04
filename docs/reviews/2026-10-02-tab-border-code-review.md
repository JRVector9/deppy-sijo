# 탭 경계선 코드 리뷰 — 2026-10-02

## 결과

최근 0.5.3의 상단선·X 오른쪽 세로선 연결 변경에서 확인된 수정 필요 결함은 없다. 독립 Codex CLI 소스 리뷰도 동일한 결론이다. 이는 아래 범위에 대한 결과이며, 작업 트리에 누적된 다른 기능을 전체 재검증한 결과는 아니다.

## 검토 범위와 근거

- 실제 변경: `crates/app/src/ui/workspace.rs`의 `paint_pane_header_base`(1312), `paint_tab_divider`(1350), 세션 없는 헤더 호출(6227), 일반 헤더 호출(6460), 기존 paint 회귀 테스트(16483).
- 변경 전 파일 `/tmp/deppy-tab-border-before-20261002/crates/app/src/ui/workspace.rs`와 범위 한정 diff `/tmp/deppy-tab-border-task-diff-20261002.patch`를 비교했다.
- 포커스된 헤더는 동일 `active_stroke`를 사용한다. 세션 탭이 선택되면 가로선 오른쪽 끝과 세로선 x가 같은 스냅 함수를 사용하고, 두 선의 시작 y도 공유한다.
- 비포커스 헤더는 기존 상단 강조선 없는 상태와 중립 세로선을 유지한다. 보조 탭을 선택하면 상단 강조선은 해당 탭으로 이동하는 기존 규칙을 유지한다. 두 호출부가 같은 스타일을 전달한다.
- `render_node`의 pane 클립, 좁은 헤더의 경계 검사, 보조 탭 배치와 페인트 순서를 읽었다. 새로운 선 좌표 때문에 이웃 pane을 침범하는 확인된 회귀는 없다.
- 클릭·닫기·런처의 interact 영역과 명령 경로는 이번 diff에서 변경하지 않았다.
- 새로운 캐시·타이머·동적 목록·I/O가 없다. 기존 두 선의 스타일/좌표 계산만 바뀌어, 이번 변경에서 확인된 메모리 누적 또는 불필요한 프레임 작업은 없다. 실제 RSS/GPU 성능 측정은 수행하지 않았다.
- Cargo.lock의 실제 egui 0.36.1 소스를 확인했다. `set_pixels_per_point`는 다음 pass에 적용되므로 기존 테스트의 다섯 배율 설정이 적용되는 경로다. epaint의 선 스냅/끝점 처리도 검토했다.

## 이번 리뷰에서 실제 실행한 검증

| 검증 | 결과 |
| --- | --- |
| App `tab_strip` | 4 passed / 0 failed, 0.03s |
| App `pane_header` | 1 passed / 0 failed, 0.00s |
| 독립 Codex CLI 소스 리뷰 | exit 0, 확인된 actionable regression 없음 |
| `git diff --check` | exit 0 |
| 제품 코드 무변경 검증 | 기존 0.5.3 소스 SHA256과 일치 |

`tab_strip`에는 실제 paint의 동일 stroke/공유 끝점/전체 높이를 1, 1.25, 1.5, 2, 3배율로 확인하는 테스트 한 개와 기존 탭 동작 테스트가 포함된다. 배율 다섯 개를 테스트 다섯 개로 계산하지 않았다.

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo tab_strip
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo pane_header
codex review -c 'sandbox_mode="read-only"' - < /tmp/deppy-tab-border-user-review-prompt-20261002.txt
git diff --check
```

증거: `/tmp/deppy-tab-border-user-review-{tests,header-tests,cli}-20261002.log`, `/tmp/deppy-tab-border-user-review-integrity-20261002.json`.

이번 리뷰에서는 전체 App 테스트, Clippy, 릴리스 빌드, 실행 앱 화면 검사, GPU 래스터 비교를 반복하지 않았다. 기존 구현 단계의 결과는 별도 `2026-10-02-tab-border-continuity.md`에 있다.

## 저장소·앱 상태

Performance worktree, `fix/cloud-agent-ended-sessions`, base `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`. 기존 변경을 보존했고 이번에는 리뷰 문서와 handoff만 기록했다.

제품 diff SHA256은 **`caad16d87d931151687d5bf6972f043e4e72e28c36a6f631dfad436733074557`**로 리뷰 전 0.5.3 소스와 같다. 제품 코드 수정·재빌드·버전 변경·커밋·푸시는 없었다. 실행 중인 0.5.2 PID96463도 종료하거나 재실행하지 않았다.
