# 중앙 입력 창과 예약 작업 추론 강도 (0.7.0)

## 구현

- Fleet 작업 설명: Claude/Codex의 큰 도구 출력 뒤 실제 사용자 지시가 작은 상태 꼬리에서 빠지는 경우를 재현하고, 동일 파일 핸들/EOF 기준 최대4MiB에서 복구한다. 상태·모델은 기존 최신 꼬리를 유지한다. 내용이 확인되지 않으면 작업 설명 수신 대기라고 표시한다.
- 공용 표시 컴포넌트는 connector-ui로 한 번 이동하며 App이 기존 경로에서 재공개한다. 모달 입력 펜스는 같은 구현을 사용한다. 이동형 창은 native 중앙 pivot/Resize 상태로 가운데 열리고 이동·크기 조절·재열기 크기 보존을 지원한다.
-18–20·06·22 공용 창/본문/필드/푸터,30–32 공용 셸과 여백. 기본 런처·OS 선택창·관련 없는 승인창 유지.
- 다음 단계: 실제 모델명 +36pt 추론 강도 선택. 기본 현재 설정 유지. Codex/Claude의 검증된 변경 경로만 사용하며 Grok/Kimi는 읽기 전용. 원래 세션/런타임/실행/예약 ID와 폐기 가능한 입력 permit을 유지한다.
- 설정은 새 턴 완료 이후 적용. Codex는 로컬 런타임의 일회성8KiB 현재 화면 조회, Claude는 입력 작업별8KiB 새 출력 확인을 같은 worker에서 처리한다. 비활성 워크스페이스에도 적용되며 GUI 스냅샷·원격 lease·추가 AI 프로세스·wire 변경을 만들지 않는다. 한 예약당 진행 중 조회 하나, 최대256개. 응답2초·준비/확인20초 제한, 취소·교체·종료 시 원래 권한을 폐기한다.
- Codex는 실제 강도에서 한 CSI 단계씩 적용하고 목표에 가까워진 것이 확인될 때만 다음 단계를 보낸다. 현재Ultra는 계산에 보존하고 자동 목표Ultra는 제외한다. Claude는 절대 `/effort` 명령 후 새 확인 출력만 사용한다. 거절/Unknown/확인 실패이면 예약을 보존·차단하고 알린다. Claude 기본값 저장 가능성은 폼에 안내한다.

## 실제 검증

- 실제 파서/UI RED→GREEN:300KiB 도구 출력 뒤 지시 누락, assistant 문장 오인, 구형 창 중앙 배치, 잘못된 모델 행 높이로 선택 메뉴 가림. 처음 측정 크기 반복 보정은 지속 repaint를 유발해 제거하고 native pivot 사용.
- 최종 수정분 게이트 `/tmp/deppy-centered-final-codex-corrected-gates-20261005.log`, exit0: App2773 +Connector22 +i18n8 +Runtime344 = **3147passed/0failed/33existingignored**. App 및 Runtime에 private/test-owned PTY 사용. 직접 사용자 PTY·파일·클립보드·계정에 입력하지 않음.
- affected all-target Clippy `-D warnings`, UI capability boundary,27-crate dependency boundary, fmt, git diff --check 모두 통과. 최초 dependency gate 실패는 공용 UI test dev dependency strict list 수정 후 해결.
- 실제 egui 테스트: 중앙 열기, 제목 이동, native 크기 조절, 크기 유지·중앙 재열기, 좁은 화면 버튼, 실제 선택→원래 예약 intent, invalid effort 저장 차단. private Warm runtime의 일회성 현재 화면 조회 및 설정 접수 뒤 원래 프롬프트/permit 유지 검증. 완료 세대 보존·기한 독립성, 예전 일치 확인 출력의 제외, 취소 이후 새 출력에도 캡처 중단,8KiB 상한·버퍼 반환도 실제 검증.
- HTML 최초12개 브라우저 QA와 실제 native Chrome 표시 검증. Native Deppy UI는 재실행하지 않아 화면·전체 프로세스 RSS·GPU·실제 사용자 CLI end-to-end 성공을 주장하지 않는다.

## 독립 코드 리뷰

- 초기 gpt-6.1-sol/xhigh source-only 리뷰:2High+3Medium, 모두 수정. 무변경 Attention에서 확인/기한이 멈춤; 과거 강도로 설정 적용 생략; warm 확인 불가; 현재Ultra가 사다리에서 빠짐; 무효 선택이None으로 바뀜.
- 수정분 집중 재리뷰에서2건 추가 확인: 완료 알림을 읽으면 설정 후속 처리가 멈춤(High), Claude 확인 개수가 스크롤 후 증가하지 않을 수 있음(Medium). 완료 세대 보존·독립 기한 검사와 입력 작업별 새 출력 확인으로 수정. `/private/tmp/deppy-fleet-20261005/followup-review-result.txt`.
- 최종 수정분 독립 리뷰 추가High: Codex 설정 단축키가 초안 입력으로 오인되어 다음 지시 차단. 원래 guarded Codex 자동 입력의 정확한 두 CSI 설정 키만 구분하고 실제 worker 회귀 테스트 RED→GREEN으로 수정. RED `/tmp/deppy-centered-codex-draft-red-20261005.log`, GREEN `/tmp/deppy-centered-codex-draft-green-20261005.log`. 일반 history 키와 실제 초안 보호는 유지. 리뷰: `/private/tmp/deppy-fleet-20261005/final-followup-review-result.txt`.

- Codex 수정의 독립 재리뷰: **No confirmed remaining findings**, `/private/tmp/deppy-fleet-20261005/codex-draft-final-review-result.txt`, 실제 exit0. 초기5 + 재리뷰2 + 최종High1, 총8건 모두 반영.

## 빌드

최종 코드 커밋 `462c4af5429056ffd8ce6bd9ab015cc4f60d9cfe`에서 재빌드·패키지·정적 버전 검증 완료. 제품 소스329개 해시가 빌드 전후 일치한다. 앞선 미배포 후보는 별도 디렉터리에 보존했다.

-0.6.1→0.7.0, inherited workspace/lock27개와 macOS plist 두 버전 필드 일치, 컴파일 메타데이터·내장 버전 확인. Deppy 바이너리 실행 없이 확인.
- `target/bundle-0.7.0/Deppy Sijo.app`, ZIP. 패키지 서명/내용 검사 exit0(명시적 로컬 개발 서명 정책). 공인 notarized 배포 아님.
- `/private/tmp/deppy-fleet-20261005/release-final-proof.json`에 해시·버전·소스 커밋 증거 기록. 최종 빌드 로그 `/tmp/deppy-centered-package-final-codex-0.7.0-20261005.log`, exit0.
- 프로그램 재실행·push 없음.

- 최종 바이너리 SHA256 `9238caf325e0040f19b9253aad4e9ead1714b84f560a02983ade7b446ddaf490`.
- 최종 ZIP SHA256 `59f63e82e02f23c6c2aaa31e194a1e404afe5ebe3390b0dfc3b276ef9c876a98`.
