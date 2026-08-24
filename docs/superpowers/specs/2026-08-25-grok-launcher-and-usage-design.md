# Grok 런처 모델·추론 강도 및 잔여 사용량 설계

상태: 2026-08-25 사용자 승인, 구현 전

## 1. 목표

에이전트 런처에서 Grok을 선택했을 때 설치된 Grok CLI가 실제로 제공하는 모델과 각
모델별 추론 강도를 선택할 수 있게 한다. 런처로 시작한 세션에는 선택한 값을 Grok의
공식 `--model`과 `--reasoning-effort` 인자로 전달한다.

하단 provider 사용량 영역에는 Grok 계정의 다음 세 값을 함께 표시한다.

- 주간 잔여 비율
- 월간 잔여 비율
- 남은 크레딧

Grok이 설치되지 않았거나, 로그인하지 않았거나, `/usage` 화면에서 유효한 수치를 얻지
못한 경우에는 Grok 칸을 만들지 않는다. 사용량 조회 때문에 렌더 스레드를 막거나 사용자
프로젝트·세션·자격증명을 건드리지 않는다.

## 2. 확인된 현재 상태와 실측

2026-08-25 현재 기기에 설치된 Grok CLI는 `grok 1.0.5`다.

- `grok --help`는 `--model <MODEL>`과 `--reasoning-effort <EFFORT>`를 공식 실행
  옵션으로 제공한다.
- `grok models`는 기본 모델을 `grok-4.6`, 사용 가능 모델을 `grok-4.6`과
  `grok-4.5`로 보고한다.
- `~/.grok/config.toml`의 `[models]`에는 `default = "grok-4.6"`과
  `default_reasoning_effort = "medium"`이 있다.
- `~/.grok/models_cache.json`의 최상위 `models`는 배열이 아니라 모델 ID를 키로
  사용하는 객체다. 각 값의 공개 모델 메타데이터는 `info` 아래에 있고, 같은 값에는
  API endpoint·환경 키 이름·자격증명처럼 런처 카탈로그에 필요하지 않은 필드도 있다.
- `grok-4.6`은 `xhigh`, `high`, `medium`, `low`를 제공하고, `grok-4.5`는
  `high`, `medium`, `low`를 제공한다.

현재 `crates/app/src/agent_model_catalog.rs`의 `parse_grok`은 `models`를
`Vec<serde_json::Value>`로만 받는다. 실제 객체 스키마는 역직렬화 단계에서 통째로
실패하고, 런처는 `crates/app/src/agent_launcher.rs`의 내장 `grok-4.5` 한 항목으로
폴백한다. 따라서 범용 모델/강도 UI와 실행 인자 생성 코드가 이미 있어도 실제
`grok-4.6` 및 `xhigh` 선택지가 보이지 않는다.

현재 Grok 설정 리더는 기본 모델만 읽고 `default_reasoning_effort`는 읽지 않는다.
런처 UI는 선택 모델의 카탈로그 기본 강도를 사용하므로, CLI 설정의 `medium`과 다른 값이
초기 선택될 수 있다.

하단 사용량 바는 Claude, Codex, Kimi만 받는다. provider 수에 따른 폭은
`50 + 190 * count`이고 현재 테스트 범위는 0~3칸이다. Grok 사용량 모듈과 전용 데이터
타입은 없다.

설치된 Grok의 공식 사용자 가이드에서 `/usage`(`/cost` 별칭)는 크레딧 사용과 결제
관리를 보여주는 명령이다. 설치 바이너리의 화면 문자열에는 `WEEKLY`, `MONTHLY`,
`Weekly limit`, `Monthly limit`, `Next reset`, `Credits left`, percentage 및
`used of $ limit` 표현이 존재한다. 반면 비대화형 `grok usage` 하위 명령은 제공되지
않는다.

## 3. 사용자 동작 계약

### 3.1 런처

Grok 카드가 활성화된 상태에서 카드를 선택하면 모델 콤보와 추론 강도 콤보를 모두
표시한다.

- 현재 객체 캐시가 정상이라면 모델은 `grok-4.6`, `grok-4.5`를 제공한다.
- `grok-4.6` 선택 시 `low`, `medium`, `high`, `xhigh`만 제공한다.
- `grok-4.5` 선택 시 `low`, `medium`, `high`만 제공한다.
- 모델을 바꾸면 새 모델이 지원하지 않는 기존 강도는 그 모델의 기본 강도로 조정한다.
- 처음 Grok을 선택할 때 설정된 `default = "grok-4.6"`과
  `default_reasoning_effort = "medium"`이 유효하면 각각 초기 모델·강도로 사용한다.
