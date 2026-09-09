//! 설정 조회 중 상태와 실제 실패를 구분한다.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotLoadState {
    Loading,
    Ready,
    Failed,
}

impl SnapshotLoadState {
    /// 조회 오류의 수명만 관리한다. 저장·삭제·공개 등 별도 작업 오류는 보존한다.
    pub fn reconcile_error<E: Copy + PartialEq>(self, error: &mut Option<E>, load_error: E) {
        if error.is_some_and(|current| current != load_error) {
            return;
        }
        *error = matches!(self, Self::Failed).then_some(load_error);
    }
}
