# Pane·프로젝트 API 구현 및 병렬 코드 리뷰

상태: 구현·병렬 코드 리뷰와 테스트 완료. 2026-10-24. 앱 배포·재시작은 수행하지 않았다.

대상은 [승인된 여섯 PR 작업 단위](../superpowers/plans/2026-10-23-pane-reflow-project-api.md)의 이번 구현이다. canonical 작업 트리는 `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, 브랜치는 `fix/product-review-integration-20261023`, 기준 HEAD는 `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1`이다. 이전 SSH·런처·사용량 변경은 `/tmp/deppy-pane-api-baseline-20261023/`와 대조해 구분했다. 이 문서는 배포 또는 실제 앱 재시작 완료를 의미하지 않는다.

## 요구사항별 구현

| 작업 단위 | 결과 | 확인 범위 |
|---|---|---|
| PR1 분할 종료와 과거 출력 | 실제 split/close·900행 history·Unicode·선택·늦은 ACK·압축/alt/archive 회귀 추가 | 다섯 회귀가 기존 코드에서 통과. 원래 증상은 재현하지 못했으므로 새 terminal 제품 수정이나 원인 확정으로 보고하지 않음 |
| PR2 키 편집 | 560pt 공용 팝업, 12pt 공통 간격, 반응형 두 열, 36pt 필드/34pt 버튼/80pt PEM | 일반 데스크톱 no-scroll, 좁은 화면 fallback, 오류 소유권, 원래 CAS 유지 및 명시적 재불러오기 |
| PR3 API 바로 등록 | 메모·터미널에서 독립 모달, 원래 프로젝트/runtime 고정, 서비스명과 실행 변수 분리 | 메모 요청은 파일트리 sidebar를 경유. 설정 이동 없이 등록, 로딩부터 입력 fence, 취소·오래된 ACK·프로젝트 전환, 선택 이름/값 구분 |
| PR4 API 목록과 권한 | 값 없는 metadata 목록, 별도 opt-in, 세션·세대·환경·프로젝트별 grant와 hashed bearer | SecretStore 조회 없는 목록, Build/StoreOnly 자동 승격 없음, 삭제·회전·권한 회수 격리 |
| PR5 발견과 실행 | 로컬 Claude/Codex/Cursor/Grok 새 실행에 bootstrap, 명시한 최신 키만 child env/PEM에 전달 | 일반 실행·빠른 우측 분할, 실행 receipt, 실제 process 수명에 결합한 정리, selected-output 마스킹 |
| PR6 통합·리뷰 | gpt-6.1-sol/xhigh 서브에이전트 세 명 독립 병렬 리뷰 및 root 재검증 | 관련 전체 5,359 normal PASS. 최종 출력 보완과 DB 포트 분리도 다른 리뷰어와 root가 별도 재검토 |

Kimi의 additive adapter, SSH 프로젝트의 Mac 보관함 접근, Cloud API recipe bridge 및 사용 grant UI는 지원하지 않는다. 해당 경로는 사용 가능한 것처럼 광고하지 않는다. 기존 아카이브/수동 재개 세션에는 새 bootstrap을 주입하지 않는다. Cloud 화면 보기·입력 허용은 API 사용 허가가 아니다.

## 사용 흐름

1. 프로젝트의 메모 또는 터미널을 우클릭해 `API 바로 등록…`을 연다. 선택한 글이 있으면 API 이름 또는 값으로 넣을 수 있다. 창에 표시된 원래 프로젝트·환경을 확인한다.
2. 서비스명과 값을 입력한다. 기본은 보관 전용이다. 에이전트에서 쓰려면 별도 사용 허용을 선택하고 실행 변수명을 입력한다. 설명·문서 URL은 고급 항목에서 추가한다. 기존 Build 키의 사용처가 자동 변경되지는 않는다.
3. 이 bootstrap을 받은 지원 로컬 세션은 값 없는 API 목록을 발견하고, 선택한 키 ID만 사용하는 명령을 실행한다. 같은 세션이 실행 중에 추가한 허용 API도 다음 조회에 반영한다. 키 수정 후 다음 명령은 최신 값을 읽는다. 이미 실행 중인 명령은 시작 시 값을 유지한다.

수정 창에서 값을 비우면 기존 값을 유지한다. 동시 수정 오류는 `최신 상태 다시 불러오기`로 명시적으로 복구하며 작성 중 초안을 임의로 최신 revision에 재결합하지 않는다.

## 병렬 리뷰에서 확인한 결함과 조치

| 영역 | 확인한 결함 | 조치·증거 |
|---|---|---|
| 프로젝트 격리 | 설정 프로젝트 B의 API 권한이 활성 runtime A에 발급 가능 | scope 불일치에서는 bootstrap 생략. 실제 RED→GREEN |
| 빠른 분할 실행 | transient bootstrap 인수 때문에 우측 세션 실행 거절 | 로컬 bounded queue의 pane target으로 전달. wire·저장 인수 변경 없이 실제 PTY 실행 GREEN |
| fallback shell | 에이전트 종료 후 bearer가 shell에 남음 | wrapper에서 reserved bearer 해제. 실제 agent→shell 환경 회귀 RED→GREEN |
| 편집 재불러오기 | ACK 직전/직후 watcher의 오래된 snapshot으로 잘못 복원 | ACK에 정확한 authoritative snapshot 전달. scope·generation·원래 key 검사, RED→GREEN |
| 편집 오류 복구 | 무관한 원본 파일 삭제나 잠긴 보관함 때문에 수정 창 새로고침도 실패 | 별도 metadata-only ReloadEditor. 원본 복구용 Refresh는 유지, 실제 네 회귀 GREEN |
| 메뉴 의도 | 일반 환경설정 메뉴가 API 등록으로 처리되고 SSH는 무반응 | Settings/RegisterApi 명시 구분, SSH 안내 모달과 입력 fence, 실제 제스처/host 회귀 GREEN |
| MCP 호출 | 선택 인수가 없는 유효한 호출에서 arguments 생략 거절 | 생략만 빈 인수로 처리하고 명시 null/잘못된 인수는 거절, RED→GREEN |
| 출력 보호 | 새/교체된 selected API와 PEM 값이 PTY·세션 로그로 출력 가능 | 실행 전 checked corpus, bounded streaming relay. 누출·용량 초과 실제 RED→GREEN |
| 터미널 job control | TOSTOP 상태에서 foreground child의 출력 relay가 실패/정지 가능 | tty write 동안 호출 thread만 SIGTTOU 차단 후 원래 mask 복원, owned PTY RED→GREEN |
| 대화형 입력 안내 | 짧은 prompt를 secret 길이만큼 보류해 입력 대기 | owned PTY에서 prompt를 본 뒤에만 입력하는 회귀가 실제 5초 timeout RED→GREEN. 고정 corpus/KMP cache를 공유하고 가능한 secret prefix와 미완 ANSI만 보류. 기존 StreamRedactor는 그대로 유지 |
| stdout/stderr 경계 | 같은 PTY에서 분리된 secret 조각이 합쳐져 노출 | 실제 PTY·로그 회귀 RED→GREEN. 같은 출력 객체는 자식 출력부터 하나의 ordered stream으로 합쳐 마스킹. 일반 파일 redirect와 서로 다른 출력 대상 보존도 GREEN |
| 구조 경계 | 새 API leaf의 concrete DB 참조 및 정리 worker의 직접 DB 생성으로 boundary 6건 실패 | storage-neutral ProjectApiRepository와 app.rs adapter로 분리. guard/allowlist 변경 없이 최종 boundary PASS, 독립 리뷰 및 전체 App 재검증 PASS |
| 기존 키 호환성 | 일반 DEPPY_API_ 이름을 내부 API로 오인해 Build/LegacyAgent 사용처 변경 | 실제 표시/SaveKey/GitHub 버튼 세 회귀 RED→GREEN. 명시 metadata 우선, fallback은 Vault+StoreOnly+자기 UUID와 정확히 일치할 때만 허용. 기존 사용처·키 전체·CAS·GitHub 동작 유지 |

실행 중인 GUI, 실제 계정 키 또는 Keychain 값은 테스트 fixture에 사용하지 않았다. UI는 합성 snapshot을 사용한 offscreen 렌더와 geometry/제스처 테스트로 확인했다. 실제 앱 화면을 열어 수동 비교했다고 주장하지 않는다.

## 실행한 검증

Cargo는 root가 직렬 실행했다. 공통 조건은 `CARGO_BUILD_JOBS=2`, shared target `/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`, `--offline --locked`이다.

| 검증 | 실제 결과 | 로그 |
|---|---|---|
| core/session/storage/terminal 전체 | 801 PASS, 5 ignored | `/tmp/deppy-pane-api-leaf-tests.log` |
| runtime 전체 | 462 PASS, 1 ignored | `/tmp/deppy-pane-api-runtime-mcp-final1.log` |
| agent-mcp 전체 | 99 PASS, 1 ignored | 같은 로그. 별도 child fixture 1 PASS는 집계 제외 |
| gate6 UI | 49 PASS, 1 ignored | `/tmp/deppy-gate6-ui__project_keys__tests.log` |
| gate6 worker | 11 PASS | `/tmp/deppy-gate6-project_keys__worker__tests.log` |
| gate6 selected API/TOSTOP | 7 PASS | `/tmp/deppy-gate6-project_api_selected_.log` |
| inherited timeout/deadline | 1 PASS | `/tmp/deppy-gate6-selected_inherited_output_preserves_timeout_and_explicit_deadline_semantics.log` |
| 최종 selected API / fixed stream / 기존 키 | 11 / 6 / 3 PASS | `/tmp/deppy-gate8-{app-selected,secret-fixed,app-prefix}.log` |
| Full App | 3,872 unit + 39 integration = 3,911 normal PASS, 49 ignored | `/tmp/deppy-pane-api-app-final2.log` (DB 포트 분리 후 재실행, unit 50.39초) |
| Full secret | 86 PASS | `/tmp/deppy-pane-api-app-secret-final.log` |
| 합계 | 5,359 normal PASS, 56 ignored | 전체 suite 합계. focused 중복 실행과 별도 child fixture는 합계 제외 |
| 최종 offscreen 이미지 생성 | 별도 1 PASS, PNG 4장 | `/tmp/deppy-pane-api-offscreen-final.log`, root가 edit/API 등록 이미지를 직접 확인 |
| strict Clippy / all targets / -D warnings | PASS 50.90초 | `/tmp/deppy-pane-api-clippy-final3.log` (DB 포트 분리 후 재실행) |
| fmt check | PASS | `/tmp/deppy-pane-api-fmt-final2.log` |
| boundary | PASS | `/tmp/deppy-pane-api-boundary-final2.log` |
| usage source guard / diff check | PASS | `/tmp/deppy-pane-api-usage-guard-final.log`, `/tmp/deppy-pane-api-diff-final.log`. 보호 seal 갱신 없음 |

위 ignored는 기존 수동/live/성능/이미지 내보내기 사례이며 통과 건수에 포함하지 않는다. 이번 결함 회귀를 ignore하거나 assertion을 제거하지 않았다.

첫 전체 App 실행에서는 공용 test fixture를 바꿔 기존 wheel 회귀가 실패했다. focused 실행에서도 재현한 뒤 기본 동작을 복원하고, 새 split/close 회귀에서만 survivor 고정을 선택하도록 수정했다. 최종 전체 App는 위와 같이 통과했다. 첫 Clippy의 일곱 style/layout 오류도 suppression 없이 식 정리와 큰 payload boxing으로 수정했다. 초기 출력 누출 RED 시도 중 fixture의 잘못된 StoreOnly+required 설정으로 실패한 것은 유효한 누출 증거로 세지 않았고, setup을 바로잡은 실제 경계 실패만 RED로 기록했다.

## 설계상 경계

- 실행 중 process는 시작 시 선택한 키 snapshot을 유지하고, 다음 실행이 최신 revision을 읽는다.
- process guard 해제는 in-memory bootstrap 참조를 즉시 제거한다. DB capability/grant 회수와 임시 bootstrap 파일 정리는 bounded worker가 재시도하므로 durable 회수는 비동기다.
- 새 로컬 세션의 API capability는 최대 24시간이다. 만료된 권한을 기존 세션에서 자동 갱신하는 경로는 이번 범위에 포함하지 않는다.
- relay는 자식 pipe만 nonblocking으로 만들고 상위 stdout/stderr fd flag는 변경하지 않는다. 같은 blocking 출력 대상에 관계없는 외부 writer가 경쟁하는 OS race까지 완전히 제거했다고 주장하지 않는다.
- 원래 terminal 현상은 재현하지 못했다. 명시적 CRLF와 TUI 표의 과거 행을 추측해서 합치는 변경은 하지 않았다.
- 버전0.18.0은 테스트 소스 상태다. 변경 앱을 사용자에게 배포하려면 0.18.0보다 높은 버전으로 증가하고 실행 파일·macOS 두 bundle version을 검증해야 한다. 이번 작업에서는 앱을 중지·시작·재시작하거나 배포하지 않았다.

## 보존한 증거와 전달 상태

- [검증 증거·소스 SHA256 목록](assets/pane-project-api-20261024/README.md)에 현재 작업 소스 48개와 baseline digest, lossless gzip 로그, 합성 UI 이미지 네 장을 보존했다. [edit](assets/pane-project-api-20261024/edit.png) / [API 바로 등록](assets/pane-project-api-20261024/api-quick-add.png) / [desktop](assets/pane-project-api-20261024/desktop.png) / [narrow](assets/pane-project-api-20261024/narrow.png).
- 여섯 PR은 작업 분할 단위다. 이번 구현 단계에서 실제 git commit·push·GitHub PR 생성·main 병합은 하지 않았다. 기존 미커밋 작업과 섞어 일괄 커밋하지 않았다.
- 소스 구현 및 요청한 병렬 코드 리뷰는 마쳤다. 지원 경계와 재현되지 않은 terminal 증상은 위와 같이 남긴다. 실행 중 앱에는 이 변경을 아직 배포하지 않았다.
