# 테스트 병렬성과 pty 안정화

작성·측정 2026-08-04. `cargo test`가 로컬에서 매번 다른 테스트로 실패하는 문제의
조사 기록과 남은 작업.

## 0. 결론부터

**pty는 타이밍 문제가 아니다.** 한 테스트가 프로세스 전역 환경변수를 조작하는 동안
다른 테스트가 자식 프로세스를 띄워서 그 값을 상속받는, **테스트 간 상호 간섭**이다.

이 리포는 이미 `--test-threads=1`을 전제로 설계됐고(테스트 안에 그 주석이 있다),
`.github/workflows/build-test.yml`이 그렇게 돌린다. 그 조건에서는 전부 통과한다.
**남은 결함은 로컬에서 그냥 `cargo test`를 치면 그 전제가 적용되지 않는다는 것뿐이다.**

## 1. 측정 (전부 이 머신에서 실측)

| 조건 | 결과 |
|---|---|
| `cargo test -p pty` (기본 병렬) | 12회 중 **11개 서로 다른 테스트**가 실패 |
| `cargo test -p pty -- --test-threads=1` | **10/10 통과** |
| `RUST_TEST_THREADS=1 cargo test -p pty` | **8/8 통과** |
| `RUST_TEST_THREADS=1 cargo test --workspace` | **3/3 통과** |
| `cargo test --workspace --locked -- --test-threads=1` (CI와 동일) | **3/3 통과** |

pty 전체 소요는 0.62초다. 느려서 타임아웃이 나는 게 아니다.

## 2. 원인 — `crates/pty/src/lib.rs:1784`

```rust
#[test]
fn 부모_에이전트_세션_마커는_pane에_상속되지_않는다() {
    // 프로세스 전역 env를 건드린다 — 이 리포는 `--test-threads=1`로 돌린다.
    for key in INHERITED_AGENT_SESSION_VARS {
        unsafe { std::env::set_var(key, "leaked") };
    }
    …
    for key in INHERITED_AGENT_SESSION_VARS {
        unsafe { std::env::remove_var(key) };
    }
}
```

`std::env::set_var`는 프로세스 전역이다(그래서 Rust 1.80+에서 `unsafe`다). 이 테스트가
값을 세팅한 구간 동안 **병렬로 도는 다른 테스트가 띄우는 자식 프로세스가 전부 그 값을
상속한다.** 그래서 자식의 환경을 단정하는 테스트들이 무너진다:

- `embedded_pty는_no_color를_제거하고_truecolor_capability를_고정한다`
- `embedded_pty는_malloc_stack_logging을_제거한다`
- `finder처럼_lang가_비면_pty는_utf8_locale을_주입한다`

환경을 단정하지 않는 테스트(`출력과_종료코드`, `입력_echo_roundtrip`,
`output_chunk가_채널에_들어오면_worker_wake를_호출한다`)까지 깨지는 것도 같은 뿌리로
설명된다 — `MALLOC_STACK_LOGGING=leaked`가 상속되면 자식이 느려지고 출력이 오염된다.

**증거로 굳힐 것**: 위 세 테스트만 골라 병렬로 돌리고 실패 출력의 실제 환경값을 찍어,
`leaked`가 나오는지 확인하라. 나오면 이 가설이 확정된다.

## 3. 오해하기 쉬운 것 두 가지

**`collect_output`이 타임아웃을 다 쓰는 게 아니다** (`lib.rs:2379`). `Err(Timeout) => continue`만
보면 5초를 항상 소모할 것 같지만, 프로세스가 끝나면 채널이 `Disconnected`가 되어
루프를 빠져나간다. 실측 0.62초가 그 증거다. 여기를 "최적화"하지 말라.

**Codex의 #66(`4c1df44`)이 만든 회귀가 아니다.** `crates/pty/src/lib.rs`를 #66 직전
버전으로 되돌려 8회 측정했더니 **동일하게 3/8 통과**였다. 원래 있던 부채다.

## 4. 권장 작업

### A. 로컬 기본값을 CI와 일치시킨다 (권장, 검증 완료)

`.cargo/config.toml`에 다음을 추가한다.

```toml
[env]
RUST_TEST_THREADS = "1"
```

