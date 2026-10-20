//! 표시명과 무관한 한 프레임의 정확한 실행 대상 핸들.

pub(crate) struct RouteRegistry<T> {
    incarnation: String,
    next: u64,
    entries: Vec<(String, T)>,
}

impl<T: PartialEq> RouteRegistry<T> {
    pub fn new(incarnation: String) -> Self {
        Self {
            incarnation,
            next: 0,
            entries: Vec::new(),
        }
    }

    pub fn refresh(&mut self, targets: Vec<T>) {
        let mut previous = std::mem::take(&mut self.entries);
        for target in targets {
            if self.entries.iter().any(|(_, value)| value == &target) {
                continue;
            }
            let key = if let Some(index) = previous.iter().position(|(_, value)| value == &target) {
                previous.swap_remove(index).0
            } else {
                let Some(next) = self.next.checked_add(1) else {
                    continue;
                };
                self.next = next;
                format!("{}:{next}", self.incarnation)
            };
            self.entries.push((key, target));
        }
    }

    pub fn key(&self, target: &T) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, value)| value == target)
            .map(|(key, _)| key.as_str())
    }

    pub fn get(&self, key: &str) -> Option<&T> {
        self.entries
            .iter()
            .find(|(value, _)| value == key)
            .map(|(_, value)| value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_targets_in_the_same_workspace_have_different_handles() {
        let mut routes = RouteRegistry::new("host-a".into());
        routes.refresh(vec![("workspace", "pane-a", 1), ("workspace", "pane-b", 1)]);
        let first = routes.key(&("workspace", "pane-a", 1)).unwrap();
        let second = routes.key(&("workspace", "pane-b", 1)).unwrap();
        assert_ne!(first, second);
        assert_eq!(routes.get(second), Some(&("workspace", "pane-b", 1)));
    }

    #[test]
    fn replaced_execution_retires_old_handle_even_when_pane_is_the_same() {
        let mut routes = RouteRegistry::new("host-a".into());
        routes.refresh(vec![("workspace", "pane-a", 1)]);
        let old = routes.key(&("workspace", "pane-a", 1)).unwrap().to_owned();
        routes.refresh(vec![("workspace", "pane-a", 2)]);
        let current = routes.key(&("workspace", "pane-a", 2)).unwrap();
        assert_ne!(old, current);
        assert!(routes.get(&old).is_none());
    }

    #[test]
    fn repaint_and_reordering_preserve_unchanged_execution_handle() {
        let mut routes = RouteRegistry::new("host-a".into());
        routes.refresh(vec!["a", "b"]);
        let old = routes.key(&"a").unwrap().to_owned();
        routes.refresh(vec!["b", "a"]);
        assert_eq!(routes.key(&"a"), Some(old.as_str()));
        routes.refresh(Vec::new());
        assert!(routes.get(&old).is_none());
        routes.refresh(vec!["a"]);
        assert_ne!(routes.key(&"a"), Some(old.as_str()));
    }
}
