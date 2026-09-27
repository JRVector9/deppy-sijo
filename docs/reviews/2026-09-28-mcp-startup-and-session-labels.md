# MCP 연결 지연과 공유 세션 표시 — 2026-09-28

## 완료 내용

- 지연 수정 `bffcf483`, 세션 표시 `4ad811af`. 브랜치 `feat/automatic-mcp-connect`.
- 버전 0.2.0 → **0.2.1**. 앱/프록시 release 빌드 19.29초 성공. 실행 중인 0.2.0 앱과 사용자 터널은 유지했다. 재실행하지 않았다.
- 공유 세션: `simpleHWP · 에이전트 1 / Codex · gpt-6-astra · xhigh`. 기존 세션 제목 및 감지된 AI/모델/추론 강도 formatter를 재사용한다. 일반 세션은 지역화된 `셸`; 감지되지 않은 모델·강도를 추정하지 않는다. 기존 공유/입력 동의와 세션 ID를 유지한다.

## 연결 실측

각 실행은 다른 임시 공개 주소와 별도 로컬 MCP 서버/소유한 helper로 측정했다. 실제 앱을 재실행하지 않았다.

| 소스/단계 | 실제 Ready 시간 | 해석 |
|---|---|---|
| 변경 전 2회 | 80.267 / 79.655초 | 주소 생성은 3.367 / 3.370초; 시스템 DNS에서 이름 없음 오류 지속 |
| 시스템 DNS + Cloudflare HTTPS DNS | 15.466 / 15.943 / 65.805초 | 중앙값은 줄었지만 긴 지연 잔존 |
| 첫 터널 등록 이후 probe | 13.351 / 12.368 / 67.069초 | 등록 대기만으로 긴 지연 해소 불가 |
| 최종: 독립 DNS를 지연된 보조 경로로 추가 | **15.467 / 17.439 / 11.241초** | 최종 3회 중앙값 15.467초; 변경 전 79.961초보다 약 80.7% 감소 |

최종 3회에는 60초 이상 지연이 없었다. 이 작은 표본만으로 모든 환경의 최악 지연이 없어졌다고 주장하지 않는다. 외부 DNS 게시 및 터널 서비스 지연은 여전히 존재한다. 정확한 시스템 DNS 음성 캐시의 위치는 규명하지 않았다.

## 구현 및 근거

