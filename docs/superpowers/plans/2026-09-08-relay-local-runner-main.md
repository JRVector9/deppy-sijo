# Relay Local Runner Main Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** #146의 개발용 Relay runner를 #168 exact850b7a8747bd71d74d7858a5d95f6c257a076857 위에서 독립 완성한다.

**Architecture:** POSIX sh 진입점 안의 Python 표준 라이브러리 runner가 env 데이터 검증과 private 상태/PID 관리를 담당한다. nonce supervisor는 자기 Popen child만 정리하며 ps 생명주기 identity가 일치할 때만 down이 supervisor를 종료한다. 모든 외부 실행은 argv이며 stdout/log로 자격증명을 출력하지 않는다. Tailscale은 소유하지 않은 포트를 변경하지 않는다.

**Tech Stack:** sh, Python3 표준 라이브러리, 기존 relay-server 및 web/relay-shell/build.sh, shellcheck.

---

현재 에이전트가 승인 범위를 inline 실행한다. 파일: scripts/relay-dev.sh, scripts/tests/test_relay_dev.py, .gitignore, docs/relay-local-runner.md 및 계획/handoff. #169 adapter, 앱 코드, deployment 설정은 변경하지 않는다. 실제 app 명령/build/relaunch/Tailscale/DNS/TLS는 실행하지 않는다.

### Task 1: RED 계약
- [x] 임시 fixture에서 스크립트의 embedded Python을 import한다. 순수 env parser, mode/symlink, port/host/path validation 및 ps identity predicate의 실패 stub를 먼저 만든다.
```python
self.assertRaises(RunnerError, parse_env, b'DEPPY_RELAY_DEV_ROUTE=$(touch /tmp/x)')
self.assertRaises(RunnerError, read_private, symlink)
self.assertFalse(owned_process(record, different_start))
```
- [x] `python3 -m unittest discover -s scripts/tests -p test_relay_dev.py -v`로 실제 RED 확인.

### Task 2: 구현·GREEN
- [x] env는 고정2field hex32/64, 4096byte 상한, unknown/duplicate/쉘 문법 거부, O_NOFOLLOW+fstat(owner,regular,0600), create O_EXCL0600. shell source/eval 없음.
- [x] private .relay-dev0700 state와 flock으로 up/down을 직렬화한다. nonce supervisor PID/start/command를 기록하고 exact identity 불일치 시 kill하지 않는다. child stdout/stderr DEVNULL, timeout cleanup에서 자기 Popen child만 종료·wait한다. tail/pgrep/pkill 없음.
- [x] loopback bind, 1..65535 port, DNS ts.net host, path length/control validation. Tailscale status JSON bounded read와 subprocess timeout, 사용 중 serve 포트 거부, 생성 후 port별 snapshot이 일치할 때만 off.
- [x] up은 relay-server만 빌드하고 기존 shell build.sh 아카이브 manifest/hash 및 tar member allowlist/크기를 검증해 private serve 디렉터리에 전개한다. app은 --launch-app 없으면 명시적 경고/거부하며 어떤 기존 앱도 kill하지 않는다.
- [x] fixture subprocess로 자체 supervisor 종료 및 unrelated 프로세스 보존, stale pid/identity, env 비출력, build/TS 실패 cleanup을 검증한다. 실제 app 명령은 테스트에서도 호출하지 않는다.

### Task 3: gate·review·게시
- [x] Python 전체 계약 tests, `sh -n`, `shellcheck scripts/relay-dev.sh`, fmt/diff/boundary를 실행한다. Tailscale/DNS/TLS/배포 실검증은 외부 자격/조건 부재 BLOCKED로 기록한다.
- [x] actual source Codex CLI review를300초상한 실행하고 필요 시 제한 readonly 리뷰로 완료한다. 확정 finding RED→GREEN 보완 및 재검증.
- [ ] handoff/Obsidian 일지, 한국어 commit/push, `gh pr create --base feat/relay-shell-main-clean --head dev/relay-local-runner-main --body-file /private/tmp/deppy-relay-runner-pr.md` Ready PR 생성. final HEAD/checks/annotations/clean 확인. rebase/force/merge 금지.
