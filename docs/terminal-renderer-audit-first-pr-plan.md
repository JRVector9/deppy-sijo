# Terminal Renderer 개선 구현 계획

## 1. 목적

현재 구현을 전면 교체하지 않는다.

각 영역을 먼저 점검한 뒤 아래 기준으로 처리한다.

1. 목표에 부합하고 안정적이면 **유지**
2. 일부 부족하면 **보완**
3. 구조적으로 목표 달성이 어렵거나 결함이 있으면 **재구현**
4. 구현 여부가 불명확하면 계측·테스트 후 결정
5. 모든 변경은 작은 PR 단위로 진행하고, 성능 회귀 시 즉시 롤백 가능해야 한다

---

## 2. 현재 확인된 구성

| 영역 | 현재 구현 | 기본 판단 |
|---|---|---|
| PTY | `portable-pty 0.9` | 유지 우선 |
| 터미널 엔진 | `alacritty_terminal 0.26` | 유지 우선 |
| ANSI Parser | `alacritty_terminal` 내부 VTE | 별도 `vte` 중복 사용 여부 점검 |
| UI | `egui / eframe` | 일반 UI는 유지 |
| GPU | `glow / OpenGL` | `wgpu` 전환 검토 |
| 터미널 텍스트 | `egui epaint` | 성능·품질 점검 후 전용 렌더러로 교체 가능 |
| Window | `winit` 간접 사용 | 유지 |
| Clipboard | `arboard 3.6` | 유지하되 비동기 처리 점검 |
| Image | `image 0.25` | 유지하되 PNG 처리 경로 점검 |

---

## 3. 최종 목표

### 렌더링

- macOS에서 `wgpu`의 Metal backend 사용
- Windows에서는 D3D12, Linux에서는 Vulkan으로 확장 가능
- 터미널 영역은 일반 egui 텍스트 위젯이 아닌 전용 GPU 렌더 경로 사용
- Retina 배율에서 선명한 글자와 안정적인 셀 정렬 보장
- 화면에 보이는 워크스페이스만 적극 렌더링
- 숨겨진 워크스페이스는 dirty 상태만 누적하고 활성화 시 한 번 갱신

### 메모리

- 워크스페이스 생성·삭제 반복 시 메모리 누적 없음
- GPU Device, Queue, FontSystem, Glyph Cache, Atlas는 앱 전체 공유
- 워크스페이스마다 렌더러·폰트 캐시·대형 텍스처를 만들지 않음
- 스크롤백과 세션 로그는 제한·지연 로딩
- 앱 RSS와 자식 프로세스 RSS를 분리 측정

### 입력 및 선택

- 일반 드래그 선택
- 더블클릭 단어 선택
- 트리플클릭 줄 선택
- 드래그 중 가장자리 자동 스크롤
- Shift+드래그로 터미널 프로그램의 마우스 모드 우회
- 선택 영역 강조와 복사 지원

### 이미지 붙여넣기

- 클립보드 읽기, 이미지 변환, PNG 인코딩, 임시파일 저장을 UI 스레드에서 수행하지 않음
- 파일 URL이 있으면 이미지 디코딩 없이 파일 경로를 우선 전달
- 불필요한 RGBA·PNG·Base64 중복 복사 제거
- 진행 중 작업 취소 및 중복 붙여넣기 제어

---


## 3-A. 확정 목표 아키텍처

```text
Application
│
├── Runtime
│   ├── PTY Manager
│   ├── Session Manager
│   ├── Workspace Manager
│   ├── Clipboard Worker
│   ├── Agent Output Observer
│   └── Agent Event Parser
│
├── Shared GPU Resources
│   ├── 1× Wgpu Instance
│   ├── 1× Adapter
│   ├── 1× Device
│   ├── 1× Queue
│   ├── 1× FontSystem
│   ├── 1× SwashCache
│   ├── Shared Glyph Atlas
│   ├── Pipeline Cache
│   ├── Buffer Pool
│   └── Texture Pool
│
├── Window/Surface Resources
│   └── 창 또는 native surface별 Wgpu Surface 및 SurfaceConfiguration
│
├── Workspace 1
│   ├── alacritty_terminal::Term
│   ├── Dirty State
│   ├── Selection State
│   ├── Cursor/View State
│   └── Small Render Buffer Handle
│
├── Workspace 2
│   └── ...
│
└── Workspace N
```

