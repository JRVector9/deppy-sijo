# 팝업 동작 점검 — 2026-09-30

대상: HTML 사례01–42 및 전용 AI 런처. 사용자 요청에 따라 열기·닫기·입력·확인·실제 작업 전달을 검토했다. 성능 작업트리의 기존 변경을 보존했다. Deppy 실행·재실행, 실제 사용자 파일 삭제·프로세스 종료·외부 인증은 수행하지 않았다.

## 확인된 오류와 수정

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | shortcuts.rs / ui/workspace.rs | 확인창·양식 뒤로 전역 단축키 전달 | 확인 중 세션 닫기·화면 전환 가능 | 공용 입력 차단 검사 적용, RED→GREEN |
| medium | app.rs / 사례01 | 이미 본 폴더 재열기는 런처 생략 | 세션 시작 선택창이 나오지 않음 | 명시적 열기 시 런처 표시, RED→GREEN |
| medium | ui/file_tree.rs / 사례02·03 | 생성 실패 문구가 모달 뒤에 표시 | 입력을 고칠 근거가 보이지 않음 | 폼 안 Error notice, RED→GREEN |
| medium | ui/fleet.rs / prompt_palette.rs | 앞 모달의 Esc가 뒤 초안도 닫음 | 예약·프롬프트 초안 유실 | 맨 앞 Window에서만 Esc 소비, RED→GREEN |
| medium | connector-ui/lib.rs / 사례06·16·22–25 | 서비스 승인·인증과 로컬 양식 동시 조작 | 요청·초안의 작업 대상 혼동 | 승인 중 로컬 양식 보류·초안 보존, RED→GREEN |

- 입력 차단: 터미널과 전역 단축키가 동일한 레이어 판정을 사용한다. egui의 `top_modal_layer()`는 이전 프레임 값이므로 현재 Foreground 영역도 확인한다. 조회용 Agents·diff Window 예외는 유지하고 회귀 검사했다.
- Esc: Fleet 예약·브로드캐스트·일괄 시작 및 프롬프트 팔레트가 공용 `popup::take_window_escape`를 사용한다. 모달·선택 메뉴를 우선하고, 키 한 번으로 앞 Window만 닫는다.
- 폴더 열기: 등록 후 대상이 실제 active인 경우에만 터미널을 드러내고 런처를 연다. 새 폴더·다른 기존 폴더·현재 활성 폴더 재선택에 적용한다. 기본 셸 자동 생성은 제거했다. 설정의 프로젝트 등록·Git 복제 저장 위치 선택은 해당 설정 흐름을 유지한다. 진행 중인 실행 요청은 교체하지 않는다.
- 생성 실패: 잘못된 이름과 큐 접수·실행 오류에 대해 기존 이름·대상 위치를 유지하고 폼 안에 오류를 표시한다. 실제 삭제·생성 대신 잘못된 이름의 검증 실패를 UI 하네스에서 실행했다.
- MCP: 승인·원격 신뢰·OAuth 상태가 있으면 서버 양식·도구 인자·삭제 대상을 보류한다. 초안은 유지해 요청 해소 후 다시 표시한다. OAuth fixture로 보류와 복귀를 확인했다.

## 42개 사례 점검표

`코드/기존 테스트`는 호출·의도·완료 처리의 소스 검토와 기존 자동화 실행 범위다. 모든 케이스를 네이티브 앱에서 클릭했다는 뜻은 아니다. 이번에 추가한 실패 재현은 별도로 표시한다.

