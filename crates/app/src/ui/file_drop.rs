//! 파일 트리 드롭 대상 판정 — 순수 로직.
//!
//! 트리에는 **순서가 없다**(정렬 표시). 그래서 "A와 B 사이에 끼워 넣는다"는 자리는
//! 존재하지 않고, 의미 있는 드롭은 언제나 **"어느 폴더로 들어가느냐"** 하나뿐이다.
//! 다만 그 폴더를 가리키는 방법이 둘이라 표시도 둘로 갈린다:
//!
//! - 폴더 행 **가운데** → 그 폴더 안으로. 행 전체를 면으로 강조한다.
//! - 폴더 행 **가장자리**, 또는 파일 행 위 → 그 행의 **부모 폴더**로. 행 사이에
//!   삽입선을 그린다. "여기 위치의 폴더로 들어간다"를 위치로 말한다.
//!
//! 렌더와 분리한 이유: 밴드 경계·자기 자신 드롭 금지 같은 판정은 화면 없이 표로
//! 고정할 수 있고, 그래야 회귀를 잡을 수 있다.

use std::path::{Component, Path, PathBuf};

/// 폴더 행에서 "안으로 들어간다"로 判定되는 세로 밴드의 비율(행 높이 기준).
/// 위아래 각각 30%는 삽입선 영역이고 가운데 40%가 폴더 진입이다.
///
/// Finder·VS Code 탐색기는 실은 이 자리를 만들지 않는다 — 둘 다 정렬된 목록이라
/// 폴더 행은 어디를 찍어도 그 폴더 "안"이고, 가장자리 예외가 없다(2026-08-11 검토,
/// VS Code `listView.ts`의 `getTargetSector`/`explorerViewer.ts`의 `handleDragOver`
/// 확인 — `target.isDirectory`면 sector와 무관하게 항상 진입, 가장자리 분기가 있는
/// 건 순서가 있는 워크스페이스 루트 재정렬뿐). 이 앱은 정렬 순서가 없어 "부모 폴더로
/// 보낸다"는 그 방법이 되레 없으므로 가장자리를 일부러 만들어 낸 것 — Finder를
/// 흉내 낸 게 아니라 이 앱만의 타협이다. 30%는 VS Code가 순서 있는 목록에 실제로
/// 쓰는 4분할(25%×4) 눈금과 크기가 비슷해 임의값은 아니지만, 폴더 진입에서 가장자리를
/// 떼어내는 선례 자체가 없다는 점은 분명히 해 둔다. 기본 행높이(25px, `file_tree.rs`
/// `measured_row_height` 기본값) 기준 가장자리 7.5px는 리사이즈 손잡이 수준의 폭이라
/// 마우스로 조준 가능하다고 판단해 유지한다.
const EDGE_BAND: f32 = 0.30;

/// 판정에 필요한 행 정보. 트리 렌더 루프가 이미 들고 있는 값들이다.
#[derive(Clone, Copy, Debug)]
pub struct RowInfo<'a> {
    pub path: &'a Path,
    pub is_dir: bool,
    /// 행의 세로 범위. 가로는 판정에 쓰지 않는다(행 전체가 대상).
    pub top: f32,
    pub bottom: f32,
}

/// 이 행에서 판정된 드롭 대상.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowDropTarget {
    /// 이 폴더 안으로. 행을 면으로 강조한다.
    IntoFolder,
    /// 이 행의 부모 폴더로. 행 **위쪽** 경계에 삽입선을 그린다.
    InsertAbove,
    /// 이 행의 부모 폴더로. 행 **아래쪽** 경계에 삽입선을 그린다.
    InsertBelow,
}