### 소유권 원칙

- GPU 핵심 자원은 Application 레벨에서 공유한다.
- `Device`, `Queue`, `FontSystem`, `SwashCache`, Glyph Atlas, Pipeline은 Workspace가 소유하지 않는다.
- `Surface`는 GPU 공용 자원이 아니라 창·native surface 수명에 종속되므로 Window 계층에서 관리한다.
- Workspace는 터미널 상태와 작은 렌더 상태만 가진다.
- Scrollback은 가능하면 `alacritty_terminal::Term` 내부 소유권을 그대로 사용하고 별도 중복 버퍼를 만들지 않는다.
- 전체 세션 로그가 필요하면 scrollback과 분리된 디스크 기반 로그 저장소를 사용한다.
- Agent Event Parser는 PTY 원본 스트림을 직접 소유하지 않고 `Agent Output Observer`가 복제·정규화한 이벤트를 입력으로 받는다.
- Workspace별 instance buffer는 전용 GPU 객체를 무조건 하나씩 만드는 대신 shared buffer pool의 slice/handle 사용을 우선한다.
- Glyph Atlas는 논리적으로 공유하되 단일 고정 크기 텍스처를 강제하지 않는다. 크기 제한, 세대 교체, eviction이 가능한 shared atlas manager로 구현한다.

## 3-B. 확정 최종 목표

> 공유 GPU 리소스 + 워크스페이스당 최소 상태 + 이벤트 기반 Dirty Rendering + Steady-State Zero Allocation Rendering

### 의미

- **공유 GPU 리소스:** 앱 전체에서 Device, Queue, FontSystem, Glyph Cache, Pipeline, Pool을 재사용한다.
- **워크스페이스당 최소 상태:** Workspace에는 `Term`, dirty/selection/view 상태와 작은 handle만 둔다.
- **이벤트 기반 Dirty Rendering:** PTY 출력, resize, selection, cursor, theme 변경이 있을 때만 필요한 범위를 갱신한다.
- **Steady-State Zero Allocation Rendering:** 정상 렌더 루프에서는 heap allocation, `Vec` 재할당, `String` 생성, pipeline/texture 생성이 발생하지 않도록 한다.

### Zero Allocation의 범위

다음 시점의 allocation은 허용한다.

- 최초 GPU 초기화
- 최초 폰트·글리프 사용
- 창 크기 증가
- workspace 최초 활성화
- atlas 성장 또는 세대 교체
- 비정상적으로 긴 행이나 대규모 resize

다음은 허용하지 않는다.

- 매 프레임 셀 목록 재할당
- 매 프레임 `String`/`Vec` 새 생성
- 매 프레임 pipeline, bind group, texture 생성
- cursor blink만으로 전체 terminal snapshot 재구성
- 숨겨진 workspace의 반복 GPU upload

### 최종 검증 조건

- Workspace마다 Device/Queue/FontSystem/SwashCache/Atlas가 새로 생성되지 않는다.
- Workspace 생성·삭제 반복 시 GPU 객체와 thread/process가 누적되지 않는다.
- 활성 Workspace만 렌더링하고 비활성 Workspace는 dirty flag만 유지한다.
- 공통 ASCII/CJK 글리프는 Workspace 간 재사용된다.
- steady-state 렌더 경로의 heap allocation 횟수가 0임을 계측으로 확인한다.
- scrollback과 세션 로그를 이중 보관하지 않는다.
- 이미지 붙여넣기 작업은 UI/render thread를 블로킹하지 않는다.

## 4. 공통 점검 규칙

각 PR은 구현 전에 반드시 기존 코드를 먼저 점검한다.

### 판정

#### KEEP

다음 조건을 모두 만족하면 기존 구현을 유지한다.

- 기능 요구사항 충족
- 메모리 누적 없음
- UI 스레드 블로킹 없음
- 워크스페이스 수 증가에 따라 과도하게 비용이 증가하지 않음
- 테스트 또는 프로파일 결과로 확인 가능

