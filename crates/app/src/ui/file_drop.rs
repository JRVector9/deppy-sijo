//! 파일 트리 드롭 대상 판정 — 순수 로직.
//!
//! 트리에는 **순서가 없다**(정렬 표시). 그래서 "A와 B 사이에 끼워 넣는다"는 자리는
//! 존재하지 않고, 의미 있는 드롭은 언제나 **"어느 폴더로 들어가느냐"** 하나뿐이다.
//! 다만 그 폴더를 가리키는 방법이 둘이라 표시도 둘로 갈린다(Finder와 같다):
//!
//! - 폴더 행 **가운데** → 그 폴더 안으로. 행 전체를 면으로 강조한다.
//! - 폴더 행 **가장자리**, 또는 파일 행 위 → 그 행의 **부모 폴더**로. 행 사이에
//!   삽입선을 그린다. "여기 위치의 폴더로 들어간다"를 위치로 말한다.
//!
//! 렌더와 분리한 이유: 밴드 경계·자기 자신 드롭 금지 같은 판정은 화면 없이 표로
//! 고정할 수 있고, 그래야 회귀를 잡을 수 있다.

use std::path::Path;

/// 폴더 행에서 "안으로 들어간다"로 判定되는 세로 밴드의 비율(행 높이 기준).
/// 위아래 각각 30%는 삽입선 영역이고 가운데 40%가 폴더 진입이다. Finder도 가장자리를
/// 좁게 잡는다 — 폴더에 넣는 게 주 동작이라 가운데를 넉넉히 준다.
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
pub fn can_drop_into(dragged: &Path, target: &Path) -> bool {
    if dragged == target {
        return false;
    }
    if dragged.parent() == Some(target) {
        return false;
    }
    !target.starts_with(dragged)
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

    /// 폴더를 자기 자손 위로 끌어도 진입/삽입선 어느 쪽도 뜨지 않아야 한다.
    #[test]
    fn 자손_행_위에서는_아무_표시도_없다() {
        let row = dir("/repo/src/ui", 100.0, 120.0);
        assert_eq!(target(&row, true, 110.0, "/repo/src"), None); // 진입 금지
        assert_eq!(target(&row, true, 101.0, "/repo/src"), None); // 삽입선도 부모가 /repo/src
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
}
