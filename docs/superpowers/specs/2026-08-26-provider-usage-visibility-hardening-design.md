# 공급자 사용량 가시성 강화 설계

상태: 2026-08-26 사용자 승인, 구현 전

## 목적

Claude, Codex, Grok, Kimi의 하단 사용량 표시가 설치 감지, 첫 조회, 일시 실패,
오래된 캐시, 비활성 설정, 좁은 창에서 일관되게 동작하도록 한다. 설치되고 활성화된
공급자는 숫자를 아직 얻지 못했더라도 로고와 `—` 자리표시자를 유지한다. 비활성 또는
미설치 공급자는 칸과 백그라운드 프로브를 모두 만들지 않는다.

현재 Kimi 0.38.0의 앱 동일 PTY 실측은 사용량을 읽지 못해 `None`을 반환한다. 현재
UI는 이 결과를 미설치와 동일하게 처리해 Kimi 칸 전체를 숨긴다. Claude와 Kimi의 PTY는
런처보다 좁은 별도 실행파일 탐지를 사용하며 입력 큐의 거부 상태도 확인하지 않는다.

## 접근안 비교

### A. 공급자 공통 표시 상태와 런처 감지 경로 공유, 채택

표시 경계에 `Hidden`, `Unavailable`, `Available` 세 상태를 둔다. 설치·활성 여부는
App이 런처 감지 스냅샷과 설정으로 확정하고, 프로브는 사용량 값만 책임진다. Claude와
Kimi의 프로브는 Grok처럼 감지된 실행파일과 launch PATH를 받는다.

장점은 `Option` 하나에 여러 의미가 섞이지 않고 새 공급자가 추가돼도 동일한 표시
계약을 재사용할 수 있다는 점이다. 설치 감지와 프로브 실행파일이 갈라지는 문제도 함께
없어진다.

### B. Kimi에만 `Option<Option<ProviderUsage>>` 추가, 제외

변경량은 작지만 Claude와 Kimi의 탐지 경로 분리, disabled 프로브, 입력 거부 문제를
남긴다. 다음 공급자에서 같은 결함이 반복될 가능성이 높다.

### C. 값이 없으면 무조건 모든 공급자를 표시, 제외

미설치 공급자까지 `—`로 보여 잘못된 상태가 된다. 프로브 실행 여부도 해결하지 못한다.

## 상태와 데이터 흐름

```text
launcher detection + disabled config
  ├─ 미설치 또는 비활성 → Hidden, 프로브 없음
  └─ 설치 + 활성
       ├─ 첫 조회/실패/만료 → Unavailable, 로고 + —
       └─ 유효한 값 → Available(value), 로고 + 숫자
```

App은 감지된 `DetectedAgent`를 Kimi, Grok, Claude 프로브에 전달한다. 각 프로브는
감지된 절대 실행파일과 bounded launch PATH를 사용한다. Codex는 기존 app-server 우선,
백엔드 빈 창 보충 계약을 유지한다.

프로브 성공값은 기존처럼 마지막 성공 시각과 함께 보존한다. 일시 실패가 마지막 성공값을
즉시 지우지 않지만, 창별 stale 한도를 넘으면 `Unavailable`로 바뀐다. PTY 입력 enqueue가
`Accepted`가 아니면 정상적인 무응답으로 삼지 않고 해당 프로브 실패로 끝낸다.

## 렌더링 계약

- 순서는 Claude, Codex, Grok, Kimi다.
- `Hidden`은 폭과 구분선을 만들지 않는다.
- `Unavailable`은 공급자 로고와 `—`를 표시한다.
- `Available`은 현재 숫자 표시 형식을 보존한다.
- 좁은 창에서는 공급자 로고와 핵심 숫자를 먼저 보존하고, 진행 막대·보조 창 라벨·플랜
  라벨 순서로 압축한다.
- 26pt 한 줄 높이는 유지하며 가로 스크롤이나 주기적 repaint를 추가하지 않는다.
- 세션/MCP 등 뒤쪽 상태가 공급자 영역을 덮지 않도록 실제 가용 폭으로 provider 영역을
  제한한다.

## 성능과 오류 처리

- 비활성 또는 미설치 공급자는 파일 검사, thread spawn, PTY 실행을 하지 않는다.
- 공급자별 single-flight와 refresh/stale 주기를 유지한다.
- thread spawn 실패도 마지막 요청 시각을 기록해 매 프레임 재시도하지 않는다.
- raw PTY 출력, 계정 식별자, 토큰, 홈 경로는 로그나 UI에 남기지 않는다.
- 결과 도착 시 repaint 한 번만 요청한다.

## 테스트 계약

구현 전 다음 RED를 각각 관찰한다.

1. 감지되고 활성화된 Kimi가 값이 없어도 `Kimi logo`와 `—`를 표시한다.
2. 미감지 또는 비활성 Kimi는 칸을 숨긴다.
3. 비활성 Kimi와 Claude는 프로브 admission을 하지 않는다.
4. Kimi와 Claude는 런처가 감지한 절대 실행파일과 launch PATH를 사용한다.
5. PTY 입력이 backpressure 또는 closed로 거부되면 프로브가 실패한다.
6. Kimi 0.38.0의 현재 설치본에서 `/status` 또는 `/usage` 중 실제 지원 경로가 숫자를
   반환한다. 계정에 표시 가능한 plan usage가 없다면 자동 테스트는 `Unavailable` 상태를
   검증하고 숫자를 만들지 않는다.
7. 300, 430, 620, 810, 1,200pt 폭에서 모든 감지·활성 공급자의 로고가 clip rect 안에
   남고 Kimi가 마지막이라는 이유로 사라지지 않는다.
8. 기존 Codex 병합, Grok Finder 최소 환경, 상태바 접근성·순서 테스트가 그대로 통과한다.

## 완료 기준

- Kimi의 값 없음이 더 이상 칸 없음으로 보이지 않는다.
- 런처에서 실행 가능한 Claude/Kimi는 동일 실행파일과 PATH로 사용량 조회를 시도한다.
- 비활성 공급자 프로브가 0회임을 테스트로 증명한다.
- 현재 Kimi와 Grok 설치본의 live regression 결과를 기록한다.
- focused tests, strict all-target Clippy, full App tests, rustfmt, `git diff --check`, 독립
  Codex 리뷰가 통과한다.
