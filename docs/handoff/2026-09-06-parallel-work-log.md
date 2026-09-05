# 2026-09-06 병렬 작업 검증·실패 기록

## 진행 중 — Relay 완성·미사용 i18n 정리·리사이즈 깜빡임·빠른 한글 입력 (2026-09-06)

- 현재 목표: PR #146의 미완성 Relay 개발을 이어서 완료하고 미사용 fleet 키를 삭제한다.
  창 리사이즈 깜빡임과 빠른 한글 입력 자모 분리를 각각 분리된 worktree에서 수정한다.
  이 작업들이 종료된 뒤, 이미 작성된 글의 리사이즈 시 레이아웃 깨짐을 **별도 PR**로 작업한다.
- 최신 사용자 지시: **작업 완료 후 바로 재빌드·재실행하지 말고 대기**한다. 코드/검증/PR까지
  진행하고 사용 중인 앱을 종료하지 않는다. 네이티브 화면 및 빠른 실제 한글 타이핑은 미확인으로 기록.
- 확인한 저장소: `feat/fleet-one-list-and-relay-wip`, HEAD `e960004` (인계 문서 추가),
  구현 커밋 `53f2a31`. 시작 시 작업 트리 깨끗함. PR #146 OPEN, 머지 승인 없음.
- 사용자 화면 확인: 같은 세션 중복 없음 / 세션 0 + 승인 1일 때 펼친 카드와 빈 안내 동시 표시 /
  좁은 창에서 카드 표시 확인. 막힌 항목 하나만 펼침은 실제 막힌 항목이 없어 확인 대기.
  `waiting_ui.render` 매 프레임 호출 및 세션 0 + 승인 카드 계약과 회귀 테스트를 보존한다.
- 완료한 변경: 로케일 5개의 `fleet.hero.now/next/clear/sessions/skip` 25줄 삭제.
  `fleet.rs`의 버튼 부재 테스트는 삭제된 키를 조회하지 않고 테스트 로케일의 예전 문구 `Skip`을 검사한다.
  사용 중인 `fleet.hero.approval/needs_input` 및 `fleet.blocked_for` 유지.
- 병렬 에이전트: Claude CLI `--model opus --effort high`, 총 3개.
  Relay: `/Users/jr/Desktop/projects/deppy-sijo-relay-20260906`, `feat/relay-completion-20260906`.
  깜빡임: `/Users/jr/Desktop/projects/deppy-sijo-resize-20260906`, `fix/resize-flicker-20260906`.
  IME: `/Users/jr/Desktop/projects/deppy-sijo-ime-20260906`, `fix/korean-ime-20260906`.
  상태/로그/프롬프트는 `/tmp/deppy-sijo-agents-20260906/` 아래 각 이름으로 저장.
- 설계 결정: 오래된 STOP HANDOFF를 그대로 재실행하지 않고 이후 Task 2/3/4 완료 기록을
  실제 코드와 대조한다. 기존 production Relay 설계와 Tailscale 독립성 유지.
  cmux 실제 소스와 현재 winit/egui/터미널의 조합 입력 처리를 비교한다.
- 리소스: `sysctl vm.swapusage` 결과 35,840MB 중 34,779MB 사용. Cargo 실행은
  `/tmp/deppy-sijo-agents-20260906/cargo-serial`의 파일 잠금과 `CARGO_BUILD_JOBS=1`로 직렬화.
  사용자가 보고 있는 앱 PID 16348은 시작 시 실행 중. 앱 재빌드/재실행/종료 승인 아직 없음.
- 테스트: `cargo fmt --all -- --check`, `git diff --check` 통과.
  직렬 래퍼로 `cargo run -p xtask -- i18n-check` 실행 중: i18n 8개 통과, 앱 관련 테스트
  컴파일 중이므로 전체 i18n 게이트는 아직 완료 아님. 과거 2103 통과 기록은 새 변경 검증이 아니다.
  화면 밖 로직은 TDD, UI는 승인된 재빌드 후 사용자 화면 확인 우선.
