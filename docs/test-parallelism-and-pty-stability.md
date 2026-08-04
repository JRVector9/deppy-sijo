# 테스트 병렬성과 pty 안정화

작성·측정 2026-08-04. `cargo test`가 로컬에서 매번 다른 테스트로 실패하는 문제의
조사 기록과 남은 작업.

> **2026-08-03 추기**: 첫 판본은 "env 상속 간섭"을 원인으로 결론 낸 상태로 커밋됐으나
> (d53b742), 후속 실험이 그 가설을 **반증**하고 진짜 원인 둘(macOS `openpty` 동시 호출
> 실패, 마지막 slave close 시 출력 큐 폐기)을 찾아 **둘 다 수정했다**. 이 판은 그
> 결과로 고쳐 쓴 것이고, 반증된 가설은 기록 가치가 있어 §2에 남겨 둔다.

## 0. 결론부터

**원인은 둘이었고 둘 다 고쳤다.**

1. macOS `openpty(3)`의 동시 호출 간헐 실패 — 병렬로 pty를 여러 개 할당하면 드물게
   `errno = -6`(`failed to openpty: Os { code: -6, kind: Uncategorized }`, portable-pty
   0.9.0 `src/unix.rs:46`에서 발생)이 나온다. 순수 C 프로브(pthread 12개 × 400회)로
   deppy/Rust/env 개입 없이 재현했고, 재시도 변형에서는 전부 일시적이었다(최대 5회
   재시도로 영구 실패 0). → **수정: spawn 경로 `openpty_with_retry` (최대 5회).**
2. macOS가 마지막 slave fd close 시 pty 출력 큐를 폐기 — spawn 직후 `drop(pair.slave)`
   하던 구조에서는 빠르게 종료되는 자식의 꼬리 출력이 reader보다 먼저 버려졌다
   (C 프로브: 즉시 close 50/50 유실, 유지 시 50/50 생존). → **수정: unix는 reader
   thread가 slave를 보유하고, 전담 reaper가 자식 종료를 감지해 reader가 drain 후
   닫는다.** 리스크 3의 즉시-drop 규칙은 Windows 전용으로 확인됐다.

둘 다 여러 pane이 동시에 뜨는 **제품 경로에도 닿는** 결함이었다. 처음 유력하게
의심된 "테스트 간 env 상속 간섭" 가설은 두 가지 대조 실험으로 반증됐다(§2 참조).
`set_var` 테스트는 무고하다.

이 리포는 이미 `--test-threads=1`을 전제로 설계됐고(테스트 안에 그 주석이 있다),
`.github/workflows/build-test.yml`이 그렇게 돌린다. 그 조건에서는 전부 통과한다.
두 수정으로 로컬 병렬 실행도 실패 0이 정상이 됐다(§5 참조).

## 1. 측정 (전부 이 머신에서 실측)

| 조건 | 결과 |
|---|---|
| `cargo test -p pty` (기본 병렬) | 12회 중 **11개 서로 다른 테스트**가 실패 |
| `cargo test -p pty -- --test-threads=1` | **10/10 통과** |
| `RUST_TEST_THREADS=1 cargo test -p pty` | **8/8 통과** |
| `RUST_TEST_THREADS=1 cargo test --workspace` | **3/3 통과** |
| `cargo test --workspace --locked -- --test-threads=1` (CI와 동일) | **3/3 통과** |
| `cargo test -p pty -- --skip 부모_에이전트 --nocapture` (env 테스트 제외, 병렬) | 15회 중 **6회 실패** (동일 실패 양상) |
| 프로브 테스트: 자식에서 9개 env + TERM/LANG 출력 덤프 | 40회 모두 **9개 전부 `absent`** — `leaked` 0건 |
| 순수 C 프로브 (pthread 12 × 400, deppy/Rust 무관) | 4800회 호출 중 첫 시도 실패 1~2건, `errno = -6` |
| C 프로브 + 최대 5회 재시도 | **영구 실패 0** — 전부 일시적 |

pty 전체 소요는 0.62초다. 느려서 타임아웃이 나는 게 아니다.

