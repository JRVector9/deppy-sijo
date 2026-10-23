# Pane / project API verification evidence — 2026-10-24

[최종 구현·리뷰 보고서](../../2026-10-24-pane-project-api-implementation-review.md).

이 디렉터리는 합성 fixture만 사용한 검증 증거다. 실행 중 앱·실제 계정 키·Keychain 값을 수집하지 않았다. RED 로그에는 누출 회귀를 검증하기 위해 일부러 만든 테스트 문자열이 포함될 수 있다. 로그는 원본 바이트 그대로 gzip으로 보존했으며 empty fmt/diff 로그도 성공한 명령의 원본이다. `SHA256SUMS`로 증거 무결성을 확인할 수 있다.

## 최종 결과

| 범위 | 실제 결과 | 파일 |
|---|---|---|
| core/session/storage/terminal | 801 PASS, 5 ignored | deppy-pane-api-leaf-tests.log.gz |
| runtime + agent-mcp | 462 + 99 PASS, 2 ignored | deppy-pane-api-runtime-mcp-final1.log.gz |
| App, 최종 DB 포트 분리 후 재실행 | 3872 unit + 39 integration PASS, 49 ignored | deppy-pane-api-app-final2.log.gz |
| secret | 86 PASS | deppy-pane-api-app-secret-final.log.gz |
| 합계 | 5359 normal PASS, 56 ignored | focused 재실행과 isolated child fixture는 중복 집계 제외 |
| strict Clippy | PASS, 50.90s | deppy-pane-api-clippy-final3.log.gz |
| fmt / boundary / usage source guard / diff | PASS | 각 final2 또는 final 로그 |
| synthetic offscreen | 1 PASS, 4 PNG | deppy-pane-api-offscreen-final.log.gz |

`app-secret-final`의 App 결과는 중복 실행이므로 합계에 더하지 않는다. `boundary-final`은 실제 6건 FAIL 기록이고 `boundary-final2`는 guard 변경 없이 포트 분리 후 실제 PASS다. gate7/prefix, selected-output/capacity, gate5/TOSTOP은 실제 RED다. gate8 focused와 전체 suite는 대응하는 최종 GREEN 증거다. 잘못된 fixture setup으로 실패한 초기 시도는 유효한 제품 RED로 포함하지 않았다.

## 소스와 UI

- `task-source-sha256.json`: 이번 작업 시작 baseline과 달라진 제품/회귀 소스 48개(37 modified, 11 new)의 SHA256. Git HEAD만으로 이미 dirty였던 기존 작업을 분리할 수 없어 baseline digest를 함께 기록했다. 미커밋 작업 상태이며 릴리스 proof가 아니다.
- `edit.png`, `api-quick-add.png`, `desktop.png`, `narrow.png`: 실제 제품 UI 코드를 합성 snapshot으로 offscreen 렌더한 네 장. root가 edit/API quick-add를 직접 확인했다. 실행 중 GUI의 수동 테스트로 해석하지 않는다.
- 지원·미지원 범위와 terminal 원증상 미재현은 최종 보고서를 따른다. 앱 배포/재시작 없음.

## 재검증

실행 당시 공통 환경: `CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`, Cargo `--offline --locked`. 루트가 모든 Cargo를 직렬 실행했다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-integration-20261023
shasum -a 256 -c docs/qa/assets/pane-project-api-20261024/SHA256SUMS
# Verify source digests using paths relative to the repository root:
python3 - <<'VERIFY'
import hashlib,json,pathlib
p=pathlib.Path('docs/qa/assets/pane-project-api-20261024/task-source-sha256.json')
for item in json.loads(p.read_text())['files']:
    assert hashlib.sha256(pathlib.Path(item['path']).read_bytes()).hexdigest()==item['final_sha256'], item['path']
print('Task source SHA256: PASS (48 files)')
VERIFY
```
