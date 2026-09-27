# 후속 연결 속도·처리·메모리 효율 조사 및 PR 개발

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | 자동 Quick Tunnel DNS | 최종 3회 중 연결 검증64.683초1회 | 최악 연결 대기 남음 | 고정 주소·운영 게이트웨이 경로 검토 |
| medium | GitHub Actions / PR199–201 | 결제·사용한도 때문에 hosted job 미시작 | 서버 CI 결과 미검증 | 계정 Billing 해결 후 CI 재실행 |

## 완료 및 PR 범위

웹 자료와 현재 생산 코드를 비교하고 확인된3개 비효율을 수정·실측·source CLI 리뷰하여 PR로 올렸다. 모든 PR은 ready for review, push 완료, merge하지 않았다.

| PR | base → head | 제품 commit | 수정 |
| --- | --- | --- | --- |
| [199](https://github.com/JRVector9/deppy-sijo/pull/199) | automatic-mcp-connect → mcp-tunnel-event-waits |76b326df| 유계 신호 대기 및 첫 probe 등록 확인 |
| [200](https://github.com/JRVector9/deppy-sijo/pull/200) | mcp-tunnel-event-waits → mcp-screen-buffer-reuse |f67e1290| 행 문자열 버퍼 재사용 |
| [201](https://github.com/JRVector9/deppy-sijo/pull/201) | mcp-screen-buffer-reuse → mcp-history-answer-index |6cabc21b| 보존 답변 partial index |

각 head의 실제 이름에는 `perf/` 접두사가 있다. 기반 `feat/automatic-mcp-connect`는 앞선 자동 MCP/7개 개선/세션 표시 작업을 포함하는 기존 검증 소스다. 이번 PR들 diff에 그 변경 전체를 중복시키지 않았다. 병합 순서는199→200→201이다. 중간 PR별 앱을 배포하지 않았으며 합본 local build만 **0.2.1→0.2.2**로 올렸다. 서브에이전트를 새로 띄우지 않고 순차 구현했다.

앞선 세션 표시는 이번 빌드에도 포함된다: `simpleHWP · 에이전트 1 / Codex · gpt-6-astra · xhigh`. 기존 제목과 AI/모델/추론 formatter를 재사용한다.

## 1. 주소·연결 속도 조사 결과

‘주소가 생성되는 시간’과 ‘공개 HTTPS MCP가 실제로 준비된 시간’을 따로 측정했다. 원래 주소 생성은 약3.37초였다. 오래 걸린 부분은 DNS 및 공개 resource 검증이었다.

| 측정 소스 | 실제 Ready 시간 |
| --- | --- |
| 최초 변경 전 |80.267 /79.655초|
| 직전0.2.1 완성 |15.467 /17.439 /11.241초|
| 이번 최종 acknowledgement 포함 |**13.660 /21.232 /64.683초**|

마지막3회의 주소 생성 시간은 3.047 /3.223 /2.957초다. 워커의 대기 개선만으로 총 네트워크 연결 시간이 더 빨라졌다고 주장하지 않는다. 최종 중앙값21.232초는 직전 표본15.467초보다 크다. 무작위 새 주소/외부 서비스 변동이 있는 작은 표본이다. 한 번의 긴 지연을 제외하여 성공률을 부풀리지 않았다. 최초80초 대비 빠른 사례가 늘었지만 최악 지연은 해결되지 않았다.

[Cloudflare 공식 Quick Tunnel 문서](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/trycloudflare/)는 임시 무작위 주소 방식이며 가용성 보장을 제공하지 않는다. 새 주소의 DNS 게시와 음성 캐시 대기는 앱 코드만으로 보장할 수 없는 영역이다. Quick Tunnel은 SSE도 지원하지 않으므로 현재 요청/응답 MCP 경로의 성공을 무한 SSE 연결의 검증으로 확대하지 않는다.

### 방법별 판단

| 방안 | 현재 코드 비교 | 처리 |
| --- | --- | --- |
| 시스템 음성 DNS 복구 | 직전0.2.1에 bounded HTTPS DNS fallback/독립 resolver 반영됨 | 기존 검증 유지; 무분별한 DNS flush·TTL 변경 없음 |
| 첫 연결 등록 이벤트 즉시 반영 | 기존250ms/50ms sleep 및1초 probe 주기 지연 | PR199: 용량1 채널, 등록+서버 Host/OAuth publication 완료 확인 뒤 probe |
| HTTP/TLS 연결 재사용 | 생산 probe는 이미 같은 ureq Agent와 lookup Agent 재사용 | 신규 pool 추가 없음; [고정3.4.0 source](https://raw.githubusercontent.com/algesten/ureq/3.4.0/src/agent.rs)와 대조 |
| 고정 named tunnel | 기존 수동 고정 HTTPS 모드 있음 | 기존 사용 가능; 새 계정 없는 사용자 기본 자동 방식의 대체로 강제하지 않음 |
| Deppy가 운영하는 고정 gateway | 현재 운영 주소/HTTP MCP gateway 없음 | 가장 큰 구조 개선 후보. 도메인·사전 DNS·TLS·운영 relay가 필요하며 이번 PR에서 서버를 배포하지 않음 |

고정 gateway가 사전 게시된 DNS와 설치별 고정 경로를 쓰면 매번 새 DNS 이름을 만드는 단계를 제거할 수 있다는 설계상 추론이다. 실제 운영 서비스의 속도 수치나 즉시 연결을 보장하는 주장은 아니다. 사용자에게 설치/계정을 요구하지 않으려면 운영자가 gateway를 준비해야 한다. 앱의 임시 포트/세션별 동의/OAuth/입력 회수는 유지하고, 기기가 꺼지면 실패하며 오프라인 명령을 재생하지 않는 구조가 맞다. 운영 domain/endpoint와 인증·비용 정책이 정해지지 않은 서버를 임의로 배포하지 않았다.

## 2. 저장소 전체 효율 점검 범위

26개 Rust crate,255개 Rust source,345,231줄(테스트 포함)의 파일/경계 inventory와 할당·clone·queue·cache·repaint·I/O 정적 검색을 수행했다. xtask 포함 workspace package는27개다. 모든 줄을 읽거나 모든 기능을 profiler로 실행한 전수 검증은 아니다. 의심된 경로를 직접 읽고 확인된3개를 아래 실제 API/fixture로 측정했다.

| 경로 | 현재 코드에서 확인한 사항 | 판단 |
| --- | --- | --- |
| terminal snapshot/render | 앞선12B셀/변경 행 공유/갤리 캐시/CJK run 적용 | 이번에 재구현하지 않음; [앞선 실측](2026-09-27-seven-improvements-final.md) |
| workspace/egui | project-name revision/Arc, cloud answers Arc 공유 | clone 문자열만 보고 deep copy로 판단하지 않음; [egui Memory 가이드](https://docs.rs/egui/0.36.0/egui/struct.Memory.html)와 비교 |
| file tree/search/watch | show_rows 가시행 렌더, listing/retained16MiB/watch 경계 | 이미 [가시행 표시 API](https://docs.rs/egui/0.36.0/egui/containers/scroll_area/struct.ScrollArea.html) 사용; 전체 행 그리기로 오판하지 않음 |
| markdown image/document |8MiB encoded/6000px/16M pixel 제한 및 generation cache 정리 | 측정 없는 압축/해상도 변경을 넣지 않음 |
| runtime/session/remote | cache budget/hidden 반납, warm 상한, bounded frame16MiB·queue와 backpressure | 한도는 무한 성장 방지 근거이며 앱 전체 RSS 절감의 증명이 아님 |
| web/relay/auth/secret/storage | connection/body/queue/redaction corpus 한도 및 streaming 보존 | 세션 동의·토큰·redaction·운송 계약 변경 없이 측정된 이력 SQL만 수정 |
| 자동 MCP 워커 | healthy loop4회/초, verifying20회/초 | PR199에서 유계 signal 대기로 줄임 |
| MCP 화면 변환 |행마다String 신규할당|PR200에서 호출 내 scratch 재사용|
| MCP 감사 DB |finish마다 최대100k operation 전체 scan|PR201 partialindex로 retained answer만 탐색|

전체 실행 앱의 RSS/CPU/GPU 개선율이나 메모리 누수 부재를 증명하지 않았다. 읽기 전용으로 본 실행 중 앱은 이전0.2.0이므로 이후 RSS와 비교하지 않는다. 기존 사용자 세션을 벤치마크로 조작하거나 새 AI를 백그라운드에 실행하지 않았다.

## 3. 각 PR의 실제 전후

### PR199: 워커

실제 소유 helper fixture와 생산 워커,2초 정상 대기:

- 루프8→2회(75% 감소). 중단 대기175.424→1.093ms(한 쌍 측정, SLA 아님).
- 주소/첫 등록/EOF/중단에만 signal. 용량1로 coalesce하며 로그 폭주를 새 큐에 쌓지 않는다.
- 최장1초 child 상태 확인, HTTPS health10초/검증1초, 초기90초 deadline 및 정확한 소유 child 회수 유지.
- 리뷰 Medium: 첫 probe와 local server.set_public_host 경합을 실제 RED(등록 전 probe1회)로 재현. exact Address acknowledgement 후 probe하도록 수정; 잘못된 host acknowledgement 거부 및 실제 positive 검증.

### PR200: 임시 할당

실제 Alacritty backend300×80, 생산 screen_text 함수를 그대로 추출한 rustc-O 프로브,3회. 카운터는 단일 호출만 켜고 timing loop에서는 껐다. 모든 fixture는 원래 함수와 출력 바이트 전체를 비교한다. [Rust buffer 재사용 가이드](https://nnethercote.github.io/perf-book/heap-allocations.html)와 일치하는 좁은 변경이며 raw 화면 장수 캐시를 추가하지 않는다.

| 화면 | alloc 전→후 | 누적 요청 B 전→후 | 시간 중앙값 µs 전→후 |
| --- | ---: | ---: | ---: |
| 빈 화면 |565→6|81,528→548|103.902→83.721|
| ASCII |568→9|157,780→76,800|86.015→69.368|
| 한글 |568→10|196,030→115,650|80.603→59.972|
| 희소 결합 문자·크기 경계 |402→11|368,324→195,000|214.707→179.982|

카운터 wrapper가 있는 고립 함수 벤치다. 앱 FPS/전체 RAM이 이 비율로 좋아진다고 확대하지 않는다. 요청량은 생애 동안 요청한 바이트의 합으로 peak/live heap과 다르다.

### PR201: SQL 규모

실제 Rust History + bundled SQLite3.53.2(memory DB), 입력 tombstone99,000/보존 답변100:

- pruning VMsteps **298,918→1,620**. 수정 후1k/10k/99k 입력 이력에서 모두1,620.
- 실제 finish 평균 **4,956.740→59.582µs**. debug API fixture의 한 쌍 결과이며 native GUI 응답시간 SLA가 아니다.
- memory DB의 page footprint5,099,520→5,103,616B. **4KiB index 비용**을 지불한다. 앱 RAM 전체 절약이라고 주장하지 않는다.
- recent 읽기는 약0.8ms 수준으로 개선을 주장하지 않는다.
- [SQLite partial index](https://www.sqlite.org/partialindex.html)는 조건에 맞는 행만 index에 넣는다. query predicate/원자성/최대100개 답변 완료 순서/100k tombstone 보존을 유지한다.

## 4. 검증과 리뷰

- 최종 cloud **32 pass/3 ignored**.
- agent-mcp 전체 **27 pass/1 ignored**. ignored실측 별도 실행 성공.
- 실제 public HTTPS MCP → 별도 실제 PTY → 출력 읽기/클라우드 자신의 답변 수신 **1 pass,16.22초**. OAuth승인fixture는로컬이다. 실제Grokbot계정통합검증이라고 주장하지 않는다.
- live startup3실행모두 통과;한 번64.683초 지연 그대로 기록.
- fmt/boundary/whitespace, release app/proxy0.2.2 **31.50초 성공**.
- sourceCLI 리뷰: 워커1 Medium, benchmark2 Medium 수용·수정. 각 최종 재리뷰 및 History 리뷰 추가 지적 없음. 문서가 아닌 실제 code/diff만 리뷰했다.
- 이전 workspace status `Idle` vs `Awaiting instruction` 기대 실패는 [직전 보고서](2026-09-28-mcp-startup-and-session-labels.md)에 실제 재현 기록이 있다. 이번 검증은 변경 관련 scoped suite이며 workspace 전체 통과를 주장하지 않는다.

GitHub PR199–201 CI는 failure로 보이지만 각 macOS job의 annotations에 결제/사용 한도로 **job was not started**라고 명시된다. steps=[]를 확인했다. 로컬 테스트 결과와 구분한다. GitGuardian scan은 성공했다. [실제 annotations](measurements/2026-09-28-followup-efficiency-audit/deppy-followup-ci-diagnostics.json). CI·계정 결제·한도·workflow를 임의로 우회하거나 변경하지 않았다.

## 5. 실패한 접근과 기록

- URL 게시 전 질의로 생기는 긴 DNS tail은 직전fallback 및 이번 event wait 후에도 남음. final3중 긴64.683초를 제외하지 않았다.
- benchmark 초안은 outputhash만 기록하고 timing에도 alloccounter를 켰다. 리뷰 후 exactbaseline byteoracle 추가 및 timingcounter비활성화, 전후 프로브를 다시 실행했다.
- History testfixture의 usize Sql 타입이 컴파일되지 않았다. i64로 보정 후 실제 VMstep RED를 얻었다(4918→298918), index 추가 후 GREEN.
- Python SQLite3.53.4 prescreen과 실제 bundled RustSQLite3.53.2 결과를 구분했다. 결론 수치는 실제 Rust결과다.

## 6. 로컬 산출물 및 유지 상태

제품 소스6cabc21b 기준 **target/bundle-0.2.2/Deppy Sijo.app** 및ZIP. Developer ID서명/strict/arm64/두plist버전/앱–ZIP3바이너리동일/helpernotice 검증 성공. manifest에 관련 sourceSHA256/제품commit 기록. Apple notarization/Gatekeeper production 승인은 실행하지 않았다.

**앱 재실행 없음.** 실행 중 target/bundle0.2.0/PID44402 및 사용자 터널52115/설정/동의 유지. 새release는별도폴더에만 만든다.

[전체 원본 실측·review·gate·artifact·CI 증거](measurements/2026-09-28-followup-efficiency-audit/). 로그 끝 개행만 정규화하고 원본SHA256 보관. 후속 운영 gateway 선정/CI계정 해소/병합은 별도 단계다.