- 설정 강도가 선택 모델의 지원 목록에 없거나 모르는 값이면 카탈로그가 선언한 기본
  강도, 그것도 없으면 지원 목록의 첫 값으로 폴백한다.
- 시작 버튼을 누르면 선택한 값이 `grok --model <model> --reasoning-effort <effort>`에
  포함된다. 실행 계약은 선택 모델이 지원하지 않는 강도를 계속 거부한다.

캐시가 없거나 손상됐을 때도 런처가 비어서는 안 된다. 내장 폴백은 현재 CLI 기준으로
`grok-4.6`과 `grok-4.5` 두 모델을 제공하며 각 모델별 강도 목록을 유지한다. 설정에만
존재하는 모델은 기존 `adopt_model` 계약대로 목록에 보존하되, 알 수 없는 모델에는
보수적인 Grok 강도 폴백만 적용한다.

### 3.2 하단 사용량

유효한 값이 있으면 Grok 로고 뒤에 가능한 값을 한 줄로 표시한다. 한국어의 압축 표기
예시는 다음과 같다.

```text
주 70% · 월 85% · $12.34
```

비율은 모두 **남은 양**이다. 화면이 `N% used`를 제공하면 `100 - N`으로 변환하고,
`$used of $limit`만 제공하면 고정소수점 정수 계산으로 `(limit - used) / limit`을
구한다. `Credits left`는 통화 기호와 소수 두 자리까지 보존하되 부동소수점으로 저장하지
않는다.

부분 성공을 허용한다. 예를 들어 주간 값과 크레딧만 확실하면 `주 70% · $12.34`처럼
두 값만 표시한다. 세 필드가 모두 없으면 Grok 칸 자체를 숨긴다. 런처 설정에서 Grok을
비활성화한 경우에는 캐시된 사용량이 있어도 칸을 숨긴다.

## 4. 선택한 아키텍처

### 4.1 모델 카탈로그: 현재 객체와 레거시 배열을 함께 수용

`agent_model_catalog.rs`의 Grok 캐시 경계를 두 스키마를 수용하는 형태로 바꾼다.

```text
현재 스키마
models: { "grok-4.6": { "info": { ...public model metadata... } } }

레거시/fixture 스키마
models: [ { ...model metadata... } ]
```

현재 객체 값에서는 `info`만 typed 모델 메타데이터로 변환한다. 객체 키를 `info.id`가
없을 때의 모델 ID 폴백으로 사용한다. 캐시 값의 API key, endpoint, 환경변수 이름은
구조체 필드로 선언하거나 복사하지 않으며 Serde의 unknown-field 무시 경계 밖에 둔다.
항목 하나가 깨져도 나머지 모델은 살리고, 모델 수는 기존 64개 상한을 유지한다.

객체 키에는 순서 계약이 없으므로 설정 기본 모델을 첫 항목으로 두고 나머지는 모델 ID로
안정 정렬한다. 배열 스키마는 기존 파일 순서를 유지한다. 중복 ID는 첫 유효 항목만
남긴다. 알 수 없는 강도는 버리고, 지원 목록 안에 있는 명시 기본값만 채택한다.

Grok 설정 리더는 `[models]`를 한 번 파싱해 기본 모델과 기본 추론 강도를 함께 반환한다.
감지 worker가 이를 `DetectedAgent`의 초기 선택 정보로 운반한다. 런처 UI는 에이전트를
처음 선택하거나 감지 결과로 현재 선택이 무효가 된 경우에만 설정 강도를 우선하며,
사용자가 모델/강도를 직접 바꾼 뒤에는 그 선택을 덮어쓰지 않는다.

### 4.2 사용량: 숨긴 bounded PTY에서 공식 `/usage` 사용

새 `crates/app/src/grok_usage.rs`가 Grok 사용량의 유일한 조회·파싱 경계가 된다.

```text
App status render
  → grok_usage::current(ctx): 캐시 조회 + 필요할 때 worker 예약
  → 전용 background thread
  → 격리된 usage-probe cwd에서 Grok PTY 시작
  → `/usage` 입력
  → 제한된 terminal output 정리·파싱
  → latest-only channel로 GrokUsage 반환
  → 다음 repaint에서 하단 provider bar 표시
```

구현은 검증된 `kimi_usage.rs` 패턴을 재사용한다.

