use deppy_core::MuxPaneId;

/// 설계문서 3장 FocusManager. 현재 포커스된 pane 추적 —
/// MuxTab.active_pane은 tab별 기억, 이것은 지금 입력이 가는 곳.
#[derive(Default)]
pub struct FocusManager {
    focused: Option<MuxPaneId>,
}

impl FocusManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn focus(&mut self, pane: MuxPaneId) {
        self.focused = Some(pane);
    }

    pub fn focused(&self) -> Option<&MuxPaneId> {
        self.focused.as_ref()
    }

    /// 포커스가 존재하는 pane 목록 밖이면 첫 pane으로 보정한다 (pane 닫힘 대응).
    pub fn ensure_valid(&mut self, panes: &[MuxPaneId]) {
        let valid = self.focused.as_ref().is_some_and(|f| panes.contains(f));
        if !valid {
            self.focused = panes.first().cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 포커스_보정() {
        let (a, b) = (MuxPaneId::new(), MuxPaneId::new());
        let mut focus = FocusManager::new();
        assert_eq!(focus.focused(), None);

        focus.focus(b.clone());
        focus.ensure_valid(&[a.clone(), b.clone()]);
        assert_eq!(focus.focused(), Some(&b));

        // b가 닫힘 → 첫 pane으로 보정
        focus.ensure_valid(std::slice::from_ref(&a));
        assert_eq!(focus.focused(), Some(&a));

        // pane이 없으면 None
        focus.ensure_valid(&[]);
        assert_eq!(focus.focused(), None);
    }
}