#### IMPROVE

기본 구조는 적합하지만 다음 중 일부가 부족하면 보완한다.

- 캐시 공유 미흡
- 작업 스레드 분리 미흡
- dirty 갱신 미흡
- 선택 기능 일부 누락
- 오류 처리·취소 처리 부족
- 성능 계측 부재

#### REPLACE

다음 조건이면 해당 구현을 교체한다.

- 매 프레임 전체 터미널 텍스트를 재생성
- 워크스페이스마다 GPU/폰트 자원 중복 생성
- 이미지 변환을 UI 스레드에서 동기 실행
- 워크스페이스를 열고 닫을 때마다 RSS가 지속 증가
- 선택 기능을 붙이기 어려운 구조
- 화면에 보이지 않는 워크스페이스까지 계속 전체 렌더링

---

# PR 계획

## PR-01 — 현행 구현 감사 및 기준선 계측

### 목적

현재 구현 중 유지할 부분과 교체할 부분을 실제 코드와 계측 결과로 결정한다.

### 점검 대상

- 앱 시작 경로
- 워크스페이스 생성·삭제 경로
- PTY 생성 및 종료
- `alacritty_terminal::Term` 생성·해제
- eframe renderer 설정
- 터미널 셀을 egui에 전달하는 경로
- 폰트 및 텍스처 생성 위치
- 스크롤백 초기 용량
- 세션 로그 복원
- 클립보드 이미지 처리
- 자식 프로세스 실행

### 구현

- 단계별 RSS 로그 추가
- 자식 프로세스 트리 RSS 집계
- 워크스페이스 생성 시간 측정
- 첫 화면 표시 시간 측정
- 이미지 붙여넣기 구간별 시간 측정
- 프레임 시간과 repaint 원인 기록
- GPU 및 폰트 자원 생성 횟수 기록

### 산출물

`docs/terminal-current-state-audit.md`

각 영역을 `KEEP`, `IMPROVE`, `REPLACE`, `UNKNOWN`으로 분류한다.

### 완료 조건

- 워크스페이스 1·5·10개에서 기준값 확보
- 생성·삭제 20회 반복 후 메모리 추이 확보
- 이미지 붙여넣기 병목 구간 확인
- 다음 PR의 실제 적용 여부 결정 가능

---

## PR-02 — 생명주기 및 메모리 누적 수정

### 사전 판정

PR-01에서 누적이 없으면 불필요한 재작성은 하지 않는다.

### 점검

- PTY reader thread 종료 여부
- child process 종료·회수 여부
- channel sender/receiver 순환 참조
- `Arc` 순환 구조
- workspace 제거 후 callback·timer 잔존 여부
- egui texture handle 해제 여부
- 로그 버퍼와 이미지 임시 버퍼 해제 여부

### 구현

필요한 항목만 수정한다.

- 명시적 `Workspace::shutdown()`
- thread cancellation token
- child `kill/wait`
- callback에서 강한 `Arc` 대신 `Weak`
- bounded channel 적용
- workspace 제거 시 renderer buffer 해제
- 임시파일 수명 및 정리 정책 추가

### 완료 조건

- 워크스페이스 생성·삭제 100회 후 지속적인 RSS 우상향 없음
- 종료된 workspace의 thread/process가 남지 않음
- 기능 회귀 없음

---

## PR-03 — 공유 GPU·폰트 리소스 구조

### 사전 판정

이미 앱 전체에서 리소스를 공유하면 기존 구현을 유지하고 검증 테스트만 추가한다.

### 목표 구조

PR-03은 `3-A. 확정 목표 아키텍처`를 기준으로 구현한다.

특히 다음을 검증한다.

- GPU Device/Queue/FontSystem/SwashCache/Atlas/Pipeline은 앱 전체 공유
- Surface는 Window 계층 소유
- Workspace는 `Term`, dirty/selection/view 상태와 작은 buffer handle만 보유
- Workspace별 독립 대형 instance buffer보다 shared buffer pool slice를 우선 사용
- scrollback 중복 소유 금지

### 금지

