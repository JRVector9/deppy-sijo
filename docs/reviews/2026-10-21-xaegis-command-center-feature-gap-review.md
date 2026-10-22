# xAegis와 Deppy 멀티에이전트 터미널 기능 비교

검토일: 2026-10-21. 요청: `ubuntu@gpu3090`에서 xAegis를 찾아 현재 개발 중인 멀티에이전트 터미널에 구현되지 않은 기능 확인.

## 비교 기준

- xAegis 배포 디렉터리: `/home/ubuntu/xAegis`. `build.json`은 **0.1.1 / build125 / commit42c2464 / dirtyfalse**를 보고한다. 비교는 이 디렉터리의 실제 소스를 기준으로 했다. 배포 메타데이터가 현재 실행 프로세스의 동일성을 증명하는 것은 아니다.
- 별도 개발 저장소: `/home/ubuntu/projects/xAegis`, clean, HEAD `b90e0601eb142b176fb3d522950b3b52a1c7d74b`, package0.1.0. 배포본과 다른 버전이므로 둘을 섞어 판정하지 않았다.
- Deppy: `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`, `feat/agent-command-center-v2-20261020`, HEAD `7584ed75`, version0.15.1. 현재 만드는 Command Center V2 및 기존 서비스 기능을 함께 확인했다.
- SSH로 소스·문서·빌드 메타데이터를 읽었다. 서비스 설정·토큰·사용자 DB·MEMORY.md 내용은 읽지 않았다. 원격 서비스 변경, 앱 재시작, 유료 에이전트 실행, 테스트 실행은 하지 않았다. 아래는 **정적 구현 비교**이며 실기기 동작 보증이나 보안 감사 결과가 아니다.
- 비교 소스의 SHA-256과 정확한 로컬 commit은 [근거 목록](2026-10-21-xaegis-source-manifest.json)에 기록했다. xAegis 구현 코드를 복제하지 않았다.

## 결론

Deppy에는 이미 프로젝트별 메인·작업자, 원격 작업자, 작업 의존성, 로컬 worktree, PWA, 알림과 클라우드 에이전트 관리가 있다. 가장 큰 차이는 **목표를 받은 메인이 작업 배정 → 결과 수집 → 실제 검증 → 수정 → 통합 → 다음 작업을 이어가는 운영 흐름**이다. 현재 Deppy는 계획 제안과 사용자 확인을 중심으로 구성되어 있고, 이 흐름 전체가 자동으로 연결되어 있지는 않다.

아래의 `high/medium/low`는 이번 제품 목표에 대한 도입 우선순위다. 현 기능의 장애 심각도나 xAegis의 무결성을 뜻하지 않는다. `미구현`은 확인한 Command Center 경로에 해당 관리 기능이 없다는 뜻이며, CLI나 수동 터미널 명령으로 같은 작업을 수행할 수 없다는 뜻은 아니다.

