use deppy_core::{MuxTabId, MuxWindowId};

/// 설계문서 5.2 MuxWindow. tab 소속/순서의 source of truth.
pub struct MuxWindow {
    pub id: MuxWindowId,
    pub tabs: Vec<MuxTabId>,
    pub active_tab: Option<MuxTabId>,
}

impl MuxWindow {
    pub fn new(id: MuxWindowId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active_tab: None,
        }
    }

    /// tab을 추가하고 활성화한다.
    pub fn add_tab(&mut self, tab: MuxTabId) {
        self.active_tab = Some(tab.clone());
        self.tabs.push(tab);
    }

    /// tab을 제거한다. 활성 tab이었다면 직전 순번(없으면 첫) tab으로 넘어간다.
    pub fn close_tab(&mut self, tab: &MuxTabId) {
        let Some(index) = self.tabs.iter().position(|t| t == tab) else {
            return;
        };
        self.tabs.remove(index);
        if self.active_tab.as_ref() == Some(tab) {
            self.active_tab = self
                .tabs
                .get(index.saturating_sub(1))
                .or_else(|| self.tabs.first())
                .cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_추가와_닫기_활성_전환() {
        let mut window = MuxWindow::new(MuxWindowId::new());
        let (a, b, c) = (MuxTabId::new(), MuxTabId::new(), MuxTabId::new());
        window.add_tab(a.clone());
        window.add_tab(b.clone());
        window.add_tab(c.clone());
        assert_eq!(window.active_tab, Some(c.clone()));

        // 활성 tab을 닫으면 직전 순번으로
        window.close_tab(&c);
        assert_eq!(window.active_tab, Some(b.clone()));
        // 비활성 tab을 닫아도 활성은 유지
        window.close_tab(&a);
        assert_eq!(window.active_tab, Some(b.clone()));
        // 마지막 tab을 닫으면 None
        window.close_tab(&b);
        assert_eq!(window.active_tab, None);
        assert!(window.tabs.is_empty());
    }
}
