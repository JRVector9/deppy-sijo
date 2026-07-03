use deppy_core::{MuxPaneId, MuxTabId};

use crate::layout_tree::{LayoutNode, RemovePane, SplitDirection};

/// 설계문서 5.2 MuxTab. layout(5.3)이 pane 배치의 source of truth.
pub struct MuxTab {
    pub id: MuxTabId,
    pub title: String,
    pub layout: LayoutNode,
    pub active_pane: Option<MuxPaneId>,
}

/// close_pane 결과.
#[derive(Debug, PartialEq)]
pub enum ClosePane {
    Closed,
    /// 마지막 pane — tab 자체를 닫아야 한다 (호출측 결정)
    LastPane,
    NotFound,
}

impl MuxTab {
    pub fn new(id: MuxTabId, title: String, root_pane: MuxPaneId) -> Self {
        Self {
            id,
            title,
            layout: LayoutNode::Pane(root_pane.clone()),
            active_pane: Some(root_pane),
        }
    }

    /// target을 분할해 new_pane을 만들고 활성화한다.
    pub fn split_pane(
        &mut self,
        target: &MuxPaneId,
        direction: SplitDirection,
        new_pane: MuxPaneId,
    ) -> bool {
        let split = self.layout.split_pane(target, direction, new_pane.clone());
        if split {
            self.active_pane = Some(new_pane);
        }
        split
    }

    /// target을 닫는다. 활성 pane이었다면 남은 첫 pane으로 넘어간다.
    pub fn close_pane(&mut self, target: &MuxPaneId) -> ClosePane {
        match self.layout.remove_pane(target) {
            RemovePane::Removed => {
                // 닫은 pane이 active였거나, (방어) active가 이미 layout 밖이면 보정
                let active_valid = self
                    .active_pane
                    .as_ref()
                    .is_some_and(|active| active != target && self.layout.contains(active));
                if !active_valid {
                    self.active_pane = self.layout.panes().into_iter().next();
                }
                ClosePane::Closed
            }
            RemovePane::LastPane => ClosePane::LastPane,
            RemovePane::NotFound => ClosePane::NotFound,
        }
    }

    pub fn panes(&self) -> Vec<MuxPaneId> {
        self.layout.panes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_close_활성_전환() {
        let (a, b) = (MuxPaneId::new(), MuxPaneId::new());
        let mut tab = MuxTab::new(MuxTabId::new(), "탭".into(), a.clone());
        assert_eq!(tab.active_pane, Some(a.clone()));

        assert!(tab.split_pane(&a, SplitDirection::Horizontal, b.clone()));
        assert_eq!(tab.active_pane, Some(b.clone()));
        assert_eq!(tab.panes(), vec![a.clone(), b.clone()]);

        // 활성 pane 닫기 → 남은 pane으로 전환
        assert_eq!(tab.close_pane(&b), ClosePane::Closed);
        assert_eq!(tab.active_pane, Some(a.clone()));
        // 마지막 pane은 tab 닫기 신호
        assert_eq!(tab.close_pane(&a), ClosePane::LastPane);
        assert_eq!(tab.close_pane(&MuxPaneId::new()), ClosePane::NotFound);
    }

    #[test]
    fn 비활성_pane_닫기는_활성_유지() {
        let (a, b) = (MuxPaneId::new(), MuxPaneId::new());
        let mut tab = MuxTab::new(MuxTabId::new(), "탭".into(), a.clone());
        tab.split_pane(&a, SplitDirection::Vertical, b.clone());
        assert_eq!(tab.close_pane(&a), ClosePane::Closed);
        assert_eq!(tab.active_pane, Some(b));
    }
}
