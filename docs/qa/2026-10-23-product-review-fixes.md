# 제품 결함 수정과 통합 검증 — 2026-10-23

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `command_center_main_tools.rs`, `command_center_main_host.rs` | 종료된 MAIN 슬롯·등록·작업·guard가 한도에 남음 | 반복 실행 후 32/128/256 한도 소진 | 폐기 상태 회수·활성 guard만 계산, 회귀 32건 통과 |
| high | `command_center_host.rs` 및 실행 어댑터 | SSH 대상에 로컬 실행·입력·Git 부수 효과 허용 | 원격 경로를 Mac 권한으로 취급할 가능성 | 소유 대상과 실제 runtime 재검사, 회귀 10건 통과 |
| medium | `app.rs` | Command Center 조기 반환이 토큰 정리를 건너뜀 | 설정 종료·이동 후 표시 토큰 잔존 | 공통 early/late 정리, 실제 App 회귀 2건 통과 |
| medium | 설정·클라우드 UI 27경로 | 승인됐던 변경이 다른 작업본에만 존재 | 통합 시 승인 UI 누락 | 27경로와 별도 lifecycle 회귀를 합친 28경로 보존 |

## 작업과 리뷰

GPT-6.1-Sol xhigh 서브에이전트 3개가 별도 worktree에서 MAIN, 설정, SSH 통합을 담당했다. Root가 변경 소유권을 분리하고 실제 RED/GREEN 실행과 최종 통합을 담당했다. MAIN·설정 교차 검토와 root 검토를 수행했으며, 교차 검토에서 발견한 CC Git 우회와 대기 중 수동 검증 결과의 대상 변경도 재현 후 수정했다.

- MAIN: 현재 권한의 replay/unknown 기록과 동시 용량 제한은 유지한다. 현재 설정뿐 아니라 실제 설치 revision도 보존해 저장 실패 후 durable 설정 복구 시 설치된 Templates가 계속 동작한다. 진짜 폐기된 Arc는 재생성된 credential을 소비할 수 없다. 추가 host guard 256회 결함도 실제 call로 재현하고, 폐기 guard를 한도 계산에서만 제외했다. 폐기 permit·replay tombstone은 유지하며 전체 host의 기존 1,024개 replay 기록 한도는 유지한다.
- 설정: 원본에서 승인된 변경을 그대로 보존하고 App·locale의 겹치는 부분만 결합했다. 표시 토큰은 CloudAgents 설정을 벗어나면 정리한다. 원본의 버전 하향·무관한 문서는 가져오지 않았다.
- SSH: 기존 OpenSSH·원격 터미널·파일·Git RPC 경로를 유지한다. Command Center의 로컬 실행은 Local metadata와 실제 Local runtime이 일치할 때만 허용한다. SSH/Offline에는 아직 지원하지 않는 CC 실행을 명시적으로 거절한다. Git 읽기는 원격 RPC를 유지하고 입력·셸 생성·Finder 부수 효과는 거절한다.
- wire36 유지, 구형31 handshake 거절 회귀를 추가했다. 실제 Linux helper 쌍 재빌드·seal·배포는 이번 소스 수정에 포함하지 않았다.

## 실제 확인한 focused 결과

| 검사 | 결과 | 증거 label |
| --- | --- | --- |
| MAIN 수명 RED → GREEN | 초기 3실패·추가 host 1실패 → 전체 32통과 | `main-lifecycle-red`, `main-host-guard-red`, `main-host-guard-green` |
| 실제 App 토큰 수명 RED → GREEN | 2실패 → 2통과 | `settings-token-red-fresh`, `settings-token-green` |
| SSH 호출·catalog·UI | 4실패 → 5통과 | `ssh-behavior-red-fixed`, `ssh-behavior-green` |
| Git·수동 증거 후속 RED → GREEN | 기존 5통과/신규 5실패 → 10통과 | `ssh-git-manual-red`, `ssh-final-green` |
| 클라우드 설정 | 229통과, 3ignored | `cloud-ui-green` |
| 공통 UI / i18n | 35 / 8통과 | `ui-i18n` (중첩 child 1건 제외) |
| 명시적 native renderer | 1통과, root 대표 이미지 4개 확인 | `settings-native-render`, `native-image-review.json` |
| PWA Command Center browser | 265checks 통과 | `browser-command-center` |
| helper packaging/deadline Python | 21통과 | `integrated-python` |
| SSH wrapper 종료 동기화 | 3통과, 엄격한 EOF 처리 유지 | `ssh-wrapper-fixture-green` |
| MCP bounded remainder 정리 | 4통과, 제품 close 로직 변경 없음 | `mcp-cleanup-fixture-green` |

결과를 서로 더해 고유 테스트 개수로 취급하지 않는다. 최종 통합 workspace 결과는 아래에 별도로 기록한다. 각 실행의 정확한 argv/cwd/exit/시간/로그 SHA256은 [실행 증거](evidence/product-review-fixes-20261023)에 있다.

## 최종 통합 게이트

