# 개발용 Relay local runner

`scripts/relay-dev.sh`는 #168의 relay-server와 모바일 셸 아티팩트를 한 Mac에서 개발할 때 쓰는 도구다. #169 앱 adapter 코드는 포함하지 않는다. 실제 Tailscale·DNS·TLS·실기기·배포 검증은 자격증명과 외부 조건이 없어 **BLOCKED**다. 로컬 fixture 통과가 배포 성공을 뜻하지 않는다.

필요 도구는 Python 3.9 이상(POSIX waitid/WNOWAIT), cargo, 기존 shell build 도구, lsof, 로그인된 Tailscale CLI다. 이 PR 검증에서는 실제 Tailscale이나 앱을 실행하지 않았다.

```sh
sh scripts/relay-dev.sh status  # 기본 명령; 자체 PID 기록만 확인
sh scripts/relay-dev.sh up      # relay-server 빌드, 셸 아티팩트 빌드, 자체 서버 및 serve 설정
sh scripts/relay-dev.sh env     # endpoint/origin만 표시; route/admission 값은 표시하지 않음
sh scripts/relay-dev.sh down    # 소유 identity가 일치하는 프로세스와 serve 설정만 정리
```

선택 입력은 `TAILSCALE_BIN`, `RELAY_PORT`(8443), `SHELL_PORT`(10000), `RELAY_LOCAL`(127.0.0.1:9443), `SHELL_LOCAL_PORT`(10080)다. TCP bind는 loopback만 허용한다. tailnet DNS는 유효한 `*.ts.net` 이름이어야 한다. 포트·경로·env 크기/필드/hex 형식은 실행 전에 제한한다.

자격증명은 저장소 루트의 `.relay-dev.env`에 0600으로 생성한다. 기존 파일은 해당 사용자 소유의 regular file, 단일 링크, 정확한 0600이어야 한다. 두 hex 필드만 데이터로 파싱하며 기존 주석은 허용한다. shell source/eval은 사용하지 않는다. symlink나 느슨한 파일 모드는 거부한다.

상태·PID·빌드 자산은 0700 `.relay-dev/` 아래에 둔다. 자체 supervisor의 PID, 시작 시간, 명령, 실행별 nonce를 확인하며 다른 프로세스를 이름으로 찾아 종료하지 않는다. 서버 child identity와 해당 child의 LISTEN 포트도 확인한다. 별도 tail 프로세스와 서버 출력 로그는 만들지 않는다. 빌드 오류 원문도 secret 유출을 막기 위해 출력하지 않는다.

`up`은 기존 serve 기록이나 사용 중인 포트를 덮어쓰지 않는다. 빌드 후 적용 직전에 다시 확인하고, 적용 전에 expected/pending 기록을 남긴다. status 조회가 실패하면 이 기록을 보존한다. 외부 조건이 복구되면 `down`으로 재확인할 수 있다. 외부에서 바뀐 포트는 유지하고, 나머지 자체 포트는 계속 정리한다. Tailscale CLI의 원자적 CAS를 가정하지 않으므로 이 runner 전용 포트를 예약하고 동시에 다른 CLI로 같은 포트를 편집하지 않아야 한다. 기존 443 설정을 reset하지 않는다.

`app`은 `--launch-app`을 명시한 경우에만 기존 dev-run 경로로 GUI를 빌드하고 새로 실행한다. 기존 앱은 종료하지 않는다. 자격증명 환경변수 소비는 해당 앱 버전의 adapter 구현에 달려 있다. 이번 작업에서 이 명령은 fixture를 포함해 실행하지 않았으며, AST로 opt-in과 종료 호출 부재만 검증했다.

로컬 계약 검증:

```sh
python3 -m unittest discover -s scripts/tests -p test_relay_dev.py -v
sh -n scripts/relay-dev.sh
shellcheck scripts/relay-dev.sh
```

API 근거: [공식 Serve CLI](https://tailscale.com/docs/reference/tailscale-cli/serve), [공식 ServeConfig 예시](https://tailscale.com/docs/kubernetes-operator/reference/troubleshooting). 지원하지 않는 응답 형식은 pending으로 보존하고 오류를 반환한다.
