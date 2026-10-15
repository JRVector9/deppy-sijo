//! Runtime event loss retires completion evidence before another proof can finalize it.
use super::{completion::LocalCompletionState, completion_integration_tests::Fixture};

#[test]
fn completion_event_loss_retires_admitted_watch_before_later_native_completion() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("event-loss-admitted", None);
    let accepted_at = fixture.admit(&started, Ok(()));
    let completed = fixture.completed_proof(&started, accepted_at);

    fixture
        .bridge
        .invalidate_completion_runtime(fixture.target.runtime);
    fixture.bridge.install_native_turns(vec![completed]);

    let status = fixture
        .bridge
        .completion_status_for_operation(
            &started.identity.connection_id,
            &started.identity.scope.session_id,
            &started.identity.scope.generation,
            &started.identity.scope.operation_id,
        )
        .expect("the retired operation keeps a bounded status");
    assert_eq!(
        status.local,
        LocalCompletionState::Unknown,
        "event loss cannot certify unchanged input sequence"
    );
    let result = fixture.finish(&mut started);
    assert_eq!(result["local_completion"]["status"], "unknown");
}

#[test]
fn completion_event_loss_requires_fresh_accepted_input_evidence_before_new_watch() {
    let mut fixture = Fixture::new();
    fixture
        .bridge
        .invalidate_completion_runtime(fixture.target.runtime);
    // A refreshed native proof alone cannot restore a sequence whose stream was lost.
    fixture
        .bridge
        .install_native_turns(vec![fixture.baseline.clone()]);
    let rejected = fixture.try_start("event-loss-stale-sequence", None);
    assert!(
        rejected.is_err(),
        "a previous subscription's input sequence authorized a new tracked submission"
    );

    fixture.bridge.observe_input(
        fixture.target.runtime,
        &[runtime::RuntimeEvent::SessionInputAccepted {
            session: fixture.target.session,
            sequence: 1000,
            operation_id: None,
            at_micros: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64,
        }],
    );
    assert!(
        fixture.try_start("event-loss-fresh-sequence", None).is_ok(),
        "fresh accepted input evidence and refreshed proof should restore eligibility"
    );
}

#[test]
fn completion_event_loss_defers_native_result_until_runtime_input_drain_finishes() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("event-order-manual-intervention", None);
    let accepted_at = fixture.admit(&started, Ok(()));
    let completed = fixture.completed_proof(&started, accepted_at);

    fixture.bridge.set_completion_observation_ready(false);
    fixture.bridge.install_native_turns(vec![completed]);
    let status = fixture
        .bridge
        .completion_status_for_operation(
            &started.identity.connection_id,
            &started.identity.scope.session_id,
            &started.identity.scope.generation,
            &started.identity.scope.operation_id,
        )
        .unwrap();
    assert_ne!(
        status.local,
        LocalCompletionState::Completed,
        "proof installation finalized before already-accepted runtime input was drained"
    );

    fixture.bridge.observe_input(
        fixture.target.runtime,
        &[runtime::RuntimeEvent::SessionInputAccepted {
            session: fixture.target.session,
            sequence: started.sequence + 1,
            operation_id: None,
            at_micros: accepted_at + 1,
        }],
    );
    fixture.bridge.set_completion_observation_ready(true);
    fixture.bridge.poll_completions();
    let status = fixture
        .bridge
        .completion_status_for_operation(
            &started.identity.connection_id,
            &started.identity.scope.session_id,
            &started.identity.scope.generation,
            &started.identity.scope.operation_id,
        )
        .unwrap();
    assert_eq!(status.local, LocalCompletionState::Unknown);
    let result = fixture.finish(&mut started);
    assert_eq!(result["local_completion"]["status"], "unknown");
}

#[test]
fn completion_event_loss_durable_native_finish_preserves_closed_delivery() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("closed-delivery-native-finish", None);
    let accepted_at = fixture.admit(&started, Ok(()));
    assert!(
        fixture
            .bridge
            .completion_transport_permit(&started.internal)
            .unwrap()
            .is_live(),
        "transport TTL closure is independent of read consent"
    );
    fixture
        .bridge
        .observe_completion_delivery(agent_mcp::CompletionDeliveryEvent {
            identity: started.identity.clone(),
            state: agent_mcp::CompletionDeliveryState::Closed,
        });
    fixture
        .bridge
        .install_native_turns(vec![fixture.completed_proof(&started, accepted_at)]);

    let result = fixture.finish(&mut started);
    assert_eq!(result["local_completion"]["status"], "completed");
    let status = fixture
        .bridge
        .completion_status_for_operation(
            &started.identity.connection_id,
            &started.identity.scope.session_id,
            &started.identity.scope.generation,
            &started.identity.scope.operation_id,
        )
        .unwrap();
    assert_eq!(status.local, LocalCompletionState::Completed);
    assert_eq!(
        status.delivery,
        agent_mcp::CompletionDeliveryState::Closed,
        "durable local completion cannot reopen a closed original transport"
    );
}

#[test]
fn completion_event_loss_fences_unadmitted_input_but_returns_unknown_on_original_stream() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("event-loss-unadmitted-input", None);
    let input_fence = fixture
        .bridge
        .completion_input_permit(&started.internal)
        .unwrap();
    assert!(input_fence.is_live());

    fixture
        .bridge
        .invalidate_completion_runtime(fixture.target.runtime);
    assert!(
        !input_fence.is_live(),
        "event loss left the already-queued PTY command's input fence live"
    );
    fixture.admit(
        &started,
        Err(runtime::PtyInputRejectReason::AdmissionDenied),
    );
    let response = fixture.finish(&mut started);
    assert_eq!(
        response["local_completion"]["status"], "unknown",
        "fencing input must preserve the read-authorized original result stream"
    );
    assert_eq!(fixture.receipt(&started).tool_result(), response);
}

#[test]
fn completion_event_loss_unrelated_runtime_does_not_block_healthy_native_completion() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("event-loss-independent-runtime", None);
    let accepted_at = fixture.admit(&started, Ok(()));

    fixture.bridge.set_completion_observation_ready(false);
    fixture
        .bridge
        .set_completion_observation_runtimes(&[(fixture.target.runtime, true), (999, false)]);
    fixture
        .bridge
        .install_native_turns(vec![fixture.completed_proof(&started, accepted_at)]);
    fixture.bridge.poll_completions();
    let status = fixture
        .bridge
        .completion_status_for_operation(
            &started.identity.connection_id,
            &started.identity.scope.session_id,
            &started.identity.scope.generation,
            &started.identity.scope.operation_id,
        )
        .unwrap();
    assert_eq!(
        status.local,
        LocalCompletionState::Completed,
        "an unrelated runtime's lost event stream blocked healthy completion evidence"
    );
    let response = fixture.finish(&mut started);
    assert_eq!(response["local_completion"]["status"], "completed");
    assert_eq!(fixture.receipt(&started).tool_result(), response);
}