- 설치된 실행 파일이 알려진 경로에 있을 때만 동작한다.
- 조회 간격은 60초다.
- 동시에 한 프로브만 허용하고 결과 채널 용량은 1이다.
- 시작 대기 2초, 전체 제한 25초, 패널 감지 뒤 안정화 대기 2초를 상한으로 둔다.
- 출력은 최신 100 KiB만 보존한다.
- 전용 `~/.deppy-sijo/usage-probe` 또는 임시 디렉터리를 cwd로 사용한다.
- 완료·실패 후 PTY를 종료하고 repaint는 결과 도착 시 한 번만 요청한다.
- 렌더 함수는 캐시 확인과 worker admission만 수행하며 프로세스 대기·파일 읽기·파싱을
  하지 않는다.

전용 projection은 기존 `(5시간, 주간)` 별칭에 억지로 맞추지 않는다.

```text
GrokUsage
  weekly_remaining_percent: Option<u8>
  monthly_remaining_percent: Option<u8>
  credits_left_minor: Option<u64>
  currency: Option<bounded symbol/code>
```

금액은 minor unit 정수로 파싱하고 표시 시에만 소수점 문자열로 만든다. 비율은 0~100으로
clamp하며, 한도 0·음수·overflow·잘못된 숫자는 그 필드만 버린다. 마지막 성공 값은 새
성공으로만 교체한다. 이후 프로브가 실패하면 최대 10분 동안 마지막 성공 값을 유지하고,
10분이 지나면 숨긴다. 이 유예는 일시적인 네트워크/TUI 재그리기 실패로 상태바가 매분
깜빡이는 것을 막으면서도 오래된 계정 상태를 계속 보여주지 않는 경계다.

### 4.3 상태바 통합

`App`은 Claude/Codex/Kimi 조회와 같은 지점에서 `grok_usage::current(ui.ctx())`를
호출하고 typed `GrokUsage`를 `AgentTerminalUi::status_bar_with_managers`에 전달한다.
`top_provider_usage`는 기존 provider renderer와 별도의 작은 Grok renderer를 사용한다.
기존 `ProviderUsage`의 5시간/주간 진행 바 의미를 Grok의 월간/주간/크레딧에 재사용하지
않는다.

Grok은 Kimi와 같은 opt-in 표시 규칙을 쓴다.

- Grok 활성 + 사용량 일부 있음: 칸 표시
- Grok 활성 + 사용량 없음: 칸 없음
- Grok 비활성: 값이 있어도 칸 없음

provider 폭 함수는 동일한 칸당 190px 공식을 유지하며 4칸 `810px` 회귀 테스트를
추가한다. 좁은 창에서는 기존 status bar 클리핑/레이아웃 정책을 그대로 사용하고 새
가로 스크롤이나 periodic repaint를 만들지 않는다.

## 5. 파싱 규칙

PTY 출력은 ANSI CSI/OSC와 carriage-return 재그리기를 먼저 제거한 뒤 줄 단위로 읽는다.
같은 패널이 여러 번 그려지면 마지막 완성 블록의 값을 우선한다.

- `weekly`, `weekly limit` 라벨 근처의 percentage/limit만 주간 값으로 인정한다.
- `monthly`, `monthly limit` 라벨 근처의 percentage/limit만 월간 값으로 인정한다.
- `N% used`는 `100 - min(N, 100)`이다.
- `N% left` 또는 `N% remaining`은 `min(N, 100)` 그대로다.
- `$used of $limit`는 금액을 minor unit으로 변환한 뒤 남은 비율을 정수 반올림한다.
- `Credits left` 뒤 금액만 크레딧 잔액으로 인정한다.
- 라벨과 값이 TUI 줄바꿈으로 갈릴 수 있어 라벨 줄부터 최대 네 줄까지만 탐색한다.
- `context`, `token`, 대화 압축, 모델 context window 근처의 percentage는 사용량으로
  인정하지 않는다.
- 로그인 안내, 결제 관리 안내, 로드 실패, 빈 패널은 정상적인 `None` 결과다.

파서는 계정명, 이메일, 경로, 토큰, 원문 출력 전체를 반환하지 않는다. 표시 가능한
세 숫자와 제한된 통화 표지만 남긴다.

## 6. 대안과 채택하지 않은 이유

### 6.1 비공개 API 직접 호출