- workspace마다 Device 생성
- workspace마다 FontSystem 생성
- workspace마다 대형 Glyph Atlas 생성
- 매 프레임 pipeline 생성

### 완료 조건

- GPU·폰트 핵심 자원이 프로세스당 1개
- workspace 추가 시 증가량이 주로 grid·scrollback·PTY에 한정
- workspace 전환 시 폰트 재로딩 없음

---

## PR-04 — eframe `wgpu` 전환

### 사전 판정

현재 Glow에서 이미 성능과 품질 목표를 만족하면 무조건 제거하지 않는다. 다만 macOS Metal이 필수 목표이므로 최종적으로는 `wgpu` 경로를 제공해야 한다.

### 구현

- eframe renderer를 `Wgpu`로 전환
- macOS에서 Metal backend 확인
- Windows D3D12, Linux Vulkan 확장을 막는 플랫폼 종속 코드 금지
- 기존 일반 UI 렌더링은 유지
- 기능 플래그 또는 런타임 fallback 유지

### 완료 조건

- macOS에서 Metal backend로 실행
- 창 resize, Retina, 멀티모니터, sleep/wake 정상
- 기존 UI 기능 회귀 없음
- Glow 기준선과 CPU/RSS 비교 결과 문서화

### 롤백

문제가 발생하면 기능 플래그로 Glow 경로를 재활성화할 수 있어야 한다.

---

## PR-05 — 터미널 전용 렌더 경로

### 사전 판정

현재 epaint 렌더러가 아래 조건을 만족하면 유지할 수 있다.

- 전체 셀을 매 프레임 재구성하지 않음
- 숨겨진 workspace를 렌더링하지 않음
- 대량 출력에서도 목표 프레임 시간 충족
- 글자 품질과 Retina 정렬 문제 없음

하나라도 구조적으로 충족하기 어렵다면 교체한다.

### 구현

- egui는 터미널 영역의 사각형만 할당
- custom wgpu paint callback에서 터미널 렌더링
- `alacritty_terminal` grid를 렌더 snapshot으로 변환
- 배경, 글리프, 장식, 선택, 커서를 레이어별 렌더
- pipeline과 buffer 재사용

### 완료 조건

- 터미널 셀을 egui TextShape로 대량 생성하지 않음
- 대량 출력 중 UI 조작이 멈추지 않음
- 기존 ANSI 출력과 색상 표현 회귀 없음

---

## PR-06 — Glyph 렌더러 및 텍스트 품질

### 사전 판정

기존 폰트 렌더링이 품질·성능 목표를 만족하면 단순히 glyphon을 도입하지 않는다.

### 구현 후보

- 1차: `glyphon/cosmic-text`
- 필요 시 macOS 폰트 품질 개선을 위한 CoreText 래스터 경로 별도 검토

### 요구사항

- monospace 셀 너비 안정성
- 한글·CJK wide character
- combining character
- emoji fallback
- bold, italic, underline, strikeout
- Retina scale 변경 시 atlas 갱신
- font fallback 캐시 공유
- atlas 크기 제한과 회수 정책

### 완료 조건

- 한글 입력·출력 셀 어긋남 없음
- 폰트 크기 변경 시 메모리 무한 증가 없음
- 여러 workspace가 동일 atlas와 font cache 공유
- Ghostty/Termius와의 시각 비교 캡처 문서화

---

## PR-07 — Dirty 렌더링 및 비활성 Workspace 정책

### 사전 판정

현재 dirty row/cell 처리와 비활성 workspace throttling이 이미 구현되어 있으면 검증 후 유지한다.

### 구현

- terminal mutation 시 dirty row 또는 full-dirty 표시
- 활성 workspace만 즉시 snapshot 갱신
- 비활성 workspace는 GPU 업데이트 지연
- 재활성화 시 1회 전체 rebuild
- cursor blink와 일반 출력 repaint 분리
- 이벤트가 없을 때 지속 repaint 금지

### 완료 조건

- idle 상태에서 불필요한 지속 렌더 없음
- 비활성 workspace 수 증가가 GPU draw call에 비례하지 않음
- workspace 전환 시 화면 누락 없음

---

## PR-08 — 마우스 선택 기능

### 기존 구현 점검