| 우선순위 | 기능 | xAegis 실제 구현 | Deppy 현재 상태 | 부족한 부분 |
|---|---|---|---|---|
| high | 메인의 작업 관리 도구 | 메인이 기기·프로젝트·작업 조회/생성/취소 도구 호출 | 부분 구현: 메인 입력과 계획 JSON, 외부 공유 세션 MCP | 메인이 Command Center 작업 API를 직접 호출하는 도구 연결 |
| high | 메인 대화·실행 과정 | 대화 메시지, 도구 활동, 진행 상태와 첨부 컨텍스트 보존 | 부분 구현: MAIN은 목표·집계·배정 행, 응답은 실제 세션으로 이동 | MAIN 안의 실제 대화·도구 진행·결과 흐름 |
| high | 지속적인 목표 수행 | 완료 이벤트로 지휘자 재평가, 빈 슬롯 보충, 시간·작업 수 제한, 예약 점검 | 미구현: 명시적 계획·배정·재확인, 완료 상태 집계 | 승인 범위 안에서 다음 작업을 이어가는 운영 루프 |
| high | 실제 검증·제한된 수정 | 검증 명령 실행, 종료 코드·출력·시간 기록, 실패 분류와 제한된 후속 작업 | 부분 구현: 사용자가 검증 근거와 통과 여부 입력 | 앱이 실행한 검증 증거와 결과별 후속 처리 |
| high | 작업 결과의 Git 통합 | 원격 결과 브랜치 전달/import/merge, 충돌·통합 상태 추적 | 부분 구현: 로컬 작업 소유 worktree, 일반 Git 화면 | task/attempt/commit에 연결된 결과 반입·검증·통합 흐름 |
| high | 쉬운 기기·프로젝트 등록 | 일회용 join code, 설치 명령, 원격 프로젝트 검색·폴더 매핑 | 부분 구현: endpoint·지문·토큰·폴더 수동 설정, 별도 터널 준비 | 설치부터 실행 허용까지 이어지는 간편 등록 |
| high | 공급자별 자동 결과 연결 | Claude/Codex 지휘자·작업자의 구조화된 실행 경로 | 부분 구현: Codex의 지원되는 실제 턴만 자동 계획/결과 연결 | Claude 등 PTY 공급자까지 동일한 계획·결과 계약 적용 |
| medium | 사용량·자원 기반 배정 | 계정 잔여량·작업 수·기기 상태를 비교하고 실제 배정 시 재검사 | 부분 구현: 사용량 표시, 기기별 profile·동시 실행 수 제한 | 사용량·계정 가용성·CPU/메모리를 배정 정책에 연결 |
| medium | 보고서·산출물 관리 | 작업 보고서, 링크·파일 업로드, HTML 등 산출물 전달·보기 | 부분 구현: 클라우드 run 결과 URI와 이력, CC 결과 요약 | 클라우드/로컬/원격 결과를 동일 작업에 연결한 산출물 보관·미리보기 |
| medium | 메인의 장기 기억·문맥 이어가기 | MEMORY/RULES 주입, 길어진 대화 압축, 실행기·계정 변경 시 요약 전달 | 부분 구현: 목표·설정·세션 저장, CLI 자체 문맥 | 앱이 관리하는 프로젝트 기억과 메인 교체 시 인계 |
| medium | 기기별 로컬 모델·GPU 관리 | 연결 기기 모델·엔진·포트, GPU/메모리 정보 수집, 실행 후보 제공 | 부분 구현: Ollama/OpenAI 호환 모델 검색·사용 | 원격 모델·자원 목록과 Command Center 배정의 통합 |
| low | 기기 GUI·브라우저 작업 | 장비 화면·브라우저 도구와 사용 소유권 관리 | 부분 구현: 터미널·MCP 확장 가능, CC 원격 화면/입력 | 기본 제공 장비 자동화 도구와 프로젝트별 실행 권한 |

## 근거와 구현 범위

### 1. 메인이 실제 관리 작업을 수행하는 연결

xAegis `coach.mjs:15`는 파일·셸·검색 도구를, `coach.mjs:192`는 `list_devices`, `list_projects`, `create_task`, `get_task`, `cancel_task`, `execution_options` 등의 허용 도구를 구성한다. 메인이 터미널 문장을 만들어 보내는 것 외에 구조화된 관리 동작을 호출할 수 있다.

Deppy의 [MCP 도구](../../crates/agent-mcp/src/lib.rs) `tools():64`에는 세션 조회·출력 읽기·입력, run 등록/갱신·결과/산출물 URI, 이력·제어권 관리가 이미 있다. 다만 이 카탈로그는 Command Center의 프로젝트 계획 설치·작업 배정 API를 노출하지 않는다. [호스트](../../crates/app/src/command_center_host.rs)의 `InstallPlanJson`, `DispatchTask`는 UI intent 경로다. 기존 MCP를 없애는 것이 아니라 메인에게 허용된 프로젝트 관리 도구로 연결할 필요가 있다.

### 2. MAIN에서 실제 응답과 진행 확인

xAegis `coach.mjs:143–193`에는 메시지·실행 상태·도구 활동과 첨부 컨텍스트가 있다. Deppy [MAIN UI](../../crates/app/src/ui/command_center/main.rs):240과 [호스트](../../crates/app/src/command_center_host.rs):1036은 `main_summary` 및 작업 행을 렌더링한다. 해당 요약도 “메인 응답은 실제 세션에서 확인합니다”라고 안내한다. 메인 세션 자체는 존재하지만 중앙 MAIN 영역이 실제 대화 스트림을 제공하는 단계까지는 연결되지 않았다.

### 3. 목표 수행·복구·예약 점검

xAegis `hub.mjs:418–443`의 완료 처리와 `autonomy.mjs:450–475`의 gate는 완료 작업·빈 슬롯·사용량·시간/작업 예산에 따라 지휘자를 깨운다. `hub.mjs:468–469`는 예약 점검을, `hub.mjs:1299–1307`은 주기적인 감시·배정 루프를 등록한다. 승인 거절/확인 대기는 자동 재시도로 우회하지 않으며, 실패 복구도 제한된다.

Deppy [orchestrator](../../crates/app/src/work_management/orchestrator.rs)는 계획 설치·확인과 상태 집계이고, [queue](../../crates/app/src/work_management/queue.rs):6은 확인된 작업의 의존성·동시 실행 수·충돌을 검사한다. [완료 관측](../../crates/app/src/command_center_observe.rs):705는 결과를 저장하지만 다음 목표 판단·재계획을 메인에 자동 요청하는 루프는 없다. 자동화를 추가하더라도 현재의 정확한 실행 식별, 사용자 허용 범위, 관측 불명 입력의 재전송 금지는 유지해야 한다.