| 번호 | 사례 | 점검한 흐름 | 결과 / 검증 범위 |
| --- | --- | --- | --- |
| 01 | 워크스페이스 추가 | 선택 목적·폴더 결과·복제 취소·등록·전환·런처 | 재열기 런처 수정; 실제 런처 선택 UI 및 등록 워커 테스트 |
| 02 | 새 폴더 | Enter/확인·취소·이름 검증·접수 거절·재시도 | 오류 표시 수정; 실제 생성 폼 회귀 |
| 03 | 새 파일 | 확인·취소·이름 검증·기존 파일 보호·재시도 | 오류 표시 수정; 실제 생성 폼 회귀 |
| 04 | 워크스페이스 이름 | 인라인 포커스·Enter·Esc·밖 클릭·대상 소멸 | 코드/기존 테스트, 렌더 하네스 |
| 05 | API 키 수정 | 저장 자격·pending 닫기 차단·실패 재입력·비밀 해제 | 코드/기존 Credentials 테스트 |
| 06 | MCP 서버 추가·수정 | HTTP/stdio·유효성·취소·저장 의도·인증 경합 | 양식 보류 수정; Connector UI 회귀 |
| 07 | 세션 닫기 | 대상 pane 보존·소멸·취소·확인 의도 | 코드/기존 UI·모달 입력 테스트, 렌더 |
| 08 | 워크스페이스 전체 종료 | 설정 자격·captured workspace·취소·종료 의도 | 코드/기존 App 테스트, 동일 사양 렌더 |
| 09 | 환경 프로젝트 목록 닫기 | 확인/취소·설정 목록 숨김·세션 유지 | 코드/기존 App 설정 테스트 |
| 10 | 리소스 세션 종료 | 원래 session/runtime 대상·팝오버 닫힘·취소 | 코드/기존 Resource UI 테스트, 렌더 |
| 11 | 미연결 프로세스 종료 | workspace/runtime instance·확인·취소 | 코드/기존 Resource UI 테스트, 렌더 |
| 12 | 포트 프로세스 종료 | 원래 소켓 식별·확인·취소·완료 처리 | 코드/기존 Ports 테스트, 렌더 |
| 13 | 영구 삭제 | 휴지통 실패 후 승격·경로 보존·접수 거절 재시도 | 코드/기존 FileTree 테스트, 렌더 |
| 14 | API 키 삭제 | 원래 credential ID·pending·확인/취소 | 코드/기존 Credentials 테스트 |
| 15 | 환경변수 삭제 | profile 변경·소스 선택·삭제/취소 의도 | 코드/기존 EnvProfiles 테스트 |
| 16 | MCP 서버 삭제 | 원래 server ID·취소·삭제 의도·승인 경합 | 로컬 양식 보류 적용, 코드/Connector 테스트 |
| 17 | 저장 프롬프트 삭제 | 상세 대상 ID·2단계 확인·취소 | 코드/기존 라이브러리 테스트 |
| 18 | 다음 단계 예약 | 원래 세션·예약/해제·Esc·초안 | Esc 수정; 실제 Window 회귀 |
| 19 | 브로드캐스트 | 프롬프트 파라미터·대상 자격·3개 이상 재확인 | Esc 수정; 앞 Window만 닫는 회귀 및 기존 Fleet 테스트 |
| 20 | 에이전트 일괄 시작 | 등록 agent·개수 제한·선택 프롬프트·시작 의도 | 공용 Esc 적용, 코드/기존 Fleet 테스트 |
| 21 | production 실행 | 원래 agent/profile·취소·확인 Run 의도 | 코드/기존 Agents 테스트 |
| 22 | MCP 도구 직접 실행 | server/tool ID·인자 제한·취소·Invoke 의도 | 양식 보류 적용, 코드/Connector 테스트 |
| 23 | MCP 도구 승인 | operation ID·Allow/Deny Once/Always | 서비스 우선 표시, 코드/Connector 테스트 |
| 24 | 원격 주소 신뢰 | operation/revision/fingerprint·허용/거부 | 서비스 우선 표시, 코드/Connector 테스트 |
| 25 | MCP OAuth | 발견·동의·클라이언트·브라우저·콜백·오류·취소 | 실제 UI 보류/복귀 회귀 및 기존 Connector 테스트 |
| 26 | 미저장 문서 닫기 | 큐 순서·저장 자격·저장/버리기/취소 | 코드/기존 App 문서 테스트 |
| 27 | 외부 수정 문서 | 원래 document ID·충돌 큐·Reload/Cancel | 코드/기존 App 문서 테스트 |
| 28 | 문서 탭 한도 | clean 탭 자격·안내·닫기 | 코드/기존 App 문서 테스트 |
| 29 | 프로젝트 폴더 이동 | 원래 경로/anchor·CAS·무시·stale 처리 | 코드/기존 App·Storage 테스트 |
| 30 | 프롬프트 라이브러리 | 검색·삽입·저장·편집·Esc·초안 | 앞 모달 보호 및 정상 Esc 복귀/닫기 회귀 |
| 31 | Git diff | 대상·새로고침·비동기 결과·닫기 정리 | 코드/기존 Diff 테스트, 비모달 단축키 회귀 |
| 32 | 에이전트 세션 관리 | 창 닫기·취소·대상 이동·비동기 결과 | 코드/기존 AgentSessions 테스트, 비모달 예외 |
| 33 | 런타임 적체 안내 | 적체 상태·안내 닫기 | 코드/기존 App runtime 테스트 |
| 34 | 워크스페이스 한도 | 전환 거절·이전 active 보존·안내 닫기 | 코드/기존 App 전환 테스트 |
| 35 | 다른 워크스페이스 셀 실패 | placeholder·실패 대상·안내 닫기 | 코드/기존 App cross-workspace 테스트 |
| 36 | 알림·승인 인박스 | 전역 대상·승인 의도·읽음·설정 이동 | 코드/기존 Inbox/Notifications 테스트 |
| 37 | 터미널 상태 팝오버 | 리소스/포트 확인 생존·앞 모달 우선 | 코드/기존 StatusBar·Resource·Ports 테스트 |
| 38 | 입력창 모델·MCP 선택 | snapshot 자격·원래 server/model·바깥 닫기 | 코드/기존 Composer 테스트 |
| 39 | 파일 트리 메뉴 | 선택 대상·생성 위치·복사/이동/삭제·Finder 의도 | 코드/기존 FileTree 메뉴/키보드 테스트 |
| 40 | 셀·세션 메뉴 | 원래 pane/tab·분할/닫기·예약 의도 | 코드/기존 Workspace/Fleet 테스트 |
| 41 | 설정 OS 창 | viewport close·포커스 해제·민감 초안 정리 | 코드/기존 설정 테스트; 네이티브 창 미실행 |
| 42 | 기본 파일·폴더 선택 | purpose·취소 completion·큐 재시도·등록 worker | 코드/기존 host/admission 테스트; 네이티브 선택창 미실행 |