- pointer 좌표를 terminal cell로 변환하는 코드
- selection lifecycle
- 선택 highlight
- 선택 텍스트 추출
- terminal mouse mode 처리

정상 구현되어 있으면 누락 기능만 추가한다.

### 구현 범위

- 드래그 선택
- double click 단어 선택
- triple click 줄 선택
- drag edge auto-scroll
- Shift override
- 화면 밖으로 포인터가 나갈 때 capture 유지
- scrollback 좌표 반영
- 선택 후 Cmd+C
- 선택 해제 정책

### 완료 조건

- 일반 쉘, tmux, Vim/Neovim, Codex TUI에서 정책대로 동작
- Retina 및 패널 offset 환경에서 좌표 오차 없음
- 드래그 중 60fps 목표
- 선택 텍스트가 실제 화면 내용과 일치

---

## PR-09 — 이미지 클립보드 비동기 파이프라인

### 기존 구현 점검

각 구간을 측정한다.

```text
clipboard read
→ image decode/copy
→ encode
→ temp write
→ app/agent dispatch
→ terminal repaint
```

빠른 구간은 유지하고 병목 구간만 교체한다.

### 우선순위

1. 클립보드 파일 URL 또는 원본 파일 경로 사용
2. 원본 encoded data 재사용
3. 필요할 때만 PNG 인코딩
4. RGBA 변환 최소화
5. Base64 사용 금지 또는 최후 수단
6. UI 스레드에서는 요청 등록과 결과 반영만 수행

### 구현

- bounded background worker
- 취소 가능한 paste job
- 중복 이미지 hash/cache
- 임시파일 atomic write
- 크기 제한과 오류 표시
- worker 종료 및 임시파일 정리

### 완료 조건

- 큰 이미지 처리 중 UI freeze 없음
- 일반적인 화면 캡처 붙여넣기 체감 대기 최소화
- 동일 이미지를 여러 번 붙여도 중복 변환 최소화
- workspace 종료 시 작업과 버퍼가 남지 않음

---

## PR-10 — 스크롤백 및 Workspace 생성 버스트 최적화

### 기존 구현 점검

- history 크기
- `Vec::with_capacity` 등 대형 선할당
- 세션 전체 로그 복원
- workspace 생성 시 texture/font 재생성
- Codex 등 자식 프로세스 시작 RSS

### 구현

필요한 항목만 적용한다.

- 합리적인 기본 scrollback 제한
- 로그 최근 구간만 즉시 복원
- 과거 로그 lazy load
- hidden workspace renderer lazy init
- 에이전트 및 보조 프로세스 단계적 시작
- 큰 초기 allocation 분할
- 워크스페이스 미리보기 이미지 지연 로딩

### 완료 조건

- workspace 생성 peak RSS가 기준선보다 감소
- 첫 화면 표시 시간이 악화되지 않음
- 전체 로그 접근 기능 유지
- 10개 workspace 생성 시 메모리 증가량 문서화

---

## PR-11 — 회귀·성능 테스트 자동화

### 테스트 시나리오

- 일반 shell
- 대량 로그 출력
- ANSI color·cursor movement
- alternate screen
- tmux
- Vim/Neovim
- Codex TUI
- 한글 IME
- emoji/CJK
- 선택·복사
- 이미지 붙여넣기
- workspace 생성·삭제 반복
- sleep/wake
- monitor scale 변경

### 측정 항목

- 앱 RSS
- child process tree RSS
- workspace당 증가 메모리
- workspace 생성 peak
- idle CPU
- 대량 출력 CPU
- frame p50/p95/p99
- clipboard 단계별 latency
- thread/process 잔존 수
- atlas 및 GPU buffer 크기
- steady-state render loop heap allocation 횟수
- workspace별 GPU resource 생성 횟수
- shared resource 참조 수와 해제 여부

### 완료 조건

- CI 또는 반복 가능한 로컬 명령 제공
- PR 전후 비교 결과 저장
- 성능 회귀 임계값 정의

---

## PR-12 — 정리 및 기본 경로 전환

### 조건

PR-01부터 PR-11까지의 결과로 새 경로가 기존 경로보다 안정적일 때만 진행한다.