- 추가 Relay 결함: 루트가 동시 WebCrypto 송신 순번 `[0,0]`과 동일 암호문 두 번 복호화
  성공을 재현. 별도 `/Users/jr/Desktop/projects/deppy-sijo-relay-channel-20260906`의
  `fix/relay-channel-concurrency-20260906`에서 방향별 bounded queue와 닫힘 후 결과 폐기를
  구현했다. `node --test web/relay-shell/tests/channel-concurrency.test.mjs` RED 6개 실패 →
  GREEN 6개 통과. 브라우저 검증 및 Relay 브랜치 통합은 남아 있다.
- 테스트 환경 진단 갱신: auth 컴파일러 PID 63630이 0% CPU로 수 분 정체. `sample`에서
  macro dylib의 `dlopen → dyld mapSegments → fcntl` 대기를 확인했다. `lsof`의 열린 파일은
  `target/debug/deps/libyoke_derive-8392f9c220f61c7e.dylib`. 원본 `codesign --verify`도
  정체하지만 `cp -X`로 만든 같은 SHA-256 사본은 즉시 정상 서명으로 검증됐다.
  원본을 `/tmp/deppy-sijo-agents-20260906/yoke-derive-original.dylib`에 백업하고 동일 바이트/
  mtime의 새 파일로 교체했다. PID 63630 및 이번 정체된 codesign 진단 2개만 SIGTERM.
  i18n 게이트는 이 의도된 종료로 exit 1 (내부 cargo 101), **통과 아님**. 재실행 필요.
  사용자 앱 PID 16348은 종료하지 않았다. 전체 스왑만으로 원인을 단정하지 말 것.
- 환경 대응: 원본 경로에서 교체한 dylib도 수 분 뒤에야 codesign 검사 완료.
  속성을 보존한 `cp -c` APFS 사본도 `/tmp`에서는 즉시 정상 검증됨(서명 우회 없음).
  `cp -cR target/debug /tmp/deppy-sijo-target-20260906/debug`로 테스트 캐시를 복제 중.
  `cp -cRX`는 macOS에서 옵션 병용 불가로 실패했고 `-cR`로 바로잡았다.
  기존 캐시 deps 77G / incremental 44G / build 280M. APFS clone도 디렉터리 메타데이터
  처리에 12분 이상 걸렸다. 기존 Cargo는 auth를 지나 계속 진전해 복제 PID 70894만 중지했다.
  `/tmp/deppy-sijo-target-20260906/debug`는 불완전 사본이므로 테스트 출력 경로로 사용하지 말 것.
  Cargo 직렬 래퍼는 기존 target을 계속 사용한다. 캐시 복제로 해결했다고 주장하지 않는다.
- Relay 배포 회귀 테스트는 외부 의존성이 없는 실제 `relay-protocol` 소스와 실제
  `crates/web-remote/tests/relay_shell_deploy.rs`를 `/tmp` 출력의 `rustc --test`로 컴파일해
  **10개 통과**, exit 0. Cargo 전체 게이트와 혼동하지 않는다.
- Relay 통합: 실제 배포 `relay-crypto.js`를 벡터 러너가 직접 import하도록 교체하고,
  방향별 유한 대기열로 동시 nonce 중복과 replay 성공을 막았다. Node 회귀 8개 및
  별도 Chrome 동시 송수신/replay 검증 PASS. Relay 에이전트의 독립 적대적 probe 5개,
  실제 Chrome 고정 벡터 PASS. 셸 staging/production 예제, CSP 헤더, 배포 워크플로와
  10개 매니페스트 테스트를 추가했다. DNS/TLS/CDN/자격증명이 없으므로 publish는 BLOCKED.
  루트 worktree로 코드 12파일 통합 완료. 새 Rust 테스트 10개 standalone PASS는 위 기록 참조.
  실제 전체 셸 페어링 Chrome 게이트 및 Cargo 최종 게이트는 아직 미완료.