## 실행 결과

- 수정 전: 전체 App **2465 passed / 0 failed / 27 ignored**; Connector UI **16 passed**. 기존 테스트만으로 위 오류를 잡지 못했다.
- 추가 회귀: App `popup_audit` **10 passed**; Connector `popup_audit` **1 passed**. 오류 재현의 RED와 수정의 GREEN을 실행했다. 파일/폴더 오류 검사는 두 생성 종류를 순회한다.
- 수정 후 전체 App **2475 passed / 0 failed / 27 ignored** (45.43s), Connector UI **17 passed**, i18n **8 passed**. 이후 lint 정리만 적용하고 `popup_audit` 10개를 다시 실행해 통과했다.
- ignored `popup_parity_render` **7 passed** (2.58s). PNG01 워크스페이스 추가·02 폴더 생성을 새로 시각 확인했다. 나머지 렌더 케이스는 출력 생성까지 확인했다.
- 엄격한 App/Connector Clippy·fmt·diff 검사, 추출 HTML JavaScript의 Node 문법 검사가 통과했다. 첫 Clippy의 미사용 production helper와 테스트 initializer 지적은 정리 후 재검사했다.
- 0.4.8→**0.4.9** 릴리스 빌드가25.12s에 완료됐다. 모든27 workspace metadata/lock 버전, 두 bundle plist 버전, 컴파일된 reported-version marker를 확인했다. 별도 Developer ID 서명 앱·ZIP의 로컬 개발 검증과 압축 해제 검증이 통과했다. 앱 실행·재실행 없음. 이 검증은 notarization 주장이 아니다.
- 소스: base `166f8daeb1054cf09a07194fa83bf0a4a19d93ce` + 미커밋 product diff/new confirmation SHA256 `21403f7a001064c16f6bc7422c1f5ca9488f5d97c237796252660b5b1aef98d0`. 산출물: `target/bundle-0.4.9/Deppy Sijo.app`, ZIP. 검토 범위에서 남은 확인된 코드 오류는 없다.

## 한계

실행 중인0.4.5 앱은 변경하지 않았다. 네이티브 Finder 선택, 별도 설정 창, 실제 삭제·프로세스 종료, 외부 OAuth 브라우저와 네트워크 연결의 실사용 검증은 포함하지 않는다. 하네스는 실제 위젯과 반환 의도·임시 데이터에서의 작업 흐름을 검사한다. 사례08 시각 하네스는 App과 같은 확인 사양이며 전체 App 종료를 실제 수행하지 않는다.
