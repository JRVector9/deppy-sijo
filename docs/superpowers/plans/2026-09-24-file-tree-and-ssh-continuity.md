# SSH 화면 연속성과 파일 트리 작업 계획

## 목표와 PR 경계

현재 v43 브랜치 `fix/environment-api-context-integration` 위에 리뷰 가능한 PR을 쌓는다. 기존 dirty 작업 트리는 수정하지 않는다. Deppy 앱은 사용자 명시 요청 없이는 실행하거나 재실행하지 않는다.

1. `fix/ssh-screen-stays-visible`: SSH에서 보던 원격 화면을 재시작 후 로컬 셸로 바뀌어도 첫 입력 전까지 표시한다. alt screen과 primary screen을 모두 다룬다. 새 셸 입력 시 라이브 화면으로 전환한다. session 및 runtime 회귀 테스트를 실행한다.
2. `feat/file-tree-typeahead`: 파일 트리에 포커스가 있을 때 입력한 접두어와 일치하는 **현재 표시된 폴더 행**을 선택하고 화면에 보이게 스크롤한다. 폴더를 열거나 루트를 변경하지 않는다. 연속 입력, 시간 초과, 반복 글자 순환, 입력 필드 포커스 분리를 검증한다.
3. `feat/file-tree-search`: 더보기 메뉴에 파일 검색을 추가한다. 현재 트리의 프로젝트 범위에서 파일과 폴더 이름을 검색하고, 결과에서 해당 경로를 찾을 수 있게 한다. 숨김 파일 정책, 검색 취소, 대량 트리에서 UI 정지를 검증한다.
4. `feat/file-tree-create-dialog`: 새 파일/새 폴더의 좁은 인라인 입력을 모달로 교체한다. 기존 CreateFile/CreateFolder IO 요청과 검증을 재사용한다. 중복 이름, 취소, Enter/Escape, 생성 후 선택을 검증한다.

## 완료 기준

- 각 PR은 독립적인 커밋과 PR 설명, 해당 단계의 테스트 결과를 갖는다.
- 최종 브랜치에서 전체 빌드, 관련 테스트, 포맷 및 변경사항 검사를 마친다.
- 최종 코드 리뷰 결과를 반영하고 남은 한계를 사용자에게 보고한다.
- `docs/CODEX_HANDOFF.md`를 각 의미 있는 단계마다 갱신한다.