최종 검증 소스는 `06db5a9e566a85ab8fc1baf92259ba167d2572f2`이며, 소스·asset 1,072개 SHA256을 고정했다.

| 최종 검사 | 실제 결과 | 증거 label |
| --- | --- | --- |
| 전체 workspace 단위·통합·문서 테스트 | **6,405통과 / 0실패 / 76ignored**, 154.84초 | `integrated-workspace-final-pass` |
| 엄격한 Clippy, workspace/all-targets | 통과, 경고를 오류로 처리 | `integrated-clippy-final` |
| fmt 전체 | 통과 | `integrated-fmt-final` |
| 제품 UI 경계 / 의존성 | 통과 / 30crate DAG 통과 | `integrated-boundary-green`, `integrated-deps` |
| i18n | 5로케일·리터럴 1,748참조 통과, 동적 182참조는 정적 검사 범위 밖 | `integrated-i18n` |
| 증거 재검증 | 소스1,072개·원본 로그34개·최종 gate9개·기존 ref301개 통과 | `verify.py` |

95개 Cargo harness마다 마지막 summary만 집계해 중첩 child summary 3개를 제외했다. focused 결과를 더해 고유 통과 개수를 부풀리지 않았다. 전체 검사 명령은 `cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=4`, Clippy는 `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`다. Cargo는 root가 jobs2로 직렬 실행했다. 76ignored는 통과로 세지 않았고, 별도 native renderer1건과 독립 browser265checks는 위 focused 결과로 구분한다.

최초 whole 검사에서 MCP fixture의 macOS socket timeout 설정 순서가 실패했다. 제품 코드를 바꾸지 않고 peer의 수명을 동기화한 뒤 focused4통과했다. 두 번째 whole 검사에서는 App3691통과/1실패/46ignored였고, SSH fixture가 queued frame 전송 전에 shutdown하는 경합을 확인했다. 정확한 backend barrier 응답 후 종료하도록 테스트만 고쳐 wrapper3통과했다. 실패 로그는 삭제하지 않았다. 세 번째 no-fail-fast 실행은 6,404통과/1실패/76ignored였으며, 유일한 실패는 SSH fixture의 PID 파일 create-before-write 경합이었다. 임시 PID 파일을 쓰고 닫은 뒤 atomic rename으로 공개하도록 생성 스크립트만 바꿨다. 기존 준비 기한·정수 parse·취소·타임아웃·자식 회수 assertion을 유지한 채 worker22통과를 확인하고, 마지막 전체 실행에서 6,405통과/0실패를 확인했다.

경계검사의 9건은 cfg(test)에서 include되는 두 fixture를 독립 제품 소스로 재검사한 오탐이었다. 두 파일을 내용 변경 없이 `workspace_ui_test_support/`로 옮기고 cfg(test) include 경로 두 곳만 바꿨다. 검사 규칙은 변경하지 않았으며 실제 경계검사와 의존성검사는 통과했다.

## 보존과 실행 범위

원래 branch/ref 301개와 settings/SSH/CC 원본 HEAD 및 원본 파일 914/957/1023개가 그대로인 것을 다시 확인했다 (`original-preservation.json`). main 이동, 원격 push, 사용자 앱 종료·시작·재시작, 실제 사용자 세션 입력, paid provider turn, 릴리스 배포는 하지 않았다. 소스 0.15.1은 미배포 검증용이다. 향후 앱 전달에는 새 버전 및 bundle/binary/helper 버전 검증이 필요하다.

원래 상태로 돌아가는 기준은 기존 보존된 main 및 각 feature branch이며, 현재 수정본은 `fix/product-review-integration-20261023`에 있다. 실제 모바일 기기·실제 사용자 SSH 서버에서의 E2E는 수행하지 않았다.

## 커밋과 재확인

| 범위 | 로컬 커밋 |
| --- | --- |
| MAIN 슬롯·등록·작업 수명 | `17195e51` |
| 승인 설정·클라우드 UI 보존과 토큰 정리 | `c777e160` |
| SSH 통합·로컬 권한 경계 | `37d82b5c` → 통합 `66f0aef1` |
| MCP / SSH 종료 fixture 동기화 | `92bbf634`, `afe60070` |
| 추가 MAIN host guard 한도 | `9fd07d0d` |
| 테스트 전용 파일 재배치 / PID 전달 | `a1fc8f49`, `06db5a9e` |

정확한 변경110경로는 [변경 경로](evidence/product-review-fixes-20261023/changed-product-paths.json), 최종 gate와 범위는 [최종 결과](evidence/product-review-fixes-20261023/final-gates.json)에 있다. 110경로에는 기존 SSH 구현의 병합과 보존된 설정이 포함되며 110개의 신규 결함을 의미하지 않는다. 읽기용 로그는 후행 공백을 정리했고, 원본 바이트는 `.log.gz`와 SHA256으로 보존했다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-integration-20261023
python3 docs/qa/evidence/product-review-fixes-20261023/verify.py
```

이 스크립트는 저장된 증거·현재 소스·ref를 대조하며 테스트나 앱을 재실행하지 않는다. 최종 report/handoff 이후 제품 소스 변경은 없다.
