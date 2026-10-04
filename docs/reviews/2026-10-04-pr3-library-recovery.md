# PR3 — 프롬프트 라이브러리 복구·제한 읽기·비동기 저장

Baseline: `85631a845d733b340e281df976522be4d4468959`.
Branch: `fix/audit-pr3-library-recovery-20261004`.
Source: `/private/tmp/deppy-audit-pr3-20261004`.
Scope: approved audit F3/F6, with the palette read-only/rejected-edit guard required by review.
No App launch, restart, stop, user PTY input, real user configuration access, version change or push.

## 완료 체크리스트

- [x] 실제 App 초기화 정책을 테스트 가능한 `load_startup`으로 옮기고 정상 빈 목록/손상 원본 덮어쓰기를 RED로 확인.
- [x] `Missing`, `Loaded`(빈 목록 포함), `Failed`를 구분하고 **Missing만** 기본 예제로 시작.
- [x] 손상·읽기 실패·초과 파일은 원본을 보존하고 프로세스 내 자동 저장/변경을 읽기 전용으로 차단.
- [x] 한 regular-file handle을 제한 읽기; Unix symlink/FIFO 등 비정규 파일을 거부. 파일 크기 메타데이터를 읽기 제한 대신 믿지 않음.
- [x] 파일16MiB, 데이터8MiB,1,024개,본문1MiB,제목4KiB,id256B,태그32개×256B 상한. 거절된 편집은 이전 라이브러리와 입력 폼 본문 보존.
- [x] worker가 bounded64KiB buffer로 직렬화·file sync·동일 디렉터리 atomic rename. 고유 create_new 임시 파일/Unix0600, 실패 시 임시 파일 정리.
- [x] 첫 seed의 최종 commit은 atomic no-clobber hard_link로 다른 프로세스가 방금 생성한 실제 파일을 덮어쓰지 않음.
- [x] 읽은 파일 내용 version을 worker가 저장 전/최종 교체 전에 재확인; 보통의 외부 변경은 `Conflict`로 원본 보존·읽기 전용 전환.
- [x] App render는 revision/dirty intent만 변경. `logic`에서 bounded snapshot과 worker admission; 원래 UI 동기 파일 저장 제거.
- [x] worker 생성은 inert, 첫 저장 시 한 스레드 시작. 한 active snapshot+한 latest pending만 보유하고 이전 pending 즉시 해제.
- [x] 단일 writer/엄격히 증가하는 revision. 이전 결과가 최신 저장 상태를 덮어쓰지 않음.64개 편집의 결정적 fixture에서 commit은 `[1,64]`.
- [x] 저장 완료 repaint wake; UI busy polling/즉시 재시도 없음. explicit retry는 새 revision으로 시작.
- [x] 정상 종료 시 마지막 render dirty intent를 admission하고 마지막 accepted snapshot을 drain/join. worker Drop도 drain.
- [x] 저장 중/실패/원본 복구 필요/외부 변경·RAM 미저장 상태를 인라인 표시. 경로·백업/복구 지침 tooltip, 정상 저장 오류는 명시적인 Retry.
- [x] 손상/외부 충돌 상태에서는 palette New/Edit/Delete/Save 비활성. App과 worker도 action/admission에서 재검사. 읽기/컴포저 삽입은 유지.
- [x] 모든5개 locale 키 반영; 새 popup/launcher 디자인 변경 없음.
- [x] PR3 함수·파일·worker·네이티브 없이 실행한 egui UI tests를 exclusive Cargo gate로 실행.

## 후속 PR4 계약

- `PromptLibrary::load(path) -> PromptLibraryLoad::{Missing, Loaded(PromptLibrary), Failed(PromptLibraryError)}`.
- `PromptLibrary::load_startup(path) -> PromptLibraryStartup { library, seed_missing, file_version, error }`.
- `validate`, `validate_upsert(&Prompt)`, `try_upsert(Prompt)`, `retained_bytes`; App은 borrow validation 후 mutate해 거절 입력을 돌려준다.
- App의 `prompt_library_revision: u64`는 실제 accepted upsert/delete에만 증가. 동일 값 upsert/없는 id delete/거절 mutation/explicit retry는 증가하지 않음. 별도 `prompt_library_save_revision`이 admission/retry의 엄격한 단조 증가를 담당.
- `PromptLibrarySaveWorker::new(path, recovery_error, file_version, wake)`, `request(revision, Arc<PromptLibrary>)`, `status`, `path`, `shutdown`.
- `PromptLibrarySaveStatus::write_blocking_error`는 load-recovery/외부충돌의 fail-closed 정책을 공용화.
- Palette 기존 render signature 유지. `set_read_only(bool)`와 `restore_rejected_prompt(Prompt)`는 PR4에서도 보존해야 함.
- Snapshot 텍스트는 각8MiB 이하. worker 장기 보존은 active+pending16MiB 이하. App 본문/추가 clone이 겹치는 순간 최대32MiB 텍스트+bounded vector/string metadata, 파일 검증 임시 읽기·allocator 여유는 별도. 프로세스 RSS 측정치가 아님.

