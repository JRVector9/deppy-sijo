//! Child of completion_bridge so retention checks need no public test seam.
use super::super::completion_integration_tests::Fixture;
use super::*;
use std::time::{Duration, Instant};

#[test]
fn completion_retention_prunes_terminal_statuses_without_revoking_active_watch() {
    let mut fixture = Fixture::new();
    let started = fixture.start("retained-active", None);
    let permit = fixture.bridge.completion_bridge.pending[&started.internal]
        .permit
        .clone();
    assert!(permit.is_live());
    for index in 0..1000 {
        let mut identity = started.identity.clone();
        identity.scope.operation_id = format!("retained-terminal-{index}");
        fixture.bridge.completion_bridge.statuses.push_back(Status {
            identity,
            permit: CompletionPermit::new(),
            view: CompletionStatusView {
                local: LocalCompletionState::Completed,
                delivery: CompletionDeliveryState::ReturnedUnacknowledged,
            },
            at: now(),
        });
    }

    fixture.bridge.prune_completion_statuses();

    assert!(
        permit.is_live(),
        "UI terminal-history pressure cannot revoke an active original request"
    );
    assert!(
        fixture
            .bridge
            .completion_bridge
            .pending
            .contains_key(&started.internal)
    );
    assert!(
        fixture
            .bridge
            .completion_bridge
            .statuses
            .iter()
            .any(|status| {
                status.identity == started.identity
                    && status.view.local == LocalCompletionState::AwaitingStart
            }),
        "the active watch must retain its separately reported status"
    );
    assert!(fixture.bridge.completion_bridge.statuses.len() <= 1000);
}

#[test]
fn completion_retention_does_not_unlock_final_admission_for_late_event_or_abandon() {
    for abandon in [false, true] {
        let mut fixture = Fixture::new();
        let mut started = fixture.start("retained-timeout", None);
        fixture
            .bridge
            .pending
            .get_mut(&started.internal)
            .unwrap()
            .deadline = Instant::now() - Duration::from_secs(1);
        fixture.bridge.expire_pending();
        let response = fixture.finish(&mut started);
        assert_eq!(response["local_completion"]["status"], "unknown");
        let stored = fixture.receipt(&started);
        assert_eq!(stored.admission["status"], "unknown");
        assert_eq!(fixture.public_admission(&started), stored.admission);
        assert!(
            !fixture
                .bridge
                .completion_bridge
                .pending
                .contains_key(&started.internal)
        );
        assert!(fixture.bridge.pending.contains_key(&started.internal));

        // UI status retention may end before the outstanding runtime admission.
        // It must not be the authority for the operation's final admission lock.
        fixture.bridge.completion_bridge.statuses.clear();
        if abandon {
            fixture
                .bridge
                .pending
                .get_mut(&started.internal)
                .unwrap()
                .deadline = Instant::now() - Duration::from_secs(31);
            fixture.bridge.expire_pending();
            fixture.bridge.drain_history(|_, _| {
                panic!("retiring an outstanding admission cannot type again")
            });
        } else {
            fixture.admit(&started, Ok(()));
        }

        assert_eq!(fixture.receipt(&started), stored);
        assert_eq!(
            fixture.public_admission(&started),
            stored.admission,
            "pruning display status cannot unlock a final admission (abandon={abandon})"
        );
    }
}