- Relay 잔여 범위 정정: Task 2/3/4는 이후 기록과 코드상 구현되어 있다. Task 6 전체 완료
  또는 운영 준비 완료는 주장하지 않는다. `rememberDeviceId` 미배선 및 일회용 입장권만
  있는 서버 때문에 알려진 기기 재접속은 여전히 미완성이다. 영속 id 저장만으로 해결되지
  않으며 재접속 입장권/인증 프로토콜 설계를 함께 다뤄야 한다. Task 7 운영 검증 BLOCKED.
- 병렬 진행: 느린 창 드래그의 매 셀 즉시 Resize 전송과, 이번 프레임에 시작된 IME
  Preedit을 이전 프레임 preedit 값만으로 판단하는 포커스 복구 경로를 각각 재현 중.
  두 담당자는 아직 RED 검증 대기이며 수정 완료가 아니다.
- i18n 정적 게이트: 실제 `xtask/src/main.rs`를 `rustc --test`와 기존 의존성으로 컴파일.
  첫 실행은 `CARGO_MANIFEST_DIR` 누락으로 1개 실패; 실행 환경을 실제 `.../deppy-sijo/xtask`로
  설정한 재실행에서 `현재_i18n_key_coverage는_5개_로케일에_모두_있다` 1개 PASS.
  리터럴 1103건 / 동적 64건, 로케일 5개. 전체 i18n-check와는 별도 결과다.
  루트 최종 `cargo clippy --workspace --all-targets -- -D warnings`를 직렬 래퍼로 시작했다.
  로그 `/tmp/deppy-sijo-agents-20260906/root-clippy.log`, 완료 전 PASS 금지.
- 리사이즈 재현: 담당자의 실제 Cargo 테스트에서 느린 드래그 중 (81,24) 전송으로 RED 확인.
  첫 세션 크기만 즉시 보내고 이후 크기 변경을 안정될 때까지 미루는 수정을 적용했다.
  GREEN 및 presentation fence 보완 검토는 아직 진행 중.
- IME 대기 작업은 첫 CLI 종료 때 `[killed]`로 끝났으므로 RED가 실행되지 않았다.
  동일 Opus/high 세션을 재개해 실제 terminal 테스트를 /tmp rustc 출력으로 실행하도록 했다.
- 재빌드 제한 위반 확인: 담당자의 `cargo test -p deppy-sijo`는 통합 테스트 때문에
  `target/debug/deppy-sijo`도 자동으로 갱신했다(mtime 2026-09-06 08:18:40). 실행 앱은
  PID 16348, 시작 00:25:04 그대로이며 종료/재실행하지 않았다. 사용자에게 사실을 알렸다.
  향후 래퍼는 앱 `test -p deppy-sijo`에 `--bin deppy-sijo`를 붙이고 앱 `--test`를 거부한다.
  xtask의 중첩 cargo 앱 테스트도 자동 재빌드 가능하므로 원래 i18n-check 재실행은 하지 말고
  명시적 `--bin deppy-sijo` 필터 검사 + i18n 테스트 + 실제 정적 키 게이트로 나누어 실행한다.
  clippy는 check 모드라 앱 실행 파일을 링크하지 않는다.
- Relay 실제 페어링 종단: `relay_shell_chrome.rs`를 기존 rlib와 `rustc --test`로
  컴파일한 바이너리에서 실행. 최초 60초 무보고 timeout FAIL 후 동일 바이너리 3회 재실행
  모두 1개 PASS(1.20/1.14/1.14초). 실패 시 updater/부하 로그가 있었지만 원인으로 단정하지
  않는다. Python 임시 배선 + 실제 JS fixture도 PASS, 반환 서명 Node 독립 검증 true/변조 false.
  로그 `/tmp/deppy-sijo-agents-20260906/shellgate/run{,2,_r1,_r2}.log`. Cargo 게이트와 구분.
- 최종 루트 앱 단위 검증은 `cargo test -p deppy-sijo --bin deppy-sijo --locked`로
  직렬 대기열에 추가했다. `/tmp/deppy-sijo-agents-20260906/root-app-tests.log`.
  통합 테스트를 제외하므로 사용자 앱 실행 파일을 갱신하지 않는다. 완료 전 PASS 금지.
