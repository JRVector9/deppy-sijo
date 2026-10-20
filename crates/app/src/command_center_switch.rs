//! 실행 중인 세션을 건드리지 않는 화면 전환 계약.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LayoutMode {
    #[default]
    Original,
    CommandCenter,
}

#[derive(Debug)]
pub(crate) struct LayoutSwitch {
    pub available: bool,
    pub mode: LayoutMode,
}

impl LayoutSwitch {
    pub fn from_flag(flag: Option<&str>) -> Self {
        let available = flag == Some("1");
        Self {
            available,
            mode: if available {
                LayoutMode::CommandCenter
            } else {
                LayoutMode::Original
            },
        }
    }

    pub fn use_original(&mut self) {
        self.mode = LayoutMode::Original;
    }

    pub fn open(&mut self) {
        if self.available {
            self.mode = LayoutMode::CommandCenter;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_layout_is_the_default_and_unknown_flags_do_not_enable_it() {
        for flag in [None, Some(""), Some("0"), Some("false"), Some("yes")] {
            let mut switch = LayoutSwitch::from_flag(flag);
            switch.open();
            assert!(!switch.available);
            assert_eq!(switch.mode, LayoutMode::Original);
        }
    }

    #[test]
    fn explicit_opt_in_can_return_to_original_and_reopen_without_restart() {
        let mut switch = LayoutSwitch::from_flag(Some("1"));
        assert!(switch.available);
        assert_eq!(switch.mode, LayoutMode::CommandCenter);
        switch.use_original();
        assert_eq!(switch.mode, LayoutMode::Original);
        switch.open();
        assert_eq!(switch.mode, LayoutMode::CommandCenter);
    }
}
