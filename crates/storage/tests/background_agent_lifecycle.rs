use storage::{AgentAttentionEvent, AgentStateJob, AttentionEventKind as K, Db};

const KEY: &str = "00000000-0000-0000-0000-000000000001:1";

fn db() -> Db {
    let dir = std::env::temp_dir().join(format!("deppy-status-review-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    Db::open(&dir.join("metadata.sqlite3")).unwrap()
}

fn event(kind: K, request: &str, at: i64) -> AgentAttentionEvent {
    AgentAttentionEvent {
        native_session_id: "native".into(),
        runtime_instance: None,
        turn_id: Some("turn".into()),
        request_id: request.into(),
        kind,
        at_micros: at,
        tool_group: None,
        tool_input_hash: None,
        child_id: None,
    }
}

#[test]
fn first_child_approval_keeps_working_after_the_result() {
    let d = db();
    d.record_agent_attention(KEY, &event(K::TurnStart, "", 1))
        .unwrap();
    let mut approval = event(K::ApprovalRequired, "child:a:q", 2);
    approval.tool_group = Some("child:a:Bash".into());
    approval.child_id = Some("a".into());
    d.record_agent_attention(KEY, &approval).unwrap();
    d.record_agent_attention(KEY, &event(K::Completed, "", 3))
        .unwrap();
    assert_eq!(
        d.pending_agent_request_ids(KEY, "native").unwrap(),
        vec!["child:a:q"]
    );
    d.record_agent_attention(KEY, &event(K::Resolved, "child:a:q", 4))
        .unwrap();
    assert!(d.list_waiting_sessions().unwrap().is_empty());
    assert_eq!(d.list_working_sessions().unwrap(), vec![KEY]);
    assert!(d.list_turn_done_sessions().unwrap().is_empty());
}

#[test]
fn first_child_ungrouped_question_keeps_working_after_the_answer() {
    let d = db();
    d.record_agent_attention(KEY, &event(K::TurnStart, "", 1))
        .unwrap();
    let mut question = event(K::ResponseRequired, "child:a:elicitation:server", 2);
    question.child_id = Some("a".into());
    d.record_agent_attention(KEY, &question).unwrap();
    d.record_agent_attention(KEY, &event(K::Completed, "", 3))
        .unwrap();
    d.record_agent_attention(KEY, &event(K::Resolved, "child:a:elicitation:server", 4))
        .unwrap();
    assert!(d.list_waiting_sessions().unwrap().is_empty());
    assert_eq!(d.list_working_sessions().unwrap(), vec![KEY]);
    assert!(d.list_turn_done_sessions().unwrap().is_empty());
}

#[test]
fn completion_uses_the_latest_child_exit_even_when_delivered_first() {
    let d = db();
    for (kind, id, at) in [
        (K::TurnStart, "", 1_000_000),
        (K::Working, "child:a:lifecycle", 2_000_000),
        (K::Working, "child:b:lifecycle", 3_000_000),
        (K::Completed, "", 4_000_000),
        (K::Cancelled, "child:a:lifecycle", 10_000_000),
        (K::Cancelled, "child:b:lifecycle", 9_000_000),
    ] {
        d.record_agent_attention(KEY, &event(kind, id, at)).unwrap();
    }
    let snapshot = d
        .apply_agent_state_job(&AgentStateJob::projection("other"))
        .unwrap();
    assert!(snapshot.working_sessions.is_empty());
    assert_eq!(snapshot.idle_sessions.len(), 1);
    assert_eq!(snapshot.idle_sessions[0].1, 10);
    assert!(snapshot.turn_done_sessions[0].1 >= 10_000_000);
}

#[test]
fn completion_clock_is_independent_of_parent_and_child_exit_arrival_order() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let d = db();
        for (kind, id, at) in [
            (K::TurnStart, "", 1_000_000),
            (K::Working, "child:a:lifecycle", 2_000_000),
            (K::Working, "child:b:lifecycle", 3_000_000),
        ] {
            d.record_agent_attention(KEY, &event(kind, id, at)).unwrap();
        }
        let exits = [
            event(K::Completed, "", 4_000_000),
            event(K::Cancelled, "child:a:lifecycle", 10_000_000),
            event(K::Cancelled, "child:b:lifecycle", 9_000_000),
        ];
        for index in order {
            d.record_agent_attention(KEY, &exits[index]).unwrap();
        }
        let snapshot = d
            .apply_agent_state_job(&AgentStateJob::projection("other"))
            .unwrap();
        assert_eq!(snapshot.idle_sessions[0].1, 10, "{order:?}");
        assert_eq!(snapshot.turn_done_sessions[0].1, 10_000_000, "{order:?}");
    }
}