- 기본 시스템 DNS를 먼저 사용하고, 검증된 자동 `*.trycloudflare.com` HTTPS 호스트의 DNS 실패만 좁게 복구한다. 일반/수동 호스트 및 적용되는 프록시는 기존 경로를 유지하고 `NO_PROXY`는 존중한다.
- DNS fallback 총 1초가 원래 요청 deadline을 공유한다. 보조 resolver를 사용할 때 각 공급자 최대 500ms. 첫 usable 응답에서 중단해 TCP/TLS/metadata 검증 시간을 남긴다. DNS 응답 크기, 이름, 유형, 상태와 공개 IP를 검증한다. TLS 인증서/SNI/Host 및 실제 MCP resource 검증을 유지한다.
- 첫 등록 로그 이후 공개 검증을 시작한다. 후속 unregister로 등록 flag를 초기화하지 않으며, Ready는 실제 공개 HTTPS 확인으로 판단한다. 소유한 프로세스만 취소·회수한다.
- 보조 Google HTTPS DNS는 probe agent 생성 20초 후에만 사용한다. 새 DNS가 게시되기 전에 두 resolver 모두 음성 캐시를 만드는 상황을 줄이는 보조 경로다. 사용자 세션/토큰은 DNS 요청에 포함되지 않는다.
- 공식 자료: [Cloudflare JSON HTTPS DNS](https://developers.cloudflare.com/1.1.1.1/encryption/dns-over-https/make-api-requests/dns-json/), [Google JSON HTTPS DNS](https://developers.google.com/speed/public-dns/docs/doh/json), [고정 helper 버전의 URL 출력·서버 시작 순서](https://raw.githubusercontent.com/cloudflare/cloudflared/2026.9.1/cmd/cloudflared/tunnel/quick_tunnel.go). 사용자 글로벌 DNS 변경, 캐시 flush, 추가 설치는 하지 않았다.

## 테스트와 코드 리뷰

- 최종 `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent`: **29 통과, 2 ignored**. 별도 실행한 live startup 3회 모두 통과.
- ignored 공개 MCP end-to-end를 실제 실행: **1 통과, 14.03초**. 실제 HTTPS 도구 호출 → 별도 실제 PTY 입력 → 출력 읽기 및 클라우드 에이전트 자신의 답변 수신. 중복 입력 방지/소유 helper 회수 확인. OAuth 설정은 로컬 테스트 fixture이며 실제 Grokbot 계정 인증은 검증하지 않았다.
- 프로젝트 이름 관련 5개, i18n 8개, boundary, fmt/whitespace 통과.
- 선택 실행한 기존 `agent_info_line` 테스트는 `Idle` vs `Awaiting instruction`에서 실패했다. [이전 원본/최종 코드에서도 확인된 실패](2026-09-27-seven-improvements-final.md). 해당 status 계약을 이번 변경으로 바꾸거나 테스트 기대값을 녹색으로 맞추지 않았다. 새 공유 목록 테스트는 실제 formatter와 UI label을 독립적으로 검증하며 통과했다. 전체 workspace 테스트가 모두 통과했다고 주장하지 않는다.
- 실제 source diff만 Codex CLI 리뷰. 첫 리뷰의 usable A 뒤 불필요 AAAA 대기, NO_PROXY, 예약 IP 3개 지적을 실패 테스트로 재현하고 수정했다. 이후 DNS/표시/등록 gating/독립 resolver 최종 범위 리뷰는 추가 지적 없음.

## 실패한 접근과 보정

- 등록 fixture의 최초 0.15초 대기가 기존 1초 probe 주기 때문에 잘못 녹색이었다. 1.25초 marker 조건으로 실제 RED를 얻고 수정 후 통과했다.
- 같은 Mac의 공개 테스트 클라이언트도 기존 DNS 음성 캐시로 실패했다. 테스트에서도 검증된 resolver를 사용한다. 보조 delay=0은 `cfg(test)` 전용이며 실제 앱 probe는 20초다.
- 새 DNS stub의 nonblocking listener에서 accepted socket read가 WouldBlock였다. accepted stream을 blocking + 1초 read timeout으로 설정한 후 최종 cloud 29개 통과했다.
- ZIP 생성과 검증을 동시에 시작한 최초 검증은 archive read 오류로 실패했다. 최종 staged ZIP 작성 완료 후 순차 검증하여 통과했다.
- 부분 stage patch 분리 오류를 발견하고 정상 hunk로 수정·amend했다. 최종 두 제품 커밋에 올바른 파일 범위를 기록했다.

## 빌드 산출물

`target/bundle-0.2.1/Deppy Sijo.app`, `target/bundle-0.2.1/Deppy Sijo.zip`.

Developer ID 서명/strict 검증, arm64, 앱/ZIP 바이너리 해시 일치, helper notice와 두 plist 버전 0.2.1 검증 완료. About 버전은 compile-time Cargo 버전이다. GUI/About 화면은 실행하지 않았다. Apple notarization/Gatekeeper production 배포 승인은 수행하지 않았다. 현재 실행 중 bundle 파일은 덮어쓰지 않았다.

[원본 측정·리뷰·테스트·artifact 증거](measurements/2026-09-28-mcp-startup-and-session-labels/). 로그 끝 개행만 정규화했고 원본 해시 manifest를 보관했다.

## 이어지는 요청

사용자가 이 작업 완료 후 연결 속도 추가 웹 조사와 전체 코드 처리/메모리 비효율 검토, 검증된 개선의 PR 개발을 요청했다. 이번 범위는 완료이며 이어서 조사·측정·독립 PR 개발한다. 앱 재실행 승인 없음.