## 2. 원인 — macOS `openpty` 동시 호출 실패 (errno -6)

병렬 실행에서 드물게 pty 할당 자체가 실패한다:

```text
failed to openpty: Os { code: -6, kind: Uncategorized }
```

portable-pty 0.9.0의 `src/unix.rs:46`이 `openpty(3)` 호출 결과를 그대로 올려 본 것으로,
우리 코드가 아니라 macOS 커널의 pty 할당 경합에서 온다. 이걸 증명하기 위해 **deppy,
Rust, 환경변수가 전혀 개입하지 않는 순수 C 프로브**(pthread 12개가 각자 `openpty`를
400회 호출)를 돌렸고, 4800회 호출당 1~2건의 첫 시도 실패가 재현됐다. 같은 프로브에
최대 5회 재시도를 넣으면 **영구 실패 0** — 전부 일시적 실패다.

그래서 실제 제품 수준의 수정은 spawn 경로에서 pty 할당을 감싸는 재시도 루프다(§4 A').

### 부수 실패 양상 — 출력 단정 (원인 확정: slave 즉시 drop에 의한 drain 유실)

`openpty` 오류 없이도 출력 단정 테스트(`hello-pty`, `wake-output`)가 병렬 부하에서
드물게 실패한다. 원인은 순수 C 프로브로 확정했다:

- **macOS는 마지막 slave fd가 닫히면 pty 출력 큐를 폐기한다.** 자식이 slave에 쓰고
  즉시 종료한 뒤 300ms 후 master를 읽는 프로브에서 데이터 유실 **50/50**(read가
  곧바로 EOF 0 반환) — 결정적 동작이지 경합이 아니다.
- **부모가 drain 이후까지 slave를 열어 두면 50/50 전부 생존.**

deppy는 spawn 직후 `drop(pair.slave)`를 했다 ("설계문서 1.2 리스크 3" 때문). 그러면
자식이 유일한 slave 보유자라, echo처럼 빨리 끝나는 자식이 종료되는 순간 아직
reader가 읽지 않은 출력이 통째로 버려진다. 직렬에서는 reader가 거의 항상 먼저
읽어서 통과하고, 병렬에서는 스케줄링이 밀려 간헐적으로 진다 — 관측된 flake와
정확히 일치한다. **제품 버그이기도 하다: 짧게 출력하고 끝나는 명령의 꼬리 출력이
macOS에서 유실될 수 있다.** env와는 무관하다 — env 테스트를 제외한 skip-marker
실행에서도 발생했다.

**수정됨(2026-08-03):** 리스크 3은 Windows의 drop 순서 race 대응이라 unix에는 적용
대상이 아니었다. 이제 unix는 reader thread가 drain 동안 slave를 보유하고, 전담
reaper thread(`pty-reaper`)가 자식 종료를 감지해 reader를 깨우면 reader가 잔여
출력을 drain한 뒤 slave를 닫고 낙는다. kill/Drop 경로는 reaper_stop flag로 기존
즉시-종료 동작을 유지한다. 회귀 테스트:
`빠르게_종료되는_자식의_꼬리_출력은_유실되지_않는다`.

### 반증된 가설 — 테스트 간 env 상속 간섭

첫 판본은 아래 테스트를 원흉으로 지목했다. `crates/pty/src/lib.rs:1785` 부근:

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

추론은 이랬다: `set_var`가 프로세스 전역이니, 이 구간 동안 병렬 테스트가 띄우는
자식이 `leaked`를 상속해 env 단정이 물어진다. 그럴듯했지만 **실험으로 반증됐다**:

1. **프로브 테스트.** 자식이 9개 `INHERITED_AGENT_SESSION_VARS`와 TERM/LANG을 그대로
   출력하게 하고 항상 덤프하는 진단 테스트를 임시로 추가해, `cargo test -p pty --
   zzz_tmp_probe 부모_에이전트 --nocapture` 40회 + 실패한 전체 스위트 실행 전부에서
   자식 출력을 확인했다. **9개 전부 `absent`, TERM/LANG도 정상 — `leaked`는 단 한 번도
   관측되지 않았다.** spawn 경로의 `env_remove` 스크럽이 경합 중에도 유지된다는 뜻이다.
   (프로브는 검증 후 되돌렸다.)
2. **skip-marker 대조.** env 변경 테스트를 제외하고 병렬 실행(`cargo test -p pty --
   --skip 부모_에이전트 --nocapture`, 15회)했는데도 **6/15 실패**, 실패 양상도 동일.
   `set_var` 테스트는 원인이 아니다.

이 테스트 자체는 무고한 목격자다. 지우면 안 되는 이유는 여전하다(§4 C).

### 첫 판본 원인 분석 (기록용)

`std::env::set_var`는 프로세스 전역이다(그래서 Rust 1.80+에서 `unsafe`다). 이 테스트가
값을 세팅한 구간 동안 병렬로 도는 다른 테스트가 띄우는 자식 프로세스가 전부 그 값을
상속한다는 가설 아래, 자식의 환경을 단정하는 테스트들이 물어진다고 봤다:

- `embedded_pty는_no_color를_제거하고_truecolor_capability를_고정한다`
- `embedded_pty는_malloc_stack_logging을_제거한다`
- `finder처럼_lang가_비면_pty는_utf8_locale을_주입한다`

환경을 단정하지 않는 테스트(`출력과_종료코드`, `입력_echo_roundtrip`,
`output_chunk가_채널에_들어오면_worker_wake를_호출한다`)까지 깨지는 것도 같은 뿌리로
설명된다고 봤다 — `MALLOC_STACK_LOGGING=leaked`가 상속되면 자식이 느려지고 출력이
오염된다는 추정이었다.

첫 판본은 "증거로 굳힐 것"으로 `leaked`가 실제로 관측되는지 확인하라고 요구했고,
그 검증이 위 두 실험의 결과 **반증**으로 끝났다.

## 3. 오해하기 쉬운 것 두 가지

**`collect_output`이 타임아웃을 다 쓰는 게 아니다** (`lib.rs:2379`). `Err(Timeout) => continue`만
보면 5초를 항상 소모할 것 같지만, 프로세스가 끝나면 채널이 `Disconnected`가 되어
루프를 빠져나간다. 실측 0.62초가 그 증거다. 여기를 "최적화"하지 말라.

**Codex의 #66(`4c1df44`)이 만든 회귀가 아니다.** `crates/pty/src/lib.rs`를 #66 직전
버전으로 되돌려 8회 측정했더니 **동일하게 3/8 통과**였다. 원래 있던 부채다.

## 4. 권장 작업

### A'. spawn 경로의 openpty 실패를 재시도한다 (진짜 수정, 구현 완료)

`crates/pty`의 spawn 경로에서 pty 할당을 감싸고, 일시적 `openpty` 실패(errno -6 계열)를
재시도한다. C 프로브가 보여준 대로 이 실패는 전부 일시적이므로(최대 5회 재시도에서
영구 실패 0) 적은 재시도 횟수로 충분하다. **테스트만이 아니라 여러 pane이 동시에 뜨는
제품 경로도 보호하는 수정이다.**

구현됨(2026-08-03): `crates/pty/src/lib.rs`의 `openpty_with_retry`가 최대 5회 재시도하고,
회귀 테스트 `동시_spawn은_openpty_경합에도_전부_성공한다`가 동시 spawn 8개를 검증한다.
적용 후 병렬 20회 실행에서 `openpty -6` 실패는 0건이었다(당시 잔재 2건은 §2 부수 양상의
drain 유실 — 이후 그 수정으로도 해소).

이게 선행되면 아래 A는 선택 사항으로 낮아진다.

### A. 로컬 기본값을 CI와 일치시킨다 (선택, 로컬 편의)

`.cargo/config.toml`에 다음을 추가한다.

```toml
[env]
RUST_TEST_THREADS = "1"
```

`RUST_TEST_THREADS=1`만으로 pty 8/8, 워크스페이스 3/3 통과를 확인했다. 로컬에서
`cargo test`만 쳐도 CI와 같은 조건이 되어, "왜 나만 빨간가"가 사라진다. 다만 직렬화가
근본 원인을 고치는 게 아니다 — 병렬성을 없애 **OS의 openpty 경합이 드러나지 않게 가리는**
것뿐이다. A'가 머지된 뒤에도 남는 병렬 잔재 실패(§2 부수 양상)를 피하고 싶을 때의
로컬 편의 옵션으로 본다.

**부작용을 반드시 확인할 것**: 전체 테스트 실행 시간이 얼마나 늘어나는지 측정하라.
`[env]`는 `cargo run`을 포함한 모든 cargo 호출에 적용되므로, 런타임 동작에 영향이
없는지도 확인하라(테스트 하네스 전용 변수라 영향은 없어야 한다).

### B. 전역 env 테스트만 격리한다 — 근거 소멸로 폐기

`부모_에이전트_세션_마커는_pane에_상속되지_않는다`를 별도 통합 테스트 바이너리
(`crates/pty/tests/inherited_env.rs`)로 옮기는 안이었다. 그런데 §2의 반증 실험으로
이 테스트가 무고함이 확인됐으므로 **격리할 이유가 사라졌다.** `INHERITED_AGENT_SESSION_VARS`와
spawn 경로를 `pub`으로 넓히는 제품 API 대가만 남고 얻는 게 없으니 채택하지 않는다.

### C. 하지 말 것

- **타임아웃을 늘리지 말 것.** 원인이 시간이 아니다. 늘리면 증상이 가끔 가려질 뿐
  전체 실행만 느려진다.
- **`set_var` 테스트를 지우지 말 것.** 이 테스트는 실제 사고를 잡는다 — deppy를 다른
  코딩 에이전트 안에서 실행하면 `CLAUDE_CODE_CHILD_SESSION`이 자식에 새어 들어가
  transcript 저장이 꺼지고, 그러면 에이전트 감지와 단축키가 통째로 죽는다(2026-08-02 실증).
  이번 조사의 원인은 아니었지만, 그렇다고 이 가드가 덜 필요해지는 건 아니다.
- **뮤텍스로 줄 세우는 방식은 여기선 안 통한다.** `auth`/`clipboard_image`에서 쓴 수법인데
  (`f677328` 참조), 이번 원인은 macOS 커널의 pty 할당 경합이라 프로세스 안의 잠금으로는
  잡히지 않는다. 잠금을 잡아도 같은 OS 호출이 다른 프로세스·스레드와 경합한다.

## 5. 검증 방법

수정 후 아래를 각각 10회 이상 돌려 실패 0을 확인한다. 1회 통과는 증거가 못 된다.

```sh
# 로컬 기본값 (A안 적용 시)
for i in $(seq 1 10); do cargo test --workspace 2>&1 | grep -cE "^test .*FAILED"; done

# CI와 동일 조건 (항상 통과해야 한다)
cargo test --workspace --locked -- --test-threads=1

# 병렬 (A' 재시도 수정이 머지된 후, openpty -6 실패가 사라졌는지 확인)
for i in $(seq 1 10); do cargo test -p pty 2>&1 | grep -cE "^test .*FAILED"; done
```

A' 재시도 수정 전에는 병렬 루프에서 `openpty` -6 실패가 간헐적으로 남는다 — OS 경합이
원인이니 재시도가 머지되기 전까지 병렬 잔재 실패를 코드 회귀로 오인하지 말라. drain
유실 수정(§2, unix slave 보유 + reaper)도 함께 들어갔으므로 두 수정 이후 병렬 실행은
실패 0이 정상이다. 단, 수정 전부터 있던 아주 드문 잔재 flake(수정 후 누적 ~3/290회:
`full_pty`의 write_input 거절 1회, 마커 테스트 출력 부분 수신 1회)는 원인을 포착하지
못했다 — 동일 증상이 수정 전 코드에서도 나왔으므로 이번 변경의 회귀는 아니며, 부하
스파이크 시 스케줄링 기아로 추정한다. 재현되면 그때 별도 추적한다.

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