### 구현

- Metal/wgpu 경로를 macOS 기본값으로 전환
- 불필요한 epaint 터미널 경로 제거
- Glow fallback 유지 여부 결정
- 중복 `vte` 의존성 제거 여부 결정
- 사용하지 않는 texture·font·clipboard 코드 제거
- 운영 문서와 장애 대응 절차 작성

### 완료 조건

- 기능·성능·메모리 기준 통과
- 기존 데이터와 설정 호환
- 롤백 절차 검증
- 제거 대상 코드가 실제로 미사용임을 확인

---

## 5. 권장 PR 실행 순서

```text
PR-01 현행 감사
  ↓
PR-02 생명주기/누수
  ↓
PR-10 생성 버스트
  ↓
PR-03 공유 리소스
  ↓
PR-04 wgpu/Metal
  ↓
PR-05 전용 렌더 경로
  ↓
PR-06 Glyph 품질
  ↓
PR-07 Dirty 렌더
  ↓
PR-08 Selection
  ↓
PR-09 Clipboard
  ↓
PR-11 회귀·성능 테스트
  ↓
PR-12 기본 경로 전환 및 정리
```

PR-08과 PR-09는 PR-05 이후 병렬 진행 가능하다.

---

## 6. 핵심 설계 원칙

1. 기존 구현이 목표를 충족하면 재작성하지 않는다.
2. PTY와 `alacritty_terminal`은 명확한 결함이 없는 한 유지한다.
3. 일반 UI는 egui를 유지하고 터미널만 전용 렌더 경로로 분리한다.
4. GPU와 폰트 자원은 앱 전체에서 공유한다.
5. UI 스레드에서 이미지 변환·파일 I/O·대형 로그 복원을 하지 않는다.
6. 숨겨진 workspace는 렌더링하지 않는다.
7. 메모리 문제는 앱과 자식 프로세스를 분리해서 측정한다.
8. RSS가 내려가지 않는 것만으로 누수라고 단정하지 않는다.
9. workspace 생성·삭제 반복 시 지속 증가하는 메모리를 누수 후보로 본다.
10. 모든 PR은 기능 테스트와 성능 전후 수치를 함께 제출한다.
11. `Surface`는 Window 계층에서 관리하고 Workspace에 넣지 않는다.
12. Scrollback은 `Term`과 별도 메모리 버퍼로 중복 보관하지 않는다.
13. Zero Allocation은 초기화가 아닌 steady-state render loop 기준으로 검증한다.
14. Glyph Atlas는 공유하되 무제한 성장하지 않도록 eviction과 상한을 둔다.
15. Workspace별 GPU buffer는 가능하면 shared pool의 slice/handle로 관리한다.

---

## 7. 제안 성능 기준

아래 수치는 초기 목표이며 PR-01 측정 후 현실적인 기준으로 조정한다.

| 항목 | 목표 |
|---|---:|
| idle CPU | 활성 workspace 1개 기준 2% 이하 |
| 숨겨진 workspace GPU draw | 0 |
| 드래그 선택 | 60fps 유지 |
| 키 입력 반응 | p95 16ms 이하 |
| 일반 이미지 붙여넣기 UI block | 16ms 이하 |
| workspace 생성 첫 화면 | 300ms 이내 목표 |
| 생성·삭제 100회 | 지속적인 RSS 우상향 없음 |
| GPU Device/FontSystem/Atlas | 앱당 각 1개 |
| workspace 10개 idle | CPU 증가 최소화 |
| 앱 종료 | child/thread 잔존 없음 |

---

## 8. 각 PR 작성 템플릿

```md
# PR-XX 제목

## 목적

## 기존 구현 점검
- 확인한 파일:
- 확인한 구조:
- 측정 결과:
- 판정: KEEP / IMPROVE / REPLACE

## 변경 범위

## 변경하지 않는 범위

## 구현 상세

## 메모리·스레드 생명주기

## 테스트

## 성능 전후

## 위험 요소

## 롤백 방법

## 완료 조건
```

이 템플릿에서 `기존 구현 점검`과 판정 근거가 없는 PR은 시작하지 않는다.
