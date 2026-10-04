# 코드 리뷰 및 팝업26–29 — 2026-09-30

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | app.rs / 폴더 이동 접수 | 다른 설정 작업으로 큐가 바쁜데 안내를 폐기 | 경로 갱신 기회가 사라짐 | Update 비활성화·동일 대상 유지·실패 재시도 적용 |
| medium | app.rs / 폴더 이동 완료 | 늦은 결과가 현재 안내를 무조건 정리 | 다른 프로젝트의 새 안내가 사라짐 | 원래 대상·generation/revision/workspace ID 일치 검사 적용 |

## 검토 범위와 결론

기존 미커밋 팝업 변경(입력 차단, Esc 소유권, 생성 오류 위치, 폴더 재열기 런처, MCP 서비스 모달 우선)을 다시 읽고, 신규 문서 큐·저장 eligibility/continuation·폴더 이동 worker admission/CAS·tab header geometry와 클릭 대상·IME focus를 검토했다. 위 두 문제가 추가로 확인됐으며 수정했다. 이 범위에 남은 확인된 오류는 없다.

- 폴더 안내는 감지 당시 workspace ID, 옛/새 경로 및 inode anchor를 보존한다. 실제 queue acceptance 후 operation key를 저장하고 중복 입력·닫기를 막는다. 실패는 원래 값과 Error notice를 유지하고 다시 시도한다. settings navigation이 아직 worker에 보내지 않은 job을 폐기하면 그 안내를 해제해 재시도한다. 다른 workspace의 이전 결과는 해당 projection만 갱신한다. 저장 CAS는 그대로 유지한다.
- 팝업26–29는 `ui/document_dialogs.rs`에 표시 코드만 분리했다. 공용 shell/body/footer/notice/action_button을 재사용한다. App의 문서 앞 큐 ID, can_save_then_close와 async 저장 후 닫기, reload 및 cap 판단은 유지했다. 문서 확인이 있는 프레임에는 cap/path 안내를 동시에 표시하지 않는다. 파일명·대기 수·저장 제한과 경로가 보이고, dismiss는 위험 동작을 실행하지 않는다.
- Modal ID는 사례별 상수다. 열었던 문서 수마다 새로운 Area 상태를 남기지 않는다. 폴더 경로 전체를 렌더 프레임마다 clone하지 않고 참조한다. UI에서 새 디스크/네트워크 대기나 반복 repaint를 추가하지 않았다. 새 결정 함수는 표시 동안만 label Strings를 만들고 보관하지 않는다. 정량 메모리 측정을 했다는 주장은 아니다.
- X 옆 divider는 기존 paint_tab_divider를 재사용하며 보조 탭이 없어도 표시된다. 빈 영역은 마지막 보이는 탭부터 toolbar 시작까지 별도 hit box다. 기존 탭 선택/닫기/toolbar와 우클릭을 보존한다. 세션 생성 대신 기존 launcher 요청만 전달한다. 비활성 입력 surface는 focus 요청만 하고, 활성 클릭도 기존 명시적 pane focus를 유지한다.

## 실제 실행한 검증

- 탭 빈 클릭 RED: 기존 구현에서 launcher 요청이 없어서 실패. 수정 후 GREEN. 최종 `tab_strip`: **4 passed**, 빈 클릭·비활성 입력·세션 없는 헤더·실제 divider paint.
- `document_popups`: **10 passed / 1 ignored**. 실제 Save/Discard/Cancel, 비활성 Save+제한+큐 안내, X/바깥 클릭/Esc, Reload/Cancel, cap 닫기, 폴더 busy/submitting/retry/ignore, 긴 경로와280×360 화면. App operation settlement 검사2개 포함.
- 첫 전체 App: **2488 passed / 1 failed / 28 ignored**. 기존 IME 명시적 pane 클릭이 새 빈 영역에서 focus claim을 잃는 회귀를 잡았다. focus를 유지하도록 수정한 뒤 해당 검사1개 통과. 최종 전체 App: **2489 passed / 0 failed / 28 ignored**,45.67s. 첫 실패를 통과로 집계하지 않았다.
- Connector UI **17 passed**, i18n **8 passed**. strict App/Connector all-targets Clippy 통과. 최초 collapsible_if 지적을 수정했고 최종 재실행 통과. fmt/diff 검사 및 추출 HTML JS Node 문법 검사 통과.
- ignored PNG renderer **1 passed**(26–29 네 장,1.72s); 네 이미지 모두 실제 시각 검토. 생성물 `target/popup-parity/26-document.png` 등. 동일한 운영 표시 함수를 사용하며 앱을 실행하지 않는다.

## 한계 및 릴리스

실제 실행 중인0.4.5 앱, 네이티브 선택창, 실제 사용자 문서 버리기·덮어쓰기·폴더 이동은 수행하지 않았다. 위젯의 의도와 App의 큐/CAS/worker 경로를 코드·회귀 테스트로 검사했다. 앱 재실행·커밋·푸시 권한은 이번 요청에 없다.

0.4.9→0.4.10 버전 증가,27 workspace metadata/Cargo.lock 값 확인 완료. 릴리스 빌드25.08s 통과. 별도 Developer ID 서명 `target/bundle-0.4.10/Deppy Sijo.app`/ZIP의 로컬 개발 검증 및 압축 해제 후 검증 통과. 두 plist 버전과 컴파일된 reported-version marker0.4.10 확인. 앱 실행·재실행은 하지 않았다. Notarization 검증은 포함하지 않는다. 소스 base166f8daeb1054cf09a07194fa83bf0a4a19d93ce + product diff/new popup files SHA256 `29d7f977cf3f10e0f897ee405ec280efe5275e46f46e72aaf5d2b158db7b42a5`.
