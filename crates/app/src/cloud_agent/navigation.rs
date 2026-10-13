//! Correlated native navigation results; admission is not successful activation.
use super::CloudAgent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunNavigation {
    pub request: u64,
    pub workspace: String,
    pub session: String,
}

#[derive(Default)]
pub(super) struct NavigationState {
    next: u64,
    current: Option<(RunNavigation, Option<Result<(), String>>)>,
}

impl CloudAgent {
    pub fn request_run_navigation(
        &mut self,
        workspace: &str,
        session: &str,
    ) -> Result<u64, String> {
        if [workspace, session]
            .iter()
            .any(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err("invalid_session_id".into());
        }
        let request = self
            .navigation_state
            .next
            .checked_add(1)
            .ok_or("navigation_id_exhausted")?;
        self.navigation_state.next = request;
        let navigation = RunNavigation {
            request,
            workspace: workspace.into(),
            session: session.into(),
        };
        self.navigation_state.current = Some((navigation.clone(), None));
        self.native_navigation = Some(navigation);
        Ok(request)
    }

    pub(crate) fn take_run_navigation(&mut self) -> Option<RunNavigation> {
        self.native_navigation.take()
    }

    pub(crate) fn finish_run_navigation(
        &mut self,
        navigation: &RunNavigation,
        result: Result<(), String>,
    ) {
        if let Some((current, outcome)) = &mut self.navigation_state.current
            && current == navigation
            && outcome.is_none()
        {
            *outcome = Some(result);
        }
    }

    pub(super) fn run_navigation_result(&self, request: u64) -> Option<Result<(), String>> {
        match &self.navigation_state.current {
            Some((current, outcome)) if current.request == request => outcome.clone(),
            _ if request > 0 && request <= self.navigation_state.next => {
                Some(Err("navigation_superseded".into()))
            }
            _ => None,
        }
    }
}

/// Only bounded outcome codes cross the transport; internal errors stay native.
pub(crate) fn navigation_outcome(
    result: &Result<(), String>,
) -> web_remote::cloud::CloudNavigationOutcome {
    use web_remote::cloud::CloudNavigationOutcome;
    match result.as_ref().map_err(String::as_str) {
        Ok(()) => CloudNavigationOutcome::Opened,
        Err("session_activation_timed_out") => CloudNavigationOutcome::TimedOut,
        Err("navigation_superseded") => CloudNavigationOutcome::Superseded,
        Err(
            "workspace_activation_rejected"
            | "session_activation_rejected"
            | "navigation_id_exhausted",
        ) => CloudNavigationOutcome::Rejected,
        Err(_) => CloudNavigationOutcome::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_navigation_completion_cannot_overwrite_a_newer_request() {
        let mut cloud = CloudAgent::memory();
        let first = cloud
            .request_run_navigation("workspace", "session")
            .unwrap();
        let obsolete = cloud.take_run_navigation().unwrap();
        let latest = cloud
            .request_run_navigation("workspace", "session")
            .unwrap();
        let current = cloud.take_run_navigation().unwrap();
        cloud.finish_run_navigation(&obsolete, Err("closed".into()));
        assert_eq!(
            cloud.run_navigation_result(first),
            Some(Err("navigation_superseded".into()))
        );
        assert!(cloud.run_navigation_result(latest).is_none());
        cloud.finish_run_navigation(&current, Err("session_changed_or_closed".into()));
        assert_eq!(
            cloud.run_navigation_result(latest),
            Some(Err("session_changed_or_closed".into()))
        );
        cloud.finish_run_navigation(&current, Ok(()));
        assert!(cloud.run_navigation_result(latest).unwrap().is_err());
    }
}