### 4. 실제 검증과 수동 확인은 다르다

xAegis `agent.mjs:774–788`의 `verifyWithCommand`는 실제 명령을 실행하고 제한 시간·종료 코드·출력·검증 시각을 기록한다. 명령 오류/도구 미설치를 작업 결과 실패와 구분한다. `hub.mjs:593–600`은 목표 완료 보고 뒤 별도 check 작업을 만든다. 검증 명령이 없으면 `verified:false`이며, xAegis도 모든 완료를 검증 완료로 취급하지 않는다.

Deppy [VerifyResult](../../crates/app/src/command_center_host.rs):1867은 사용자가 입력한 `passed`와 `evidence`를 저장하며 “사용자 직접 검증”이라고 기록한다. 실제 검증 명령 runner는 이 경로에 없다. 사용자 검증 기능 자체는 정상적인 별도 기능이며, 앱 실행 검증 증거가 추가로 필요하다는 뜻이다.

### 5. worktree 이후 결과 통합

xAegis `hub.mjs:397–413`은 결과 브랜치 import/merge를 기기에 요청하고 통합 상태·충돌을 저장한다. `agent.mjs:1032–1033`에는 실제 `mergeBranch`/`importBranch` 실행 진입점이 있다.

Deppy [task_worktree](../../crates/app/src/work_management/task_worktree.rs)는 작업 소유권을 확인한 로컬 worktree 생성·준비를 이미 제공한다. [호스트](../../crates/app/src/command_center_host.rs):924는 서버 소유 증명이 없는 원격 worktree를 차단한다. 따라서 “worktree 없음”이 아니라 **원격 소유권 증명 및 결과 반입부터 통합까지의 관리 기능 부족**이다. 기존 Git 화면/CLI의 수동 병합과는 구분한다.

### 6. 쉬운 등록과 기기 배정

xAegis `join.mjs:10–15,97–115`에는 만료되는 join code와 TLS 고정 설치 경로가 있다. `hub.mjs:1178–1179`, `agent.mjs:1015,1028`은 원격 폴더·프로젝트 탐색을 제공한다. Deppy [기기 설정](../../crates/app/src/ui/command_center/devices.rs):47–64,110–123은 IP:포트·인증서 지문·프로젝트 폴더·토큰을 직접 입력받는다. [지원 문서](../command-center-v2.md)의 다른 컴퓨터 연결 항목에는 별도 보안 터널 준비가 명시되어 있다. 현재 권한 검사를 유지하면서 입력 단계를 줄이는 등록 절차가 필요하다.

xAegis `allocation.mjs:3–31`은 모델별 계정 잔여량·현재 작업 수를 비교하고, `autonomy.mjs:364–390`은 메모리/CPU/디스크 상태와 기기 capacity를 다룬다. Deppy에는 공급자 사용량 모듈이 이미 있으나 [queue](../../crates/app/src/work_management/queue.rs) 및 [배정](../../crates/app/src/command_center_assignment.rs)은 profile·정책·동시 실행 수 중심이며 사용량 지표를 소비하지 않는다. xAegis 역시 모든 공급자의 사용량을 아는 것은 아니고, 계정 자동 선택과 메인의 모델 선택을 구분한다.

### 7. 공급자·기억·산출물·로컬 모델

- 공급자: xAegis coach는 Claude/Codex 실행기를 선택하고 대화 요약을 넘긴다(`coach.mjs:78,84,156–193`). Deppy [provider_adapter](../../crates/app/src/work_management/provider_adapter.rs):16–48과 [observe](../../crates/app/src/command_center_observe.rs):239,705는 실제 지원되는 Codex 턴을 연결하고, 다른 CLI는 수동 JSON/결과 경로를 유지한다. Claude/Kimi/Grok/Cursor 자체가 미지원이라는 뜻이 아니다. xAegis가 이들 모두에 동등한 구조화 지원을 제공한다고도 판정하지 않는다.
- 기억: xAegis `coach.mjs:19,22,71–84,172–173`은 별도 규칙·기억 주입과 길어진 대화 압축/인계를 한다. Deppy에는 목표·설정·세션의 영속화와 CLI 고유 문맥이 있다. 추가할 부분은 Command Center가 소유하고 사용자에게 보이는 프로젝트 기억과 메인 교체 인계다.
- 산출물: xAegis `reports.mjs:12–15,115`, `report-artifacts.mjs:12–14`, `docs/REPORTS.md`에는 파일을 포함한 보고·전달·보기 경로가 있다. Deppy 클라우드 `update_run`은 이미 artifact URI를 받는다. 그러나 [CC TaskResult](../../crates/core/src/work_management.rs):175는 요약과 source/artifact revision 문자열 중심이고, [결과 관측](../../crates/app/src/command_center_observe.rs):721은 `artifact_revision:None`이다. CC 작업과 파일 저장·미리보기·확인 기록을 통합하는 부분이 남아 있다.
- 로컬 모델: xAegis `local-llm.mjs:15,87`과 `agent.mjs:538–595`는 연결 기기의 모델 런타임과 하드웨어 정보를 수집한다. Deppy [local_llm](../../crates/app/src/local_llm.rs):28–61은 Ollama/OpenAI 호환 모델 검색이 이미 있다. 부족한 것은 등록된 원격 기기의 GPU/모델 목록을 배정 정책과 연결하는 기능이다.
- 기기 자동화: xAegis `agent.mjs:999–1001` 및 `automation/`에는 GUI·브라우저 명령 경로가 있다. Deppy의 터미널·MCP 확장 능력과는 별개로, 장비 화면 제어를 기본 제공하는 통합은 확인되지 않았다. 터미널 제품의 우선 목표에서는 후순위로 두는 것이 적절하다.

