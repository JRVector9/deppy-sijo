# 폴더 트리(파일 탐색기) 사이드바 설계

작성: 2026-07-04. 상태: 설계(구현 전). 대상: workspace 좌측 파일 트리 + 마우스 드래그 파일 이동.

## 1. 목표 / 비목표

**목표**
- workspace 루트(`workspaces.path`) 기준 폴더/파일 트리 탐색 (lazy, 가상화)
- **마우스 드래그로 파일/폴더 이동** (트리 내 폴더 간)
- 기본 파일 조작: 새 폴더, 이름 변경, 삭제(확인), 경로 복사, 활성 터미널에 경로 삽입
- 리소스 불변: idle CPU +0%, 메모리 +1~3MB (실측 84MB 기준 — cmux 196MB 대비 우위 유지)

**비목표 (후속)**
- 파일 내용 미리보기/편집기, 다중 선택 드래그, 외부 앱과의 드래그(파인더↔앱), git 상태 배지,
  remote workspace 트리(원격이면 파일 IO를 RuntimeCommand로 승격해야 — 확장점만 남김)

## 2. 아키텍처 배치

- **`crates/app/src/ui/file_tree.rs`** (신규, UI 전용) — v2.5 목표 트리의 `workspace_sidebar.rs` 자리.
- 파일 IO는 **app이 `std::fs` 직접** 사용. v2.5 "UI는 RuntimeClient만 본다"는 **런타임 상태**(세션/mux)에
  대한 경계이고, 로컬 파일시스템은 config/DB처럼 앱 소관(설정 저장·.mcp.json 생성과 동일 부류).
  remote workspace가 생기면 이 모듈의 IO 함수를 trait로 추상화해 RuntimeCommand 백엔드로 교체(확장점).
- **FT-0 선행**: `WorkspaceRow`에 `path` 노출(컬럼은 이미 존재 — SELECT/구조체만 확장). App이 활성
  workspace 전환 시 트리 루트 갱신.

## 3. 데이터 모델 / 성능 (리소스 조건 3원칙)

```rust
struct TreeNode { name: String, is_dir: bool, children: Option<Vec<TreeNode>> /*None=미로딩*/ }
struct FileTreeUi { root: PathBuf, tree: TreeNode, flat: Vec<RowRef> /*가시 행 평탄화 캐시*/,
                    show_hidden: bool, drag: Option<PathBuf>, rename/confirm 상태… }
```
1. **Lazy 로딩**: 펼치는 디렉터리만 `read_dir` (재귀 전체 스캔 금지). 정렬 = 디렉터리 우선 + 이름
   (한글은 로케일 비교 대신 단순 유니코드 순 — 단순성). 숨김(`.`) 토글.
2. **가상화**: 펼친 트리를 평탄화한 `flat` 행 리스트로 유지, `ScrollArea::show_rows`로 보이는 행만 렌더.
   flat 재계산은 펼침/접힘/조작 시에만.
3. **IO는 상호작용 시점만**: 자동 워처 없음(MVP). 수동 새로고침 버튼 + 파일 조작 후 해당 부모만 재나열.
   FSEvents(notify crate) 실시간 갱신은 FT-4 선택(스레드 +1, idle ~0).

메모리 상한: 캐시는 "펼친 노드"만 — 실사용 수백~수천 엔트리(≪1MB). 대형 디렉터리(수만 파일)도
그 한 디렉터리 나열분만 유지.

## 4. 드래그&드롭 이동 (핵심)

- egui 0.35 내장 DnD: 파일/폴더 행 = `dnd_drag_source(Id, payload=PathBuf)`, 폴더 행+루트 영역 =
  `dnd_drop_zone`. 드래그 중 고스트 라벨 + hover된 드롭 대상 폴더 하이라이트.
- **드롭 처리 규칙**
  - 이동 = `std::fs::rename(src, dst_dir.join(file_name))`. 크로스 볼륨(EXDEV) → 재귀 copy+delete 폴백
    (실패 시 부분 정리 후 에러 표면화 — 원본 보존 우선: copy 전부 성공 후에만 delete).
  - **가드**: 자기 자신/자기 자손으로 이동 금지(경로 prefix 검사, canonicalize 후), 같은 부모로 no-op,
    **workspace 루트 밖으로 이동 금지**(dst가 root 하위인지 canonicalize 검증 — 심볼릭 링크 탈출 차단).
  - 대상에 같은 이름 존재 → **덮어쓰기 금지**, 인라인 확인("이름 변경/취소" — MVP는 취소만이어도 가능,
    자동 " (2)" 접미는 후속).
  - 조작 후: src 부모·dst 부모만 재나열 + flat 재계산.
- 실패는 사이드바 하단 에러 라벨(빨간) + tracing::warn.

## 5. 안전 (파괴적 조작)

- 삭제: 기본 **휴지통 이동**(`trash` crate — 크로스 플랫폼) — 영구삭제 아님. 휴지통 실패 시에만
  확인 다이얼로그 후 영구삭제. 디렉터리 삭제는 항목 수 표시.
- 이름 변경/새 폴더: 인라인 편집, 경로 구분자/빈 이름 거부.
- 모든 조작은 UI 스레드 동기 `std::fs`(로컬 SSD 전제 — ms 단위). 네트워크 마운트에서 느릴 수 있음은
  알려진 한계(후속: 큰 copy만 백그라운드 스레드+진행 표시).