## 실제 검증

### RED

`CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo pr3_startup -- --test-threads=1 --nocapture`

- 실제 temporary file2개 assertion **실패**: 정상 빈 목록이 seed로 바뀜, 손상 원본 bytes가 seed JSON으로 덮어써짐.
- `/tmp/deppy-audit-pr3-red-20261004.log`.

### GREEN (독점 gate)

`python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo --bin deppy-sijo pr3_ -- --test-threads=1 --nocapture`

- **20 passed,0 failed,0 ignored,2,559 filtered,0.43s**.
- `/tmp/deppy-audit-pr3-gated-final-20261004.log`.
- 정상 빈/손상/없는 파일, 크기·본문·개수·총량·serialized escape 상한, 읽기 실패/symlink, 외부 변경/최종 seed 경쟁, readonly directory 실제 쓰기 오류/재시도, worker64개 coalescing/Arc 회수/최신 pending status/종료 drain, known conflict/read-only, UI 오류 표기/명시적 retry/readonly save·거절 폼 본문 보존.

추가 gate 명령:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo --bin deppy-sijo pr3_ -- --list
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p i18n
rustfmt --edition 2024 --check crates/app/src/app.rs crates/app/src/main.rs crates/app/src/prompt_library.rs crates/app/src/prompt_library_worker.rs crates/app/src/ui/prompt_palette.rs
git diff --check
```

### 최종 fresh-source gate (최종 근거)

Root는 source worktree가 바뀌면 workspace package artifacts를 clean하고 **한 번의 lock 아래 test→named list→i18n**을 실행하도록 gate를 수정했다. 외부 dependency artifacts는 재사용했다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr3_","--","--test-threads=1","--nocapture"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr3_","--","--list"],["test","--offline","--locked","-q","-p","i18n"]]'
```

- PR3 **20 passed,0 failed,0 ignored,2,559 filtered,0.43s**.
- Named list는 **20개 PR3 이름**(App feedback/Palette guard/library/worker)을 확인. 다른 worktree 소스가 아님을 함께 검사.
- i18n **8 passed,0 failed,0.02s**,doc tests0.
- 최종 batch warnings0. `/tmp/deppy-audit-pr3-fresh-gated-final-20261004.log`.
- `rustfmt --edition 2024 --check ...` 및 `git diff --check` 실제 exit0.
- 초반 독점 gate만 적용한 named list/전체 App/i18n cache-contended 결과는 최종 근거에서 제외. Root 최종 full integrated gates는 별도 진행.


## 실패 접근과 검증 보정

- 첫 GREEN compile에서 실제 App constructor 이름이 `egui_ctx`인데 `cc.egui_ctx`로 사용한 오류; 수정 후 통과. 이 compile 오류는 RED 회귀 증거로 세지 않음.
- 중간 tooltip 추가가 formatter 형태와 맞지 않아 unused variable warning; 실제 reusable paint helper로 연결하고 최종 gated tests에는 경고 없음.
- 여러 worktree가 같은 Cargo target을 공유할 때 별도 full App 실행이 예상2,578개 대신2,567개 테스트를 실행했다. 해당 전체 통과 결과는 **PR3 검증으로 폐기**. 최초 full 통과 수치 역시 최종 근거로 사용하지 않는다.
- Root의 최초 filesystem lock만으로는 source worktree fingerprint 재사용이 남았다. 해당 named list에는 PR3 소스에 없는 PR1 `PromptReceipt` warning이 나왔고 PR3 이름이0개였다. 이 결과도 폐기했다. 이후 source-switch package clean+batch lock으로 최종20개와 명단/i18n을 fresh 확인했다. 전체 통합 App/runtime/MCP gates는 root가 최종 합친 코드에서 fresh 실행한다.

## 보장 범위와 남은 통합 gate

- 실제 네이티브 App·장시간 RSS·disk-stall frame 측정은 하지 않았다. 화면 피드백은 native-free egui harness로 검증했다.
- 기존 파일 교체의 내용 확인→rename은 관련 없는 외부 편집기에 대해 filesystem atomic CAS가 아니다. 마지막 확인 직후 rename과 정확히 겹친 외부 writer의 수정은 원자적으로 방어한다고 주장하지 않는다. 첫 seed는 hard_link의 atomic no-clobber를 사용한다.
- 파일 sync/atomic 교체는 수행하지만 부모 디렉터리 fsync나 강제 종료/OOM/power-loss까지 포함한 durability를 주장하지 않는다. 정상 shutdown은 최신 accepted snapshot을 drain한다.
- 정상 쓰기 실패/뒤늦은 외부 충돌에 대한 이미 accepted RAM 편집은 저장 성공으로 표기하지 않는다. worker 실패/원본 보호 및 RAM 미저장 안내를 유지하며 사용자는 현재 변경을 백업하고 원본을 복구해야 한다.
- Root: PR3 commit source diff review, PR1+PR3+PR6 sequential integration, PR4 cache가 readonly/restore/revision 계약 유지하는지 확인, 최종 전체 tests/format/clippy/version/release artifact 검증. 앱 재실행은 금지.
