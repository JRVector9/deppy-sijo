# 커밋·푸시와 구형 워크트리 정리 — 2026-10-24

사용자가 커밋·푸시와 `~/Desktop/projects` 안의 불필요한 프로젝트 폴더 정리를 요청했다. 검증된 작업 브랜치 `fix/product-review-integration-20261023`에 커밋하고 정상 푸시했다. main 병합·버전 배포·앱 재시작은 하지 않았다.

## 커밋과 검증

- `4c8c58ed`: 이전에 검증한 SSH 복구·경로 링크·런처 전환·Claude/Grok 사용량 수정과 보호 harness. 보존된 API 작업 전 snapshot에서 따로 stage해 두 단계를 혼합하지 않았다.
- `ed842f0c`: 조밀한 키 편집기·직접 API 등록·로컬 세션 API 발견/권한/실행·회귀 및 병렬 리뷰 결과. 원격 HEAD `ed842f0c82009b489cbe6d3e2419fed04928329a`와 일치 확인.
- 최종 테스트5359 PASS/56 ignored 소스와 현재48개 작업 파일의 SHA256 일치. 제품 코드 변경 없이 source guard·staged guard·diff check를 재검증했다. 같은 전체 테스트를 이번 커밋 단계에서 다시 실행한 것으로 세지 않는다.

## 제거한 폴더

먼저 미커밋·미추적·무시된 비캐시 파일까지 전체 소스를 private archive에 저장했다. 각 archive를 다시 읽어 모든 파일 바이트·mode·symlink를 원본과 대조하고, status/HEAD/branch 및 활성 open file/cwd가 없는지 재확인한 뒤 `git worktree remove --force`로 제거했다. Git 브랜치와 커밋은 삭제하지 않았다.

| 폴더 | 보존한 브랜치 | HEAD | 백업 항목 수 |
|---|---|---|---|
| deppy-sijo-agent-wait-audit | `fix/environment-api-context-integration` | `2a1583d1` | 1283 |
| deppy-sijo-cc-fix-native-20261020 | `fix/cc-review-native-20261020` | `174d8ca1` | 2621 |
| deppy-sijo-cc-fix-pwa-20261020 | `fix/cc-review-pwa-20261020` | `7a98674e` | 2628 |
| deppy-sijo-cc-fix-workflow-20261020 | `fix/cc-review-workflow-20261020` | `91f534ef` | 2616 |
| deppy-sijo-cloud-agent-mcp | `feat/cloud-agent-mcp` | `78f054b9` | 1321 |
| deppy-sijo-command-center-20261020 | `feat/agent-command-center-v2-20261020` | `24260718` | 3159 |
| deppy-sijo-main-workspace-menu-20261022 | `main` | `bc950e88` | 2495 |
| deppy-sijo-perf-model | `perf/seven-snapshot-model` | `ffaa8093` | 1306 |
| deppy-sijo-perf-storage | `perf/seven-visible-rows` | `32a4abea` | 1307 |
| deppy-sijo-pwa-reading-20261008 | `feat/pwa-reading-pr4-20261008` | `27186086` | 1750 |

제거10개 모두 실제 경로 부재와 Git worktree 등록 해제를 확인했다. 각 branch HEAD10개와 backup archive SHA25610개를 재검증했다. `.git`, 재생성 가능한 target/node_modules/pycache와 .DS_Store만 archive에서 제외했으며, 제외 경로에 추적 파일이 없는 것을 확인했다.

## 보존한 폴더

| 폴더 | 이유 |
|---|---|
| deppy-sijo | 공용 Git 저장소와 현재 shell/agent 작업 위치 |
| deppy-sijo-integration-20261023 | 현재 검증한 소스 및 실행 중 앱 bundle |
| deppy-sijo-ime-cloud-20261015 | 실행 중 Python 미리보기의 작업 위치 |
| deppy-sijo-performance | 실행 중 Python 미리보기의 작업 위치 |
| deppy-sijo-ssh-workspace-20261022 | 현재 공유 Cargo target 경로와 검증 cache |

다른 프로젝트인 deppy/deppy-mux/deppy-worktrees, `/private/tmp` 작업 트리와 SSH fixture는 정리 대상으로 삼지 않았다. 실행 중 프로세스를 강제 종료하지 않았다.

## 로컬 복구

Private backup: `/Users/jr/Library/Application Support/Deppy Sijo/maintenance/2026-10-24-worktree-cleanup/`.

모든 Git ref의 검증된 `repository.bundle`, 폴더별 `source.tar.gz`, SHA256 manifest, staged/unstaged binary patch, status와 결과 JSON, `RESTORE.md`를 보존했다. 이 백업은 로컬에만 있으며 GitHub에 올리지 않았다. 최상위0700/archive0600 권한을 적용했다.

정리 전후 관찰한 파일시스템 가용 공간 증가는 약 69.4 GB다. APFS 공유·동시 시스템 작업이 포함된 관찰값으로, 폴더별 du 합을 실제 회수 용량으로 보고하지 않는다.
