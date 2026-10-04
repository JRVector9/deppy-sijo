# 탭 X 오른쪽 선 연결 수정 — 2026-10-02

## 완료

`ui/workspace.rs`의 header paint 경로만 수정했다. 포커스된 탭의 상단선과 X 오른쪽 세로선은 동일한 `active_stroke` 색·두께를 사용한다. 상단선의 오른쪽 끝과 세로선 x를 같은 픽셀 격자로 맞추고, 세로선을 상단선 중심 좌표부터 헤더 하단까지 그린다. 기존6pt 상단·하단 빈 공간을 제거했다. 비포커스 neutral divider, 보조 탭 선택, 탭 클릭/런처·닫기·layout 동작은 유지했다.

원인: 상단은 workspace identity stroke, 세로는 별도 회색 separator였고 위아래6pt를 비웠다. 또한 세로 x만 스냅하고 가로 끝은 raw 좌표여서 교차점도 달랐다.

## 실제 검증

기존 실제 paint harness를 강화했다. 동일 stroke·정확히 공유하는 끝점·헤더 전체 높이를 확인하며,1/1.25/1.5/2/3의5개 배율을 한 테스트 안에서 실행한다.

| 검증 | 실행 결과 |
| --- | --- |
| 수정 전 paint 회귀 | RED: 상단과 세로 stroke 불일치 |
| `tab_strip` | 4 passed / 0 failed, 0.05s; paint5배율·빈 클릭·비활성 초점·세션 없는 헤더 포함 |
| `pane_header` | 1 passed / 0 failed, 0.00s |
| App Clippy all-targets | exit0, 38.02s |
| fmt / diff | exit0 |
| 독립 범위 한정 Codex 소스 리뷰 | exit0, 확인된 미해결 결함 없음 |
| App/proxy release build | exit0, 26.56s |
| bundle/추출 ZIP 버전·서명 검사 | exit0 |

첫 RED assertion 중 egui TexturesDelta가 처리되지 않은 채 unwind해 별도 destructor panic이 발생했다. 추가 assertion 전에 명시적으로 delta를 버리도록 테스트 정리를 수정하고, 제품 코드를 바꾸기 전에 예상 mismatch에서 정상적인 RED를 다시 확인했다. 다른 실패한 구현 접근은 없었다. 전체 App/실행 앱 E2E/메모리 벤치는 반복하지 않았다.

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo tab_strip
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo pane_header
cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

로그 `/tmp/deppy-tab-border-{red,green,header-tests,clippy,cli-review,release,package}-20261002.log`. 리뷰는 이전 source snapshot `/tmp/deppy-tab-border-before-20261002`와 해당 파일의 task diff `/tmp/deppy-tab-border-task-diff-20261002.patch`를 사용했다. 다른 누적 dirty 변경을 새로 검증했다는 의미가 아니다.

## 릴리스

- **0.5.2 → 0.5.3**, 기존 artifact 최대0.5.2와 27개 inherited workspace/lock 버전 확인.
- 별도 Developer ID signed `target/bundle-0.5.3/Deppy Sijo.app`, ZIP. 컴파일된 `Deppy0.5.3` marker, plist 두 버전0.5.3, nested signature·architecture·추출 ZIP 바이너리 일치 확인. 로컬 개발 검증이며 Apple 공증 완료를 주장하지 않는다.
- Base `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, branch `fix/cloud-agent-ended-sessions`. 기존 dirty 작업 보존, 커밋/푸시 없음.
- Product diff SHA256 **`caad16d87d931151687d5bf6972f043e4e72e28c36a6f631dfad436733074557`**. Cargo.toml/lock, app/storage/i18n/connector-ui/runtime/session binary diff와 sorted untracked product path+NUL+bytes를 결합했다. 빌드 뒤 동일 hash를 검증했다. 기록 `/tmp/deppy-tab-border-release-source-20261002.json`.
- 실행 중인0.5.2 PID96463은 유지했다. **재실행하지 않았다.**
