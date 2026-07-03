use deppy_core::MuxPaneId;

/// 설계문서 5.3 LayoutTree. split 구조의 source of truth.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutNode {
    Pane(MuxPaneId),
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// remove_pane 결과.
#[derive(Debug, PartialEq)]
pub enum RemovePane {
    /// 제거됨 — 형제 노드가 자리를 승계했다
    Removed,
    /// 트리에 이 pane 하나뿐 — 트리를 비울지는 호출측(tab 닫기)이 결정한다
    LastPane,
    NotFound,
}

impl LayoutNode {
    /// target pane 자리를 Split으로 바꾸고 new_pane을 두 번째 칸에 넣는다.
    pub fn split_pane(
        &mut self,
        target: &MuxPaneId,
        direction: SplitDirection,
        new_pane: MuxPaneId,
    ) -> bool {
        match self {
            LayoutNode::Pane(id) if id == target => {
                *self = LayoutNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Pane(target.clone())),
                    second: Box::new(LayoutNode::Pane(new_pane)),
                };
                true
            }
            LayoutNode::Pane(_) => false,
            LayoutNode::Split { first, second, .. } => {
                first.split_pane(target, direction, new_pane.clone())
                    || second.split_pane(target, direction, new_pane)
            }
        }
    }

    /// target pane을 제거한다. 부모 Split은 형제 노드로 붕괴한다.
    pub fn remove_pane(&mut self, target: &MuxPaneId) -> RemovePane {
        match self {
            LayoutNode::Pane(id) if id == target => RemovePane::LastPane,
            LayoutNode::Pane(_) => RemovePane::NotFound,
            LayoutNode::Split { first, second, .. } => {
                if matches!(first.as_ref(), LayoutNode::Pane(id) if id == target) {
                    *self = std::mem::replace(second.as_mut(), LayoutNode::Pane(target.clone()));
                    return RemovePane::Removed;
                }
                if matches!(second.as_ref(), LayoutNode::Pane(id) if id == target) {
                    *self = std::mem::replace(first.as_mut(), LayoutNode::Pane(target.clone()));
                    return RemovePane::Removed;
                }
                match first.remove_pane(target) {
                    RemovePane::NotFound => second.remove_pane(target),
                    found => found,
                }
            }
        }
    }

    /// 좌→우 DFS 순서의 pane 목록.
    pub fn panes(&self) -> Vec<MuxPaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<MuxPaneId>) {
        match self {
            LayoutNode::Pane(id) => out.push(id.clone()),
            LayoutNode::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }

    pub fn contains(&self, target: &MuxPaneId) -> bool {
        match self {
            LayoutNode::Pane(id) => id == target,
            LayoutNode::Split { first, second, .. } => {
                first.contains(target) || second.contains(target)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> MuxPaneId {
        MuxPaneId::new()
    }

    #[test]
    fn split과_panes_순서() {
        let (a, b, c) = (pane(), pane(), pane());
        let mut layout = LayoutNode::Pane(a.clone());
        assert!(layout.split_pane(&a, SplitDirection::Horizontal, b.clone()));
        assert!(layout.split_pane(&b, SplitDirection::Vertical, c.clone()));
        assert_eq!(layout.panes(), vec![a.clone(), b.clone(), c.clone()]);
        // 없는 pane split은 실패
        assert!(!layout.split_pane(&pane(), SplitDirection::Horizontal, pane()));
    }

    #[test]
    fn remove시_형제가_승계() {
        let (a, b, c) = (pane(), pane(), pane());
        let mut layout = LayoutNode::Pane(a.clone());
        layout.split_pane(&a, SplitDirection::Horizontal, b.clone());
        layout.split_pane(&b, SplitDirection::Vertical, c.clone());

        // 중첩 split 안의 b 제거 → c가 자리 승계
        assert_eq!(layout.remove_pane(&b), RemovePane::Removed);
        assert_eq!(layout.panes(), vec![a.clone(), c.clone()]);
        // a 제거 → 루트가 c 단일 pane으로 붕괴
        assert_eq!(layout.remove_pane(&a), RemovePane::Removed);
        assert_eq!(layout, LayoutNode::Pane(c.clone()));
        // 마지막 pane
        assert_eq!(layout.remove_pane(&c), RemovePane::LastPane);
        assert_eq!(layout.remove_pane(&pane()), RemovePane::NotFound);
    }

    #[test]
    fn contains() {
        let (a, b) = (pane(), pane());
        let mut layout = LayoutNode::Pane(a.clone());
        layout.split_pane(&a, SplitDirection::Vertical, b.clone());
        assert!(layout.contains(&a));
        assert!(layout.contains(&b));
        assert!(!layout.contains(&pane()));
    }
}