`RUST_TEST_THREADS=1`만으로 pty 8/8, 워크스페이스 3/3 통과를 확인했다. 로컬에서
`cargo test`만 쳐도 CI와 같은 조건이 되어, "왜 나만 빨간가"가 사라진다.

**부작용을 반드시 확인할 것**: 전체 테스트 실행 시간이 얼마나 늘어나는지 측정하고,
받아들일 수 없으면 B로 간다. `[env]`는 `cargo run`을 포함한 모든 cargo 호출에 적용되므로,
런타임 동작에 영향이 없는지도 확인하라(테스트 하네스 전용 변수라 영향은 없어야 한다).

### B. 전역 env 테스트만 격리한다 (대안)

`부모_에이전트_세션_마커는_pane에_상속되지_않는다`를 별도 통합 테스트 바이너리
(`crates/pty/tests/inherited_env.rs`)로 옮긴다. 별도 프로세스가 되므로 다른 테스트의
환경을 오염시키지 않고, 나머지는 병렬로 빨리 돌릴 수 있다.

이때 `INHERITED_AGENT_SESSION_VARS`와 spawn 경로가 crate 외부에서 접근 가능해야 한다.
`pub(crate)`면 `pub`으로 올리거나 `#[doc(hidden)]` 테스트 훅이 필요하다 — **제품 API를
넓히는 대가**가 있으니 A를 먼저 검토하라.

### C. 하지 말 것

- **타임아웃을 늘리지 말 것.** 원인이 시간이 아니다. 늘리면 증상이 가끔 가려질 뿐
  전체 실행만 느려진다.
- **`set_var` 테스트를 지우지 말 것.** 이 테스트는 실제 사고를 잡는다 — deppy를 다른
  코딩 에이전트 안에서 실행하면 `CLAUDE_CODE_CHILD_SESSION`이 자식에 새어 들어가
  transcript 저장이 꺼지고, 그러면 에이전트 감지와 단축키가 통째로 죽는다(2026-08-02 실증).
- **뮤텍스로 줄 세우는 방식은 여기선 안 통한다.** `auth`/`clipboard_image`에서 쓴 수법인데
  (`f677328` 참조), env 오염은 잠금을 안 잡는 **다른 모든 테스트**에게도 퍼지므로
  전역 env에는 적용되지 않는다.

## 5. 검증 방법

수정 후 아래를 각각 10회 이상 돌려 실패 0을 확인한다. 1회 통과는 증거가 못 된다.

```sh
# 로컬 기본값 (A안 적용 후)
for i in $(seq 1 10); do cargo test --workspace 2>&1 | grep -cE "^test .*FAILED"; done

# CI와 동일 조건 (항상 통과해야 한다)
cargo test --workspace --locked -- --test-threads=1

# 병렬을 유지하기로 했다면 (B안)
for i in $(seq 1 10); do cargo test -p pty 2>&1 | grep -cE "^test .*FAILED"; done
```

머신 부하도 함께 기록하라(`uptime`). 이 조사 중 Orca가 8일째 CPU 1131%를 먹고 있어
로드 애버리지가 44였다. 죽여서 20으로 낮췄지만 **실패율은 그대로였다** — 부하는 이
문제의 원인이 아니라는 반증이다.

## 6. 이미 고친 같은 계통 (참고)

`f677328`에서 프로세스 전역 자원 경합 두 건을 뮤텍스 직렬화로 고쳤다. env가 아니라
**잠금으로 감쌀 수 있는** 자원이라 그 수법이 통했다.

- `crates/auth/src/callback.rs` — 해제한 포트를 재bind해 해제를 증명하는데, 그 틈에
  다른 테스트가 채가 `AddrInUse`. 고정 포트든 임의 포트든 발생한다(OS가 방금 반납한
  포트를 다음 `bind(0)`에 그대로 내준다).
- `crates/app/src/ui/clipboard_image.rs` — `ClipboardCacheAdmission`의 `CACHE_MUTEX`가
  전역 `try_lock`이라 즉시 `clipboard.cache.busy`. UI를 막지 않으려는 정상 제품
  동작이므로 제품이 아니라 테스트를 줄 세웠다.