## 이미 있는 기능과 선택적인 차이

다음은 새로 만들어야 할 기능으로 중복 분류하지 않는다.

- 프로젝트별 메인/작업자 연결, 정확한 task/attempt/runtime 식별과 작업 의존성.
- 로컬 작업 소유 worktree 및 범위 충돌·동시 실행 제한.
- TLS 지문·프로젝트 token·허용 profile을 확인한 원격 작업자 실행과 native 후속 입력/종료.
- PWA/Relay 화면과 기존 모바일 입력, 웹 푸시·데스크톱 알림, 클라우드 run 이력·결과 URI.
- 공급자 사용량 표시, 로컬 모델 검색, 일반 터미널·Git·문서·이력 기능.

다만 현재 **PWA의 다른 기기 원격 워커는 읽기 전용이며 cross-TLS cloud 입력도 비활성화**되어 있다([지원 범위](../command-center-v2.md):36). “모든 기기에서 확인하고 제어”라는 목표에는 이 제한을 풀기 위한 별도 권한·실행 증명 설계가 필요하다. xAegis의 웹 기반 관리 도구와 Deppy PTY 모바일 입력을 동일 기능으로 간주하거나, xAegis의 모든 화면이 완전한 원격 터미널이라고 가정하지 않았다.

xAegis의 native iOS/iPad 앱도 확인했지만, Deppy의 현재 PWA 목표에서 필수 누락으로 판정하지 않았다. xAegis의 계정 자격 증명 배포 방식·자동 병합·GUI 제어는 그대로 채택하기 전에 우리 권한 모델과 맞춰야 한다.

## 권장 진행 순서

1. **PR1 — 메인 관리 도구와 실제 대화 연결.** 프로젝트 범위의 조회/계획 제안/허용된 배정 도구를 제공하고 MAIN에 실제 메시지·도구 진행을 연결한다. 기존 외부 공유 MCP의 권한과 새 메인의 권한을 명확히 연결한다.
2. **PR2 — 검증 실행과 결과 계약.** 앱 실행 검증/사용자 직접 검증을 구분하고 command·cwd·timeout·exit·증거·result revision을 보존한다. 완료/검증/사용자 확인을 계속 분리한다.
3. **PR3 — 목표 운영 루프.** PR2 결과에 따라 메인이 검토·다음 작업 제안/배정을 이어간다. 사전 허용 범위, 작업·시간·비용 한도, 중단/재개, 제한된 수정 회수를 둔다. Unknown 재실행과 거절 우회는 금지한다.
4. **PR4 — 간편 기기 등록과 공급자 어댑터.** 일회용 등록, 실제 agent/profile 탐색, 프로젝트 폴더 매핑을 연결한다. Claude 등은 공급자별 실행/완료 증명을 먼저 정의하고 지원 범위가 확인된 것부터 확장한다.
5. **PR5 — 결과 반입·통합·보고서.** 정확한 커밋·원격 worktree 소유권·산출물 전달을 연결하고 승인된 통합을 지원한다. 충돌은 완료로 처리하지 않는다.
6. **PR6 — 자원 배정·모바일 제어·장기 운영.** 계정 잔여량/기기 자원/로컬 모델 정보를 배정에 연결하고, 별도 검증을 거쳐 모바일 원격 입력을 확장한다. 장기 기억·예약 점검도 명시적인 운영 범위 안에서 적용한다.

이 순서는 제안이며 이번 요청에서 구현을 시작하지 않았다. 제품 소스·실행 앱·원격 서비스는 변경하지 않았다. 보고서와 handoff만 추가·갱신했다.
