# Command Center v2 원안 복구

## 고정 기준

- 원안 source: `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`.
- 원안 branch: `rollback/command-center-pre-v2-20261020`.
- 원안 annotated tag: `command-center-pre-v2-20261020`.
- 구현 통합 branch: `feat/agent-command-center-v2-20261020`.
- 승인한 디자인은 `docs/design/command-center-v2/reference.json`의 SHA로 보존한다.

## 복구 계약

1. 기존 화면과 런타임을 유지하며 새 화면은 명시적 기능 스위치로 선택한다. 스위치를 끄면 원래 화면 경로를 사용한다.
2. 작업 배정 데이터는 `command-center-v2.sqlite3` 보조 DB에 저장한다. 기존 `metadata.sqlite3`의 migration version을 올리지 않는다. 원안 앱은 새 DB를 열 필요가 없고 기존 데이터를 그대로 사용한다.
3. 작업자 실행 중 화면을 되돌려도 해당 프로세스를 임의로 종료·재실행하지 않는다. 원래 터미널 경로에서 계속 접근한다.
4. 원안으로 돌아가며 새 작업 기록을 삭제하거나 기존 DB를 덮어쓰지 않는다.

## 소스 복구

변경 중인 폴더를 초기화하지 않고 원안용 별도 폴더를 만든다.

```sh
git worktree add --detach /Users/jr/Desktop/projects/deppy-sijo-command-center-original-20261020 command-center-pre-v2-20261020
```

이 명령은 앱을 종료·실행하지 않는다. 앱 교체·재실행은 해당 시점의 사용자 요청에 따라 진행한다.
## 실행 중 화면 복귀

새 화면은 환경변수 `DEPPY_COMMAND_CENTER_V2=1`에서만 시작한다. 미설정·`0`·기타 값이면 기존 화면을 사용한다. 새 화면 상단의 **기존 화면**을 누르면 세션을 종료하지 않고 기존 레이아웃으로 돌아간다. opt-in으로 시작한 경우 기존 화면에서 다시 Command Center로 진입할 수 있다.

소스 위치는 `crates/app/src/command_center_switch.rs`다. 실제 source를 실행한 전환 검사 2개가 통과했다. 앱 프로세스를 종료·실행한 복구 검사는 이번 작업에서 실행하지 않았다.

## 데이터 복구 안전성

`command-center-v2.sqlite3`는 새 화면 전용 보조 DB다. 기존 `metadata.sqlite3`와 migration 원장은 변경하지 않는다. 임시 metadata fixture 바이트를 보존하면서 sidecar 저장·복원을 실제 검사했다. 원안 소스는 보조 DB를 읽거나 삭제하지 않는다. 새 화면의 작업자도 기존 세션·터미널 경로로 확인한다.

새 UI에서 진행 중인 저장·입력 요청은 화면 복귀만으로 재전송하지 않는다. 승인 여부를 확인하지 못한 요청을 자동으로 다시 실행하지 않는 계약을 유지한다. 원안으로 돌아가기 위해 `git reset --hard`, DB 삭제 또는 작업자 강제 종료를 사용할 필요가 없다.