#[test]
fn a_new_parent_episode_discards_previous_child_exit_clocks_and_duplicates() {
    let d = db();
    for (kind, id, at) in [
        (K::TurnStart, "", 1_000_000),
        (K::Working, "child:a:lifecycle", 2_000_000),
        (K::Completed, "", 3_000_000),
        (K::Cancelled, "child:a:lifecycle", 10_000_000),
        (K::Working, "bash", 20_000_000),
        (K::Cancelled, "child:a:lifecycle", 50_000_000),
        (K::Completed, "", 30_000_000),
    ] {
        d.record_agent_attention(KEY, &event(kind, id, at)).unwrap();
    }
    let snapshot = d
        .apply_agent_state_job(&AgentStateJob::projection("other"))
        .unwrap();
    assert_eq!(snapshot.idle_sessions[0].1, 30);
    assert_eq!(snapshot.turn_done_sessions[0].1, 30_000_000);
}

#[test]
fn delayed_parent_activity_preserves_a_newer_child_exit_in_the_same_episode() {
    let d = db();
    for (kind, id, at) in [
        (K::TurnStart, "", 1_000_000),
        (K::Working, "child:a:lifecycle", 2_000_000),
        (K::Working, "child:b:lifecycle", 3_000_000),
        (K::Completed, "", 4_000_000),
        (K::Cancelled, "child:a:lifecycle", 10_000_000),
        (K::Working, "bash", 8_000_000),
        (K::Completed, "", 9_000_000),
        (K::Cancelled, "child:b:lifecycle", 9_500_000),
    ] {
        d.record_agent_attention(KEY, &event(kind, id, at)).unwrap();
    }
    let snapshot = d
        .apply_agent_state_job(&AgentStateJob::projection("other"))
        .unwrap();
    assert_eq!(snapshot.idle_sessions[0].1, 10);
    assert_eq!(snapshot.turn_done_sessions[0].1, 10_000_000);
}

#[test]
fn resolved_first_child_requests_do_not_reopen_a_lifetime() {
    for (kind, id) in [
        (K::ApprovalRequired, "child:a:q"),
        (K::ResponseRequired, "child:a:elicitation:server"),
    ] {
        let d = db();
        for next in [
            event(K::TurnStart, "", 1_000_000),
            event(K::Resolved, id, 4_000_000),
            event(kind, id, 2_000_000),
            event(K::Completed, "", 3_000_000),
        ] {
            d.record_agent_attention(KEY, &next).unwrap();
        }
        let snapshot = d
            .apply_agent_state_job(&AgentStateJob::projection("other"))
            .unwrap();
        assert!(snapshot.waiting_sessions.is_empty());
        assert!(snapshot.working_sessions.is_empty());
        assert_eq!(snapshot.idle_sessions[0].1, 4);
    }
}

#[test]
fn first_child_requests_reserve_capacity_atomically() {
    for kind in [K::ApprovalRequired, K::ResponseRequired] {
        let d = db();
        d.record_agent_attention(KEY, &event(K::TurnStart, "", 1))
            .unwrap();
        for index in 0..64 {
            d.record_agent_attention(
                KEY,
                &event(K::Working, &format!("child:{index}:lifecycle"), index + 2),
            )
            .unwrap();
        }
        let before = d.agent_result_candidate_ids(KEY, "native").unwrap();
        assert!(
            d.record_agent_attention(KEY, &event(kind, "child:extra:q", 66))
                .is_err()
        );
        assert!(
            d.pending_agent_request_ids(KEY, "native")
                .unwrap()
                .is_empty()
        );
        assert_eq!(d.agent_result_candidate_ids(KEY, "native").unwrap(), before);
        d.record_agent_attention(KEY, &event(kind, "child:0:q", 67))
            .unwrap();
        d.record_agent_attention(KEY, &event(K::Resolved, "child:0:q", 68))
            .unwrap();
        d.record_agent_attention(KEY, &event(K::SessionEnd, "", 69))
            .unwrap();
        assert!(d.list_waiting_sessions().unwrap().is_empty());
        assert!(d.list_working_sessions().unwrap().is_empty());
    }
}