/// 드래그 중인 경로를 이 행에 떨어뜨릴 수 있는지, 있다면 어떤 형태인지.
///
/// `None`인 경우:
/// - 포인터가 행 밖
/// - 자기 자신에게 떨어뜨리려는 경우
/// - 폴더를 **자기 자손** 안으로 넣으려는 경우(디렉터리 순환)
/// - 부모가 없는 행(루트)에 삽입선을 그리려는 경우
pub fn row_drop_target(row: RowInfo<'_>, pointer_y: f32, dragged: &Path) -> Option<RowDropTarget> {
    if pointer_y < row.top || pointer_y >= row.bottom {
        return None;
    }
    let height = row.bottom - row.top;
    if height <= 0.0 {
        return None;
    }

    let offset = (pointer_y - row.top) / height;
    let wants_into = row.is_dir && (EDGE_BAND..1.0 - EDGE_BAND).contains(&offset);

    if wants_into {
        return can_drop_into(dragged, row.path).then_some(RowDropTarget::IntoFolder);
    }

    // 삽입선 = 이 행의 부모 폴더로. 부모가 없으면(루트) 그릴 자리가 없다.
    let parent = row.path.parent()?;
    if !can_drop_into(dragged, parent) {
        return None;
    }
    Some(if offset < 0.5 {
        RowDropTarget::InsertAbove
    } else {
        RowDropTarget::InsertBelow
    })
}

/// `dragged`를 `target` 폴더 안으로 옮길 수 있나.
///
/// 막는 경우 셋:
/// - 자기 자신을 자기 안으로
/// - **이미 그 폴더 안에 있는 것**을 같은 폴더로 (이동이 아무 일도 안 한다)
/// - 폴더를 자기 자손 안으로 (디렉터리가 자기를 삼킨다)
///
/// 비교 전에 `.`/`..`을 컴포넌트 단위로 걷어낸다 — `Path::starts_with`·`parent()`는
/// 리터럴 컴포넌트만 보므로 `/repo/../repo/src`처럼 정규화 안 된 입력이 오면 실제로는
/// `/repo/src`인데도 "이미 그 폴더 안"을 놓친다. 트리에서 오는 경로는 항상 정규화돼
/// 있지만(§set_root 주석), 이 함수는 공개 API라 방어적으로 정규화한다.
///
/// symlink는 못 따라간다(파일시스템을 안 읽는 순수 함수라서다) — 그래서 이 함수는
/// UI 판정이지 보안 경계가 아니다. symlink로 우회한 순환은 host의 `app_host_move`가
/// canonicalize로 최종 차단한다.
pub fn can_drop_into(dragged: &Path, target: &Path) -> bool {
    let dragged = normalize_lexically(dragged);
    let target = normalize_lexically(target);
    if dragged == target {
        return false;
    }
    if dragged.parent() == Some(target.as_path()) {
        return false;
    }
    !target.starts_with(&dragged)
}