- 느린 드래그 1차 수정 GREEN: 담당 Cargo가 main 테스트 뒤 통합 테스트까지 갔지만
  tail 때문에 main 결과가 유실됐다. 루트가 그 실제 Cargo 테스트 바이너리
  `target/debug/deps/deppy_sijo-5815d54f4cd311c1`을 /tmp/resize-app-tests로 복제하여
  `ui::workspace::tests --test-threads=1` 실행: **229 PASS / 0 FAIL, 1.19초**.
  로그 `/tmp/deppy-sijo-agents-20260906/resize-direct-tests.log`. 중복 Cargo 대기를 취소하고
  담당 CLI를 재개해 창 Resize의 최종 clear/redraw를 숨기는 기존 bounded fence 보완 중.
  이 추가 변경은 위 229 PASS 이후이므로 별도 재검증 필요. 실제 app/src/main.rs를
  기존 의존성 rlib로 /tmp 출력에 컴파일하는 `/tmp/.../build-app-test.py`도 작성했다.
- **공유 target 캐시 함정 추가 발견**: 루트 `cargo test --bin deppy-sijo`가 컴파일 없이
  resize worktree의 바이너리를 재사용했다. 루트에 없는 `느린_창_드래그` 테스트가 로그에
  나타난 것이 증거. `root-app-tests.log`의 **2105 PASS / 14 ignored를 루트 검증으로 인정하지
  않는다**. 같은 캐시를 쓴 IME 앱 57 PASS도 새 구현의 증거가 아니다. 사용자에게 정정했다.
  래퍼가 잠금을 얻은 뒤 현재 worktree의 변경 Rust 파일 및 app/terminal/i18n 진입 소스의
  mtime을 갱신하여 해당 코드가 반드시 재컴파일되게 했다. 내용 변경이나 앱 링크는 하지 않는다.
  루트 재검증 `root-app-tests-fresh.log` 진행 중. standalone terminal 86 PASS는 실제 소스를
  직접 컴파일했으므로 이 캐시 오염과 무관하다.
- `cargo clippy --workspace --all-targets -- -D warnings`: 루트에서 실제 app/web-remote/i18n
  Checking 로그 후 exit 0 (8m47초). fmt / diff-check PASS. 앱 전체 테스트 최종 증거는 위 재검증
  완료가 필요하다. app standalone rustc 접근은 최신 rlib 선택이 의존성 버전/feature를 섞어
  3개 타입 오류로 실패했고, Cargo 재컴파일을 강제하는 쪽으로 전환했다.
- 실패한 접근: 전체 `docs/CODEX_HANDOFF.md` 읽기는 너무 커 출력이 잘림. 이후 `rg`와
  `sed`로 관련 절만 읽는다. 코드 변경 실패는 아직 없음.
- 남은 일: i18n 변경 검사와 커밋, 각 에이전트 결과 리뷰·통합·검증·PR 생성, 승인 후
  앱 빌드/재기동과 화면 확인, 이후 글 레이아웃 보존 별도 PR. rebase/force-push 금지.
  DNS/TLS/배포 자격증명 및 실기기/24시간 soak 없는 운영 검증은 **BLOCKED**, PASS 금지.
- 다음 에이전트 명령:

```sh
cd /Users/jr/Desktop/projects/deppy-sijo
cat AGENTS.md
cat CLAUDE.md
sed -n '1,85p' docs/CODEX_HANDOFF.md
git status --short --branch
git diff --stat
git worktree list
ls /tmp/deppy-sijo-agents-20260906
```


## 루트 최종 검증

- 캐시를 무효화하고 실제 루트 소스를 컴파일한 앱 단위 테스트: 2104 PASS / 14 ignored, exit 0.
- 실제 변경된 relay_webcrypto_vectors.rs를 rustc --test로 컴파일하고 Chrome 포함 5개 PASS, exit 0.
- workspace clippy -D warnings, fmt, diff --check PASS.
- 이 문서는 작업 중 실패·환경 대응 기록이다. 현재 상태는 CODEX_HANDOFF.md 맨 위가 권위다.