구조화된 응답을 빠르게 받을 수 있지만 Grok의 사설 endpoint 및 인증 저장 형식에
결합되고, Deppy가 Grok 자격증명을 읽거나 재사용해야 한다. 캐시의 API key 필드가
존재하더라도 이 기능의 권한 경계를 넓히므로 채택하지 않는다.

### 6.2 사용자가 직접 `/usage`를 실행했을 때만 수동 수집

별도 프로세스가 필요 없지만 사용자가 명령을 실행하지 않은 계정에는 값이 없고, 다른
세션·워크스페이스에서 갱신된 상태를 안정적으로 모으기 어렵다. 하단 잔여 상태의 지속성
요구를 충족하지 못해 채택하지 않는다.

### 6.3 채택안: 공식 UI를 제한된 PTY로 조회

Grok CLI가 자신의 인증과 서버 계약으로 값을 가져오므로 Deppy는 자격증명을 읽지 않는다.
TUI 문구가 바뀌면 파서 유지보수가 필요하지만, frozen screen fixture와 보수적 partial
parse로 변경을 탐지하고 잘못된 수치를 표시하지 않는 방향으로 실패시킨다.

## 7. 오류·보안·성능 계약

- 모델 캐시와 설정 읽기는 기존 8 MiB 상한, regular UTF-8 파일, 실패 허용 규칙을
  유지한다.
- 캐시의 비공개 필드는 역직렬화 대상, 로그, UI, Debug 출력에 포함하지 않는다.
- 사용량 프로브는 raw PTY output, 계정 식별자, 홈 경로, 인증 상태 세부값을 로그에
  남기지 않는다.
- 프로브 thread는 하나, pending 결과는 하나, 출력은 100 KiB, 실행은 25초로 제한한다.
- `grok`이 설치되지 않았으면 thread와 provider 칸을 모두 만들지 않는다.
- 프로브 실패는 앱 오류나 toast로 승격하지 않고 마지막 신선 값 또는 숨김으로 처리한다.
- 렌더마다 새 문자열 정규식·프로세스·파일 I/O를 만들지 않는다. 정규식은 `OnceLock`,
  표시 문자열은 실제 Grok 칸을 그릴 때만 만든다.
- 이 기능은 terminal snapshot, PTY resize, split presentation fence를 변경하지 않고
  terminal repaint를 요청하지 않는다. worker 결과 도착에 대한 egui repaint만 한 번
  요청한다.

## 8. 국제화와 접근성

하드코딩한 한국어 라벨 대신 다섯 locale(`ko-KR`, `en-US`, `ja-JP`, `zh-Hans`,
`zh-Hant`)에 동일한 키 집합을 추가한다.

- 주간 잔여 압축 라벨
- 월간 잔여 압축 라벨
- 크레딧 잔액 압축 라벨
- Grok usage 접근성 라벨
- 값이 부분적으로 존재할 때 사용하는 hover 설명

로고에는 기존 provider 로고와 같은 접근성 이름을 주고, 합쳐진 사용량 텍스트에는
화면에 실제 표시된 필드만 포함한 완전한 접근성/hover 문장을 제공한다. 색상만으로
주간·월간·크레딧을 구분하지 않는다.

## 9. TDD 및 검증 설계

구현 전에 다음 실패 테스트를 각각 관찰한다.

### 9.1 모델 카탈로그와 설정

- 실측 객체형 fixture가 `grok-4.6`/`grok-4.5`와 모델별 강도를 만든다.
- 객체 값의 `info.id`가 없으면 객체 키를 모델 ID로 사용한다.
- API key 등 sibling 필드가 모델 결과나 Debug 표면에 노출되지 않는다.
- 객체 항목 하나가 깨져도 나머지는 남고, 중복/64개 상한이 적용된다.
- 기존 배열 fixture가 계속 동일하게 파싱된다.
- 손상/잘못된 `models` 형식은 빈 결과로 폴백한다.
- `[models] default`와 `default_reasoning_effort`를 함께 읽고, 잘못된 위치·빈 값·알 수
  없는 강도는 무시한다.
- 캐시가 없을 때 내장 4.6/4.5 목록과 각 모델의 강도가 제공된다.

### 9.2 런처 동작과 실행 계약

- Grok을 처음 선택하면 설정된 4.6/medium이 선택된다.
- 4.6에서 xhigh를 선택할 수 있고 4.5로 바꾸면 지원 강도로 조정된다.
- 사용자가 직접 고른 유효 강도는 감지 reconcile이 덮어쓰지 않는다.
- 4.6/xhigh 및 4.5/high가 각각 정확한 `--model`/`--reasoning-effort` 인자를 만든다.
- 4.5/xhigh처럼 모델이 지원하지 않는 조합은 실행 전에 거부된다.

