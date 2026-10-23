| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| medium | `crates/remote-host/src/supervisor.rs:302` | stdout 정체 시 bridge 종료 대기 무한 지속 | 연결이 끝나도 원격 bridge 프로세스·스레드 잔류 | 출력 쓰기에 취소·기한 적용 |
| medium | `crates/app/src/ui/workspace.rs:5647` | 렌더 생략 동안 dirty 범위 유실 | 선택 해제·홈 복귀 시 일부 행이 이전 내용 | catch-up 캐시 무효화 또는 dirty 누적 |
| medium | `crates/terminal/src/alacritty_backend.rs:1573` | 아카이브가 숨김·SGR 속성 누락 | 재열람 시 숨긴 글자 표시·서식 소실 | 숨김·서식을 보존하는 직렬화 |

# SSH 연결·터미널 렌더링·수명 병렬 코드 리뷰 — 2026-10-23

## 범위와 결과

Canonical tree `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, branch `fix/product-review-integration-20261023`, reviewed HEAD `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1` (product source `9aca83f2`, version0.17.0). 사용자 요청으로 GPT-6.1-Sol/xhigh 세 리뷰어가 SSH 연결/보존, 터미널 렌더링, 리소스/크래시를 나누어 검토하고 root가 코드·재현 결과를 교차 확인했다.

**재현한 결함3건**, 모두 medium/신뢰도10/10. 제품 코드 변경, 앱 재시작, 실제 사용자 SSH 세션 조작, 서버 helper 설치, push/merge는 하지 않았다. 이 보고서·handoff·재현 증거만 추가했다. 실행 앱은 이전0.16.0 그대로다. 설치된 review skill의 필수 checklist 파일은 없어 전체 skill 실행 완료로 주장하지 않고 저장소 코드와 직접 재현으로 검토했다.

## Details

### 1. 출력 정체 시 SSH bridge 종료가 끝나지 않는다

조건: stdout 읽기 끝이 열려 있지만 소비하지 않아 파이프가 가득 찬 상태에서 입력 EOF/연결 종료가 발생한다. `supervisor.rs:302–303`의 stdout `write_all/flush`에는 취소/기한이 없다. main은 `338`에서 UnixStream만 shutdown하고 `339–341`에서 출력 스레드를 join하므로 stdout 파이프 쓰기를 깨우지 못한다.

최신 실제0.17.0 helper로 HOME 격리·lease2초 fixture를 실행했다. root 독립 재현은 bounded Ping901개 뒤 stdin EOF, **2.633초 후 supervisor socket은 사라졌는데 bridge는 생존**했다. stdout74,397bytes를 읽자 **0.002초에 종료**했다. read end가 완전히 닫히면 EPIPE로 풀리므로 모든 네트워크 단절에서 무조건 발생한다고 일반화하지 않는다. Ping 부하의 정상 오류 종료 자체가 결함은 아니며, 해당 오류/취소 뒤에도 출력이 정체되면 종료되지 않는 부분이 결함이다.

별도 단일 정상128KiB ReadFile RPC로도 stdin EOF 후7.36초 bridge 생존, stdout 소비 재개0.001초 뒤 종료를 재현했다. 이 fixture에서는 관측 시점 supervisor socket이 남아 있었으므로 supervisor 정리가 완료됐다고 주장하지 않는다. 모든 fixture는 소유 프로세스만 종료·wait하고 격리 HOME을 삭제했다. 사용자 서버에는 접근하지 않았다.

개선: bridge 출력 쓰기가 취소/EOF/기한에 반응하도록 하고, stdout 소비 중단·읽기 끝 폐쇄·정상 대량 출력·취소를 회귀 검사한다. socket timeout만 늘리는 변경은 이 stdout 대기를 해결하지 못한다.

### 2. 최신 snapshot을 받았어도 일부 행이 예전 내용으로 남는다

`workspace.rs:5647`은 선택 freeze 중 pending snapshot을 매번 교체하면서 이전 dirty 범위를 합치지 않는다. 선택 해제 후 `7873–7874 → install_snapshot:2954`는 화면 데이터와 generation만 갱신하고 행 render cache는 유지한다. `renderer_egui.rs:216/652–681`은 generation이 달라졌다는 사실만 보고 마지막 snapshot의 dirty 행만 재생성한다.

Canonical renderer source를 직접 include한 독립 fixture로, 두 행 OLD0/OLD1을 그린 뒤 선택 중 서로 다른 시점에 NEW0/NEW1을 받는 경로를 재현했다. 최신 snapshot은 두 행 모두 NEW인데 실제 egui가 발행한 텍스트는 **OLD0|NEW1**, rebuilt_rows=1이었다. root도 동일 재현을 실행했다. cache.clear() 대조군은 NEW0/NEW1로 정상이다.

Home/Fleet에서도 `app.rs:39242–39251 → workspace.rs:6129`가 렌더 없이 이벤트/snapshot을 계속 적용하고, 복귀 토글(`app.rs:38801/38813`)이 cache를 초기화하지 않아 같은 원인이 적용된다. 이 경로는 코드 대조로 확인했으며 실제 사용자 UI를 조작해 검증했다고 주장하지 않는다. 일반 receiver의 최신 슬롯 교체는 `runtime/src/event.rs:361` dirty 병합이 있어 별도 결함으로 중복 계산하지 않았다.

개선: 표시하지 않은 snapshot 이후의 catch-up에서는 전체 캐시를 무효화하거나, 마지막으로 실제 그린 상태 이후 dirty를 제한 있는 범위로 누적한다. freeze와 Home/Fleet 복귀를 함께 검사한다. 화면 크기를 바꿔 캐시가 재생성되면 우연히 복구될 수 있어 평상시 테스트가 놓칠 수 있다.

### 3. 종료된 화면 아카이브 재열람이 숨김을 해제한다

`alacritty_backend.rs:1558`은 fg/bg만 추적하며 `1573–1576`에서 HIDDEN(SGR8) 원문을 숨김 속성 없이 쓴다. bold/italic/underline/strike 속성도 직렬화하지 않는다.

실제 public AlacrittyBackend API 왕복: 처음에는 `private-value`가 공백으로 표시되는데 serialize_scrollback→새 backend.feed 후에는 원문이 보였다. 일반 서식도 CellAttrs(15)→CellAttrs(0)으로 사라졌다. root가 재실행해 확인했다.

실제 메모리 archive 경로는 `runtime/src/in_process.rs:6725 → session/src/session.rs:323`, 복원은 `in_process.rs:6968 → 7062 → session.rs:276`이다. 디스크 저장에는 별도 redaction이 있으므로 모든 저장 경로에 redaction이 없다고 주장하지 않는다. 임의 SGR8 텍스트의 숨김을 색상만으로 복원할 수는 없다. 터미널 숨김은 암호화/보안 경계 자체는 아니지만 재열람 시 표시 의미가 바뀌는 버그다.

개선: 숨김을 공백으로 투영하거나 SGR8을 보존하고, 나머지 SGR 속성도 archive round-trip에서 보존한다. 숨김/색/서식·wide/combining 문자·trailing 빈칸 회귀를 함께 확인한다.

## 세션 유지 정책: 무기한 유지가 아님

- `remote-host/src/supervisor.rs:381–385/449–452`: 서버가 attachment 종료를 인식한 뒤 기본300초 보존, 환경설정 `DEPPY_REMOTE_LEASE_SECONDS` 허용1–1800초. 이후 backend.shutdown으로 세션을 종료한다.
- `app/src/app.rs:17137`: 종료된 owner는 offline/retry 표시로 남기고 자동 재연결하지 않는 명시 정책이다. 따라서 오래 잠자기 후에도 계속 유지·자동 복귀를 보장하는 구현은 아니다.
- heartbeat15초, 클라이언트 무응답45초, supervisor socket I/O45초. 실제 망 단절 시 서버가 종료를 인식하는 시점과 실제 네트워크가 사라진 시점은 다를 수 있다.
- `supervisor.rs:134`는 helper release version으로 socket namespace를 분리한다. 서로 다른 버전의 지속 세션 이행은 이 리뷰에서 지원된다고 판단하지 않았다. 동시 attachment 대기/버전 격리는 명시 정책과 관련해 별도 확정 결함으로 올리지 않았다.

요구사항이 장시간 잠자기에도 원격 작업 계속 유지라면, 위3개 버그와 별도로 보존 정책·안전한 자동 재연결·버전 이행 동작을 제품 요구사항으로 정해야 한다.

## 실제 실행한 검사

공유 Cargo target `/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`, jobs2. root가 순차 실행했으며 subagents는 Cargo를 실행하지 않았다.

| 명령 (`cargo test --offline --locked`) | 결과 |
|---|---|
| `-p runtime ssh_stream -- --test-threads=2` | 21PASS |
| `-p deppy-remote-host -- --test-threads=2` | 29unit+4integrationPASS, 2child fixture top-level ignored |
| `-p ssh-transport -- --test-threads=2` | 24PASS, 2별도 실제 loopback ignored |
| `-p terminal -- --test-threads=2` | 140PASS, 4별도 측정 ignored |
| `-p deppy-sijo ssh_ -- --test-threads=2` | 96PASS, 3별도 Linux/시각 검사 ignored |

**총314PASS/0FAIL**. ignored/filtered/부모에서 호출한 child fixture를 중복 합산하지 않았다. 이 기존 테스트들이 위 새 재현3건을 포착했다는 뜻은 아니다. 버그 재현 fixture는 현재 오동작을 확인하는 assertion으로 성공하며, 제품이 정상이라는 테스트가 아니다.

독립 실제 SshRuntimeClient connect→Drop50회(25idle+25응답 없는128KiB RPC)도 실행했다. root 재실행의 baseline thread/FD1/3→매회1/3, peak6/7, 최대 정리15ms. 명시shutdown 없이 production Drop 경로를 검사했다. Cargo fresh compiler-artifact가 canonical runtime manifest와 사용한 rlib를 연결하고, libruntime.rlib와 deps/libruntime-6b004cc85219a80a.rlib의 바이트가 같음을 확인했다. 50회 범위의 수명 검사이며 무기한 누수 없음 보장은 아니다.

리뷰에서 추가 확정 panic/잠금 교착은 찾지 못했다. 실제 Mac 잠자기, 장시간 고부하 RSS, 실제 Linux 서버의 네트워크 blackhole, 사용자 Claude/Codex/Grok 장기 세션은 이번에 재현하지 않았다. 원격 helper 통합 테스트는 격리 native helper/PTY 기준이며 실제 사용자 OpenSSH 서버 검증을 대신하지 않는다.

## 증거와 다음 명령

소스·정확 compile 인자·root/agent 재현 결과·소스 hash·손실 없는 압축 테스트 로그는 `docs/qa/assets/ssh-terminal-lifecycle-review-20261023/`, SHA256 index는 `SHA256.json`에 보관했다. 실행 바이너리는 git에 추가하지 않았다. renderer 명령은 해당 `renderer-commands.txt`, lifecycle argv는 `lifecycle-compile-argv.json`이다. SSH Python fixture는 전용HOME/프로세스만 만든다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-integration-20261023
git status --short --branch
git diff --check
# Build current investigation helper before reproducing if its version changed.
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target cargo test --offline --locked -p deppy-remote-host -- --test-threads=2
python3 docs/qa/assets/ssh-terminal-lifecycle-review-20261023/bridge_backpressure.py
# Read renderer-commands.txt for direct rustc commands (no product edits).
# Do not launch/restart the app; no restart requested in review.
```