## 6. UI 배치

- 좌측 사이드바(기본 240px, 접기 버튼, egui `SidePanel::left` resizable). 헤더: workspace 이름 +
  새로고침/숨김토글. 트리 행: 들여쓰기 + ▸/▾ + 이름(파일은 아이콘 없이 이름만 — 외부 아이콘 의존 금지).
- 컨텍스트 메뉴(우클릭): 새 폴더 / 이름 변경 / 삭제 / 경로 복사 / 터미널에 경로 붙여넣기(활성 세션에
  `RuntimeCommand::WriteInput`으로 경로 전송 — 유일한 runtime 접점).

## 7. PR 분할 (각 단계 독립 가치·검증)

| PR | 내용 | 검증 |
|---|---|---|
| FT-0 | WorkspaceRow.path 노출 | storage 테스트 |
| FT-1 | 읽기전용 트리 (lazy+가상화+새로고침+숨김토글+사이드바) | flat 평탄화/정렬 단위 테스트, frame p95 전후 비교(DEPPY_FRAME_STATS) |
| FT-2 | DnD 이동 (가드 4종 + EXDEV 폴백 + 충돌 처리) | 이동 가드/폴백 단위 테스트(tempdir), 수동 스모크 |
| FT-3 | 컨텍스트 메뉴 (새폴더/이름변경/휴지통 삭제/경로) | tempdir 단위 테스트 |
| FT-4(선택) | notify(FSEvents) 실시간 갱신 | 워처 이벤트→부분 재나열 테스트 |

## 8. 리스크

- canonicalize 기반 루트 탈출 검사: 심볼릭 링크가 루트 밖을 가리키는 경우의 UX(표시는 하되 이동 대상
  제한) — FT-2에서 구체화.
- egui DnD와 기존 pane 클릭 포커스의 상호작용(드래그 시작 임계값으로 클릭과 구분 — egui 기본 제공).
- 대형 디렉터리 read_dir 블로킹(수만 엔트리 ~수십 ms) — 허용, 그 이상이면 후속에 백그라운드 나열.

---

## 9. codex-exec 5.5 xhigh 교차검토 반영 (2026-07-04)

실코드·egui 0.35 소스 대조 검토 결과(보완필요 → 아래 반영으로 해소):

1. **[P1] API 정정**: `SidePanel::left`는 egui 0.35에 없음 — **`egui::Panel::left`** 사용(기존 코드의 `Panel::top`과 동일 계열). CentralPanel **앞**에 추가(egui 권장 순서). 기존 `workspaces_window`가 CentralPanel 앞에서 호출되는 순서도 이때 함께 정리.
2. **[P1] FT-0 범위 확대**: `workspaces.path`는 스키마에 있으나 **기본/신규 workspace가 `path=''`로 생성**됨. WorkspaceRow.path 노출만으론 부족 — (a) workspace 생성/편집 UI에 **경로 입력·수정**(폴더 선택), (b) 기존 행은 빈 path 유지 시 **트리 숨김+"프로젝트 경로를 설정하세요" 안내**(backfill 강제 없음), (c) invalid/missing root는 에러 라벨+트리 비활성.
3. **[P1] IO 스레딩 재조정**: 동기 허용은 **FT-1의 lazy `read_dir`만**. FT-2/3의 EXDEV 재귀 copy·휴지통 이동·디렉터리 항목 수 계산은 **백그라운드 스레드 + 완료/에러를 UI에 전달**(egui 프레임 블로킹 시 활성 터미널까지 멈춤 — WorkspaceUi::show와 같은 프레임).
4. **[P1] DnD 가드 구체화**: dst는 아직 없으므로 **root·src·dst_dir를 canonicalize**해 `dst_dir⊂root`(루트 탈출 차단)·`¬(dst_dir⊂src)`(자손 금지) 검사. **symlink는 따라가지 않고 링크 자체를 이동**(정책 확정). EXDEV 폴백 순서 = `dst_dir/.tmp-<uuid>`에 전체 copy → 최종 이름으로 rename → 성공 후에만 원본 delete(부분 실패 시 tmp 정리, 원본 보존).
5. **[P1] 덮어쓰기 TOCTOU**: `try_exists` 사전검사만으론 경합 창 존재(Unix rename은 덮어씀). **macOS(주 타깃)는 `renamex_np(RENAME_EXCL)`** 사용, 그 외/실패 시 사전검사 폴백 + "단일 사용자 로컬 조작, 외부 동시 변경과의 경합은 비전제" 명시.
6. **[P2] 가상화 조건**: `show_rows`는 고정 행높이 전제 — 행높이 안정화 + **path 기반 explicit `Id`**(재정렬 시 상태 꼬임 방지).
7. **[P2] trash crate**: 의존 추가 필요. macOS/Windows는 안정, Linux는 FreeDesktop 가정의 best-effort — 실패 시 확인 다이얼로그 후 영구삭제 폴백 유지.
8. **[P2] rename 인라인 편집의 포커스**: TextEdit가 터미널 입력 포커스와 경합하지 않게 FT-3에서 별도 처리(편집 중 터미널 키 입력 차단).

검증 확인된 것: `dnd_drag_source`(payload=PathBuf 가능)/`dnd_drop_zone`/`show_rows` 존재·적합, Panel::left와 기존 중앙 레이아웃 비충돌, pane 클릭 포커스와 DnD 직접 충돌 없음.
