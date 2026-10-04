# PR18 — Runtime 테스트 secret 저장소 격리

## 범위와 확인된 원인

- 기준 커밋: `d7b0acaa0806aa0b083bea7d5e5817e7f80bff58`.
- Runtime의 `in_process`와 `remote` 테스트만 private in-memory `SecretStore`를 주입하도록 변경한다. 제품 코드, 의존성, 버전, lockfile, App 및 사용자 저장소는 변경하지 않는다.
- 최종 통합 gate의 Runtime 단독 직렬 실행에서 `로그_secret_scan_평문_없음`이 8분 이상 종료되지 않았다. 원본 로그는 `/tmp/deppy-final-root-workspace-20261004.log`, owned test PID 76085의 실제 stack sample은 `/tmp/deppy-final-runtime-hang-20261004.txt`에 보존되어 있다. 해당 gate는 workspace-minus-Runtime 4475 passed / 0 failed / 47 ignored 이후 Runtime 337개 실행 중 멈췄고, root가 해당 테스트 프로세스만 종료하여 exit 101로 끝났다.
- 실제 stack의 worker 경로는 `SecretStoreResolver::resolve -> KeyringSecretStore::get_secret -> secret::macos::get -> SecItemCopyMatching`이다. 테스트 스레드는 unwind 중 `InProcessRuntimeClient::shutdown -> JoinHandle::join`에서 worker를 기다리고 있었다.
- 기존 테스트 helper는 `KeyringSecretStore`를 생성하고 `keyring_core`의 전역 mock backend를 등록했다. macOS에서 dependency로 빌드한 `secret`에는 Runtime의 `cfg(test)`가 전달되지 않는다. `test-keyring-core` feature가 없으면 별도의 native SecItem 구현이 선택되어 mock 등록을 우회한다. 다른 workspace의 dev-feature 통합에 테스트 안전성을 맡길 수 없다.
- 이전 `spawn_agent_secret_env_주입` timeout도 같은 helper를 사용하지만 그 당시 native stack은 없다. 이번 hang의 원인이 확인되었다는 사실을 이전 timeout의 원인 확정으로 확대하지 않는다. 그 fixture의 기존 failure-only 진단은 유지한다.

## 구현 결정

- `#[cfg(test)] mod test_secret_store`에 공유 helper를 두고, 호출마다 빈 `MemorySecretStore`의 새 `Arc<dyn SecretStore>`를 반환한다. 동일 fixture가 worker와 저장소를 공유할 때만 Arc를 명시적으로 clone한다.
- fixture마다 소유하는 mutex/BTreeMap으로 set/get/overwrite, 확인된 부재, idempotent delete, prefix inventory API를 구현한다. 저장된 값과 get 반환값은 기존 zeroizing `SecretString`을 사용한다. 부재 오류는 값이나 credential 좌표를 포함하지 않는 고정 문자열이다.
- Runtime 테스트의 전역 mock 등록과 native store 생성을 모두 제거한다. production `SecretStoreResolver`, PTY spawn, secret env 주입, redactor 및 로그 검증 로직은 그대로 유지한다.
- 원래의 native hang을 다시 실행하지 않는다. 별도의 source-isolation regression을 먼저 실행하여 기존 helper의 실제 native adapter 선택을 안전하게 RED로 확인했다.

## 실행 기록

모든 Cargo 명령은 `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`를 통해 실행한다. 아래 로그는 원본 stdout/stderr를 보존한다.

### RED

- `/tmp/deppy-pr18-secret-store-safe-red-20261004.log`: exit 101. `pr18_runtime_test_helpers_never_select_native_keyring_backend` 0 passed / 1 failed. 실제 `in_process.rs`의 `secret::KeyringSecretStore` 선택 때문에 실패했다. 이 테스트는 소스를 검사하며 keychain, PTY, runtime worker를 호출하지 않는다.
- 실제 명령:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","runtime","pr18_runtime_test_helpers_never_select_native_keyring_backend","--","--test-threads=1","--nocapture"]]' > /tmp/deppy-pr18-secret-store-safe-red-20261004.log 2>&1
```

### GREEN 및 최종 gate

- `/tmp/deppy-pr18-secret-store-focused-green-20261004.log`: exit 0. 새 PR18 regression 5개와 기존 실제 private PTY fixture `spawn_agent_secret_env_주입`, `로그_secret_scan_평문_없음`, `spawn_agent_resolve_실패시_spawn_안함` 각 1개가 모두 passed. 기존 secret env 값 검증, 로그 평문 부재/치환 마커 검증, resolve 실패 시 spawn하지 않는 검증 및 timeout은 변경하지 않았다.
- `/tmp/deppy-pr18-secret-store-final-gate-20261004.log`: exit 0. Runtime 전체 직렬 **342 passed / 0 failed / 0 ignored**, 50.97s. Runtime doc-test 0개. Runtime all-target strict Clippy `-D warnings` 및 workspace fmt check passed.
- source 비교에서 `in_process.rs`와 `remote.rs`의 테스트 모듈 이전 부분은 기준 커밋과 byte-for-byte 동일하다. `lib.rs`는 cfg(test) 모듈 선언만 추가했다. 실제 비교 기록: `/tmp/deppy-pr18-secret-store-source-proof-20261004.log`. `git diff --check` passed.
- 실제 명령:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","runtime","pr18_","--","--test-threads=1","--nocapture"],["test","--offline","--locked","-p","runtime","in_process::tests::spawn_agent_secret_env_주입","--","--exact","--test-threads=1","--nocapture"],["test","--offline","--locked","-p","runtime","in_process::tests::로그_secret_scan_평문_없음","--","--exact","--test-threads=1","--nocapture"],["test","--offline","--locked","-p","runtime","in_process::tests::spawn_agent_resolve_실패시_spawn_안함","--","--exact","--test-threads=1","--nocapture"]]' > /tmp/deppy-pr18-secret-store-focused-green-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","runtime","--","--test-threads=1"],["clippy","--offline","--locked","-p","runtime","--all-targets","--","-D","warnings"],["fmt","--all","--check"]]' > /tmp/deppy-pr18-secret-store-final-gate-20261004.log 2>&1
git diff --check
```

완료된 테스트 목록: fixture별 store 격리와 Arc를 통한 thread 공유, overwrite/부재/idempotent delete, prefix inventory, 실제 `SecretStoreResolver`의 injected-store 사용, Runtime helper의 ambient/native backend 선택 금지. 전체 직렬 gate는 remote helper 변경도 포함한다. 추가 optional suite나 native keychain 재현은 실행하지 않았다.

## 한계

이 변경은 Runtime 테스트 harness의 native keychain 접근을 제거한다. 실제 macOS keychain의 지연, 접근 권한, 장애 및 제품의 keychain 동작을 테스트하거나 변경한 것은 아니다. 기존 실제 private PTY fixture의 secret 값 주입 및 로그 redaction 검증은 그대로 실행한다. native 앱 또는 사용자 keychain/clipboard/session 데이터는 조작하지 않는다.