### 9.3 사용량 순수 파서

- 주간/월간 `N% used`가 각각 남은 비율로 변환된다.
- `N% left` 및 `$used of $limit` 화면도 같은 남은 비율을 만든다.
- `Credits left`의 정수·소수 금액이 정확한 minor unit이 된다.
- 주간/월간/크레딧 일부만 있는 화면은 부분 결과를 보존한다.
- 반복 TUI redraw에서는 마지막 완성 값이 선택된다.
- context/token percentage는 사용량으로 오인하지 않는다.
- 로그인 안 됨, 무료/결제 안내, 로드 실패, 손상 숫자, ANSI-only 출력은 `None` 또는
  해당 필드 없음으로 처리한다.
- overflow, 0 limit, 100% 초과 입력은 panic 없이 보수적으로 처리한다.

### 9.4 worker와 상태바

- Grok 실행 파일이 없으면 worker를 시작하지 않는다.
- 60초 refresh, pending 1개, 성공 값 교체, 실패 시 10분 보존, 10분 후 숨김을 순수
  상태 전이 테스트로 고정한다.
- Grok 활성/값 있음, 활성/값 없음, 비활성/값 있음의 세 표시 규칙을 kittest로 검증한다.
- 주간·월간·크레딧 전체와 각 partial 조합이 올바른 locale/accessibility 문자열을 만든다.
- provider 폭은 0, 1, 2, 3, 4칸에서 각각 `0`, `240`, `430`, `620`, `810`이다.

### 9.5 통합 게이트

- focused `agent_model_catalog` tests
- focused Grok `agent_launcher` 및 `ui::agent_launcher` tests
- `grok_usage` module tests
- provider status kittests와 App wiring tests
- `cargo test -p deppy-sijo --locked -- --test-threads=1`
- `cargo test -p i18n --locked -- --test-threads=1`
- strict all-target Clippy with `-D warnings`
- `cargo fmt --all -- --check`
- `cargo run -p xtask -- i18n-check`
- `cargo run -p xtask -- check-boundary`
- `git diff --check`
- `codex review --uncommitted` 후 지적 재현·수정·관련 회귀 재실행
- signed macOS release rebuild, deep/strict codesign verification, 정확한 bundle relaunch

실제 계정 PTY probe는 자격증명과 네트워크 상태가 필요한 bounded ignored 측정 테스트로만
두고 자동 테스트의 통과 근거로 사용하지 않는다. 자동 테스트는 비식별 frozen 화면 fixture를
사용한다.

## 10. 수동 인수 기준

1. 런처에서 Grok을 선택하면 4.6과 4.5를 선택할 수 있다.
2. 4.6에는 xhigh/high/medium/low, 4.5에는 high/medium/low만 보인다.
3. 초기 선택이 설치된 Grok 설정의 4.6/medium과 일치한다.
4. 실행된 Grok 세션의 모델·강도가 런처 선택과 일치한다.
5. 유효한 계정은 하단에 주간 잔여, 월간 잔여, 크레딧 잔액이 함께 보인다.
6. 한 필드가 없는 계정은 나머지 확실한 값만 보이고 잘못된 `0`이나 `—`를 만들지 않는다.
7. Grok 비활성, 미설치, 로그아웃, 사용량 로드 실패 상태에서는 Grok 칸이 없다.
8. 네트워크를 잠시 끊어도 마지막 성공 값은 10분 내에서 유지되고 그 뒤에는 사라진다.
9. 장시간 idle 상태에서 프로브가 중첩되지 않고 CPU·메모리·repaint가 계속 증가하지 않는다.
10. Grok 사용량 갱신 시 terminal 내용, cursor, split 크기, resize presentation이 변하거나
    깜빡이지 않는다.

## 11. 범위 밖

- Grok 결제 페이지 열기, 크레딧 구매·충전·한도 변경
- Grok 비공개 API 또는 인증 파일 직접 사용
- 원본 `/usage` 화면이나 계정 식별 정보 저장
- 사용량 이력 그래프·영속 DB·알림 임계값
- Grok CLI 자체 모델 캐시 갱신 강제 실행
- Claude/Codex/Kimi 사용량 데이터 모델 통합 재설계
- terminal resize/repaint 파이프라인 변경
