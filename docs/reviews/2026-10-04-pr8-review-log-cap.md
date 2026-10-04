# PR8 후속 수정: append 오류 경로의 로그 상한 복구

## 목표와 범위

- 기준 커밋: `ae967f8ceb801ffe31743dcbd122a05d874614eb`.
- 작업 브랜치: `fix/audit-pr8r-log-error-cap-20261004`.
- 작업 위치: `/private/tmp/deppy-audit-pr8r-20261004`.
- 수정 파일: `crates/storage/src/logs.rs`, 이 검토 기록.
- PR8의 확인된 오류 경로를 보정한다. 새로운 기능 PR이나 릴리스 작업은 아니다.

## 재현한 원인

`BoundedLogFile`은 정상 append에서 metadata 조회를 줄이기 위해 확인된 길이를
캐시한다. 외부 append로 실제 파일이 커진 뒤 자체 append가 일부만 쓰고 실패하면,
기존 코드는 `write_all`의 오류에서 즉시 반환하여 실제 EOF 확인과 상한 압축을
건너뛰었다.

수정 전 실행한 회귀 시험은 cap 64바이트에서 자체 32바이트, 별도 append handle의
32바이트, 20바이트 중 주입된 부분 쓰기 5바이트를 사용했다. append는 오류를
반환했지만 파일은 69바이트가 남았다. 상한 assertion이 이 수치로 실패했다.

## 완료한 변경과 결정

- 모든 append 오류는 길이 캐시를 무효화한 뒤 기존 pinned handle의 metadata로
  실제 길이를 확인한다. 상한을 넘으면 기존 tail 압축 함수를 같은 handle에 적용한다.
- 원래 append를 재시도하지 않는다. 부분 기록된 바이트를 중복으로 쓰지 않는다.
- 복구가 성공해도 원래 append 오류를 반환하고 길이 캐시는 unknown 상태로 둔다.
  다음 append가 실제 길이를 다시 확인한다.
- 복구도 실패하면 그 실패를 오류 context에 포함하여 원래 오류와 함께 보존한다.
  길이를 0으로 가정하거나 성공으로 보고하지 않는다.
- 정상 append에는 metadata 조회를 추가하지 않았다.
- 기존 ANSI/UTF-8 tail 경계 함수, inode 고정, symlink 거부 및 redaction 계약을 유지했다.
  복구는 경로를 다시 열거나 redaction 전 원문을 받지 않는다.
- 회귀 시험은 부분 쓰기 상한, EOF position 조회 오류, 복구 metadata 오류와 후속
  회복, 경로 교체 후 pinned inode 및 OSC/UTF-8 경계를 검증한다.

## 실행한 검증

모든 Cargo 명령은 수정된 공유 target 소유권/잠금 gate를 거쳤다.

```sh
cd /private/tmp/deppy-audit-pr8r-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -p storage pr8_external_append_then_partial_write_error_repairs_file_cap -- --nocapture
```

- RED: production 수정 전 실행. 0 passed / 1 failed, exit 101.
  오류: `failed partial append left 69 bytes beyond cap`.
- GREEN: production 수정 후 같은 명령 실행. 1 passed / 0 failed, exit 0.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","storage","pr8_","--","--nocapture"],["test","--offline","--locked","-p","storage"],["clippy","--offline","--locked","-p","storage","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]'
```

- PR8 집중 시험: 11 passed / 0 failed, 0.04초.
- 전체 Storage: 408 passed / 0 failed, 19.80초. Doc-tests: 0개.
- 엄격한 Storage Clippy: `--all-targets -- -D warnings` 통과, 3.32초.
- 정상 append 2048 × 128바이트: metadata 0회, write_all 2048회.
  같은 집중 시험의 5회 시간 중앙값은 5408.791µs다. 환경 독립적인 속도 보장은 아니다.
- 첫 format check는 새 assertion 한 곳의 줄바꿈 차이로 exit 1이었다. 해당 부분을
  rustfmt 기대 형식으로 수정했다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py fmt --all -- --check
git diff --check
```

- 최종 workspace format check: exit 0.
- 최종 diff check: exit 0.

## 남는 한계와 다음 단계

외부 writer의 임의 동시 변경을 원자적으로 통제하지 않는다. 복구 시 metadata 조회나
압축 자체가 실패하면 상한 복구를 보장할 수 없으며 오류에 이를 남긴다. 주입 시험에서
metadata 복구 실패 시 69바이트가 유지되고, 오류가 해제된 다음 append에서 상한으로
회복되는 것을 검증했다. 경합을 검출하는 기존 압축 검사는 그대로 유지한다.

이 변경의 목적은 오류를 반환하기 전에 같은 pinned handle의 상한 복구를 시도하는
것이다. 새로운 제품 코드 변경이나 추가 범위의 검증은 필요하지 않다. 루트 통합자는
최종 커밋을 cherry-pick하고 통합 소스에 필요한 검증을 수행한다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-performance
git cherry-pick fix/audit-pr8r-log-error-cap-20261004
```
