# Vendored alacritty_terminal 0.26.0

## 왜 vendor했나

스크롤백 라인 압축(옵션 D)을 구현하기 위해 alacritty_terminal 0.26.0을 리포에
포함한다. alacritty의 `Storage`/`Grid` 내부(private 필드 `inner`/`zero`)에 손을
대야 스크롤아웃된 라인을 "텍스트 + run-length 속성 클러스터"로 압축해 **실제로 힙을
반납**할 수 있다. crates.io 공개 API만으로는 `Vec::truncate`/`split_off`가 capacity를
유지해 메모리를 회수할 수 없다(실측 확인: hidden 트림 후에도 phys_footprint 불변).

## 경계

- 압축 타입(`CompressedRow`)과 codec은 이 포크 안(`src/grid/`)에만 존재한다.
- `crates/terminal`은 alacritty 타입을 밖으로 노출하지 않는다(`crates/terminal/src/lib.rs`
  상단 규약). 포크의 영향 범위는 `crates/terminal` 안에 갇힌다.

## 언제 제거 가능한가

상류 alacritty_terminal에 구조적 스크롤백 압축(라인 단위 임의 접근 가능한)이 생기면
이 vendor를 제거하고 crates.io 의존으로 되돌릴 수 있다.

## 롤백

`Cargo.toml`의 `[patch.crates-io]` alacritty 라인과 `[workspace] exclude` 항목,
그리고 이 디렉터리를 지우면 즉시 crates.io 의존(무변경)으로 복귀한다.
브랜치 태그 `pre-scrollback-compression`이 D 시작 직전 상태다.

## 원본과의 diff

PR-1(vendoring) 시점: **코드 변경 0** (Cargo.toml 정규화 차이 제외).
이후 PR들이 `src/grid/{storage,mod,compressed}.rs`, `src/term/mod.rs`를 수정한다.
