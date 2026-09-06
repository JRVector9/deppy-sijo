# 기존 출력의 리사이즈 레이아웃 — 조사 상태

- 사용자 요청: 앞선 작업을 마친 뒤, 창 크기를 바꿔도 기존 글의 형태를 유지하도록 별도 PR.
- 앞선 PR: Relay/i18n #146, 한글 IME #147, 깜빡임 #148. 후속 조사는 #147/#148 생성 후 시작했다.
- 작업 트리: `/Users/jr/Desktop/projects/deppy-sijo-layout-20260906`.
- 브랜치: `fix/preserve-terminal-layout`, 기반 `50d195f`(PR #148).
- 현재 프로덕션 변경·커밋·PR 없음. 앱 재실행 없이 backend 검사만 했다.

## 확인된 동작

창 너비가 PTY와 Alacritty 그리드의 열 수로 함께 전달된다. 기본 화면은 기존 출력을 새
너비로 다시 줄바꿈한다. 따라서 좁은 창에서도 표의 원래 행 모양을 고정하려면 현재와 다른
표시 방식이 필요하다. 단순히 `Grid::resize(false)`를 호출하면 오른쪽 열이 버려지므로
보존 기능으로 쓸 수 없다. alt 화면은 자식 TUI가 새 크기로 다시 그리는 계약을 사용한다.

- `crates/app/src/ui/workspace.rs`: `stage_terminal_resize_for_pass`.
- `crates/session/src/session.rs`: `resize`가 backend와 PTY에 같은 크기를 전달.
- `crates/terminal/src/alacritty_backend.rs`: `resize` → `Term::resize`.
- `third_party/alacritty_terminal-0.26.0/src/grid/resize.rs`: 기본 화면 reflow와 행수 상한.

## 실제 검사

원본 `AlacrittyBackend`에 ASCII·한글 상자표, 여러 내부 공백, 전경·배경색을 넣고
80→10/11/20→80을 각 3회 반복했다. 스크롤백 1000, 화면 40행으로 상한 절단을 피했다.
논리 줄과 화면 행 배열을 각각 비교하고, 표 경계 열·한글 2셀·색·내부 공백도 확인했다.
좁힐 때 행 수가 실제 늘고, 변경된 내용은 오라클이 구분하는 대조 검사도 포함했다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-layout-20260906
/tmp/deppy-sijo-agents-20260906/cargo-serial test -p terminal --lib --locked -- --test-threads 1 --nocapture
```

검사 당시 임시 모듈을 추가했다. 실제 해당 worktree의 vendor 및 terminal을 재컴파일했고
**89 passed / 4 ignored**(신규 프로브 6개 + 기존 83개), 0.23초였다.

원래 폭으로 돌아왔을 때 손실은 재현되지 않았다. 초기 에이전트 보고서의 “후행 공백 trim이
표의 내부 공백을 잘라 정렬을 깨뜨린다”는 주장은 이 검사로 반박되어 철회했다.
이 결과는 좁아진 상태에서도 원래 표 모양을 유지한다는 뜻이 아니다.

## 검사하지 않은 범위

- 이미 꽉 찬 스크롤백: 좁히면서 행수 상한을 넘으면 오래된 행이 절단되는 경로는 존재한다.
- 리사이즈 중 새 출력이나 셸의 프롬프트 재그리기, active TUI, 폭이 달라진 로그 복원.
- 실제 앱에서 사용자가 보고한 화면. 사용자 지시로 재실행과 시각 검증을 대기한다.

무제한 history 확장이나 전역 고정 폭 전환은 적용하지 않았다. 전자는 메모리 예산을,
후자는 새 입력과 active TUI의 동작까지 바꾼다.

## 다음 단계

사용자에게 다음 중 기대 동작을 질문했고 응답을 기다린다.

1. 원래 줄바꿈과 표 모양을 유지하고, 넘치는 부분은 가로로 이동해서 본다.
2. 새 창 너비로 줄바꿈하되 누락·겹침·순서 깨짐을 방지한다.

2번이라면 깨지는 화면/출력과 발생 과정을 먼저 특정해야 한다. 현재 왕복 프로브를
실패하도록 만드는 재현 없이 정상 reflow를 임의 변경하지 않는다.

임시 프로브는 작업 트리에서 제거했고 다음 경로에 남겼다.

```sh
cat /tmp/deppy-sijo-agents-20260906/layout-roundtrip-status.md
cat /tmp/deppy-sijo-agents-20260906/layout-roundtrip.log
# 다시 실행할 필요가 있을 때만 임시 모듈을 복원한다.
cp /tmp/deppy-sijo-agents-20260906/layout_roundtrip_probe.rs.kept crates/terminal/src/layout_roundtrip_probe.rs
# crates/terminal/src/lib.rs에 #[cfg(test)] mod layout_roundtrip_probe;를 추가 후 위 --lib 검사.
```

검사용 Cargo 래퍼는 공유 target 잠금을 얻은 뒤 현재 worktree의 변경된 소스와 crate 진입
파일 mtime을 갱신한다. 공유 캐시가 다른 worktree의 더 최신 테스트 바이너리를 재사용한
사고를 막기 위한 것이다. 래퍼가 사라졌다면 별도 target을 사용한다.