/// `.`/`..`을 파일시스템 접근 없이 컴포넌트 단위로 걷어낸다. symlink는 고려하지
/// 않는다(리터럴 경로만 안다) — `can_drop_into`의 문서 참고.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(path: &str, top: f32, bottom: f32) -> (PathBuf, f32, f32) {
        (PathBuf::from(path), top, bottom)
    }

    fn target(
        row: &(PathBuf, f32, f32),
        is_dir: bool,
        pointer_y: f32,
        dragged: &str,
    ) -> Option<RowDropTarget> {
        row_drop_target(
            RowInfo {
                path: &row.0,
                is_dir,
                top: row.1,
                bottom: row.2,
            },
            pointer_y,
            Path::new(dragged),
        )
    }

    /// 폴더 행은 가운데 40%가 「안으로」, 위아래 30%씩이 삽입선이다.
    /// 경계값을 표로 고정한다 — 밴드가 흔들리면 폴더에 넣으려다 옆 폴더로 간다.
    #[test]
    fn 폴더_행은_가운데가_진입_가장자리가_삽입선() {
        let row = dir("/repo/src", 100.0, 120.0); // 높이 20 → 밴드 경계 106.0 / 114.0
        // 다른 폴더에서 끌어온다. `/repo/a.txt`처럼 이미 부모(`/repo`) 안에 있는 것을
        // 쓰면 삽입선 쪽이 "무의미한 이동"으로 막혀 밴드 경계를 못 잰다(설계대로다).
        let drag = "/other/a.txt";

        assert_eq!(
            target(&row, true, 100.0, drag),
            Some(RowDropTarget::InsertAbove)
        );
        assert_eq!(
            target(&row, true, 105.9, drag),
            Some(RowDropTarget::InsertAbove)
        );
        assert_eq!(
            target(&row, true, 106.0, drag),
            Some(RowDropTarget::IntoFolder)
        );
        assert_eq!(
            target(&row, true, 113.9, drag),
            Some(RowDropTarget::IntoFolder)
        );
        assert_eq!(
            target(&row, true, 114.0, drag),
            Some(RowDropTarget::InsertBelow)
        );
        assert_eq!(
            target(&row, true, 119.9, drag),
            Some(RowDropTarget::InsertBelow)
        );
    }

    /// 파일 행은 어디를 짚어도 삽입선이다 — 파일 안으로 들어갈 수는 없다.
    #[test]
    fn 파일_행은_언제나_삽입선() {
        let row = dir("/repo/src/main.rs", 100.0, 120.0);
        let drag = "/other/a.txt";
        assert_eq!(
            target(&row, false, 101.0, drag),
            Some(RowDropTarget::InsertAbove)
        );
        assert_eq!(
            target(&row, false, 110.0, drag),
            Some(RowDropTarget::InsertBelow)
        );
        assert_eq!(
            target(&row, false, 119.0, drag),
            Some(RowDropTarget::InsertBelow)
        );
    }

    /// 행 밖이면 판정하지 않는다. bottom은 배타적이라 인접 행이 겹쳐 잡히지 않는다.
    #[test]
    fn 행_밖은_대상이_아니다() {
        let row = dir("/repo/src", 100.0, 120.0);
        assert_eq!(target(&row, true, 99.9, "/other/a.txt"), None);
        assert_eq!(target(&row, true, 120.0, "/other/a.txt"), None);
    }

    /// 자기 자신·이미 있는 자리·자기 자손으로는 못 옮긴다.
    #[test]
    fn 무의미하거나_순환하는_이동은_막는다() {
        // 자기 자신
        assert!(!can_drop_into(
            Path::new("/repo/src"),
            Path::new("/repo/src")
        ));
        // 이미 그 폴더 안에 있다 — 옮겨도 아무 일이 없다
        assert!(!can_drop_into(
            Path::new("/repo/src/main.rs"),
            Path::new("/repo/src")
        ));
        // 폴더를 자기 자손 안으로 — 디렉터리가 자기를 삼킨다
        assert!(!can_drop_into(
            Path::new("/repo/src"),
            Path::new("/repo/src/ui")
        ));
        // 정상
        assert!(can_drop_into(
            Path::new("/repo/a.txt"),
            Path::new("/repo/src")
        ));
    }

    /// `starts_with`는 문자열이 아니라 경로 컴포넌트 단위로 비교한다 — 이름이
    /// 겹치는 형제("src"와 "src-backup")를 조상/자손 관계로 오판하면 안 된다.
    /// (문자열 prefix로 잘못 구현했다면 이 테스트가 잡아낸다.)
    #[test]
    fn 이름이_겹치는_형제는_조상_자손_관계가_아니다() {
        assert!(can_drop_into(
            Path::new("/repo/src"),
            Path::new("/repo/src-backup")
        ));
        assert!(can_drop_into(
            Path::new("/repo/src-backup"),
            Path::new("/repo/src")
        ));
    }

    /// 정규화 안 된 입력(리터럴 `..`)이 들어오면 `starts_with`/`parent()`가 컴포넌트를
    /// 그대로 비교해 "이미 그 폴더 안"을 놓칠 수 있다 — `/repo/../repo/src`는 실제로
    /// `/repo/src`이고 그 부모는 `/repo`이므로 막혀야 한다.
    #[test]
    fn 리터럴_dotdot이_있어도_이미_그_폴더_안이면_막는다() {
        assert!(!can_drop_into(
            Path::new("/repo/../repo/src"),
            Path::new("/repo")
        ));
    }

    /// 리터럴 `.`은 Rust `Path`가 컴포넌트 비교에서 이미 걸러낸다(`..`과 달리
    /// 정규화가 필요 없다) — 회귀 방지로 고정해 둔다.
    #[test]
    fn 리터럴_dot은_원래도_안전하다() {
        assert!(!can_drop_into(
            Path::new("/repo/./src"),
            Path::new("/repo/src")
        ));
    }

    /// 폴더를 자기 자손 위로 끌어도 진입/삽입선 어느 쪽도 뜨지 않아야 한다.
    #[test]
    fn 자손_행_위에서는_아무_표시도_없다() {
        let row = dir("/repo/src/ui", 100.0, 120.0);
        assert_eq!(target(&row, true, 110.0, "/repo/src"), None); // 진입 금지
        assert_eq!(target(&row, true, 101.0, "/repo/src"), None); // 삽입선도 부모가 /repo/src
    }

    /// 자손 폴더 "안의 파일 행" 위에서도 마찬가지다 — 파일 행은 언제나 삽입선이고
    /// 그 부모는 dragged 자신이라, "이미 그 폴더 안" 규칙에 걸려 막혀야 한다.
    #[test]
    fn 자손_폴더_안의_파일_행_위에서도_아무_표시도_없다() {
        let row = dir("/repo/src/main.rs", 100.0, 120.0);
        assert_eq!(target(&row, false, 101.0, "/repo/src"), None);
        assert_eq!(target(&row, false, 119.0, "/repo/src"), None);
    }

    /// 부모가 없는 행(루트)에는 삽입선을 그릴 자리가 없다.
    #[test]
    fn 부모가_없으면_삽입선을_그리지_않는다() {
        let row = dir("/", 100.0, 120.0);
        assert_eq!(target(&row, true, 101.0, "/other/a.txt"), None);
    }

    /// 높이가 0인 행(레이아웃 과도기)에서 0으로 나누지 않는다.
    #[test]
    fn 높이가_0이면_판정하지_않는다() {
        let row = dir("/repo/src", 100.0, 100.0);
        assert_eq!(target(&row, true, 100.0, "/other/a.txt"), None);
    }

    /// top이 bottom보다 큰 행(레이아웃이 뒤집힌 과도기 프레임)도 판정하지 않는다.
    /// top > bottom이면 `top <= pointer_y < bottom`을 만족하는 y가 아예 없어 첫
    /// 가드(행 밖 판정)에서부터 걸린다 — `height <= 0.0` 가드는 top == bottom(0
    /// 높이)에서만 실제로 쓰인다. 그 경계도 명시적으로 고정해 둔다.
    #[test]
    fn top이_bottom보다_크면_판정하지_않는다() {
        let row = dir("/repo/src", 120.0, 100.0);
        assert_eq!(target(&row, true, 110.0, "/other/a.txt"), None);
    }

    /// 실측 행높이(기본 25px)보다 훨씬 작은 극단값(18px — 밀집 모드 가정, EDGE_BAND
    /// 논의에서 언급된 값)에서도 세 구간이 정상 순서로 나오는지 고정해 둔다. 경계
    /// 정확한 픽셀(105.4/112.6 근방)은 f32 반올림에 취약해 피하고, 각 구간 안쪽
    /// 지점만 확인한다 — 정밀 경계 고정은 20px 행 테스트가 이미 한다.
    #[test]
    fn 아주_낮은_행에서도_세_구간_순서가_유지된다() {
        let row = dir("/repo/src", 100.0, 118.0); // 높이 18 → 가장자리 5.4px
        let drag = "/other/a.txt";
        assert_eq!(
            target(&row, true, 101.8, drag), // offset 0.1 — 위쪽 삽입선
            Some(RowDropTarget::InsertAbove)
        );
        assert_eq!(
            target(&row, true, 109.0, drag), // offset 0.5 — 진입
            Some(RowDropTarget::IntoFolder)
        );
        assert_eq!(
            target(&row, true, 116.2, drag), // offset 0.9 — 아래쪽 삽입선
            Some(RowDropTarget::InsertBelow)
        );
    }
}
