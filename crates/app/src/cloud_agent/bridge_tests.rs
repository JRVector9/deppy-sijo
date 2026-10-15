//! App-boundary fixtures use independently authenticated credentials, not supplied actor IDs.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::time::Duration;

struct Client {
    id: String,
    bearer: String,
}
fn setup() -> (CloudAgent, Target) {
    let mut bridge = CloudAgent::memory();
    bridge.server = Some(Server::start(0, "", || {}).unwrap());
    let target = Target::fixture("original", "g1");
    bridge.set_targets(vec![target.clone()]);
    (bridge, target)
}
fn client(bridge: &CloudAgent, name: &str) -> Client {
    let (principal, token) = bridge
        .server
        .as_ref()
        .unwrap()
        .auth
        .mint_manual_connection(name, true, now())
        .unwrap();
    Client {
        id: principal.id,
        bearer: token.as_str().into(),
    }
}
fn request(
    bridge: &CloudAgent,
    client: &Client,
    tool: &str,
    args: Value,
) -> (Request, mpsc::Receiver<Result<Value, String>>) {
    let auth = &bridge.server.as_ref().unwrap().auth;
    let header = format!("Bearer {}", client.bearer);
    let (reply, rx) = mpsc::sync_channel(1);
    (
        Request {
            epoch: auth.authenticate(&header, now()).unwrap(),
            principal: auth.authenticate_principal(&header, now()).unwrap(),
            access_key: Some(URL_SAFE_NO_PAD.encode(Sha256::digest(client.bearer.as_bytes()))),
            input_scope: true,
            deadline: Instant::now() + Duration::from_secs(3),
            tool: tool.into(),
            args,
            reply,
        },
        rx,
    )
}

#[test]
fn granting_only_a_named_connection_does_not_share_with_the_legacy_bearer() {
    let (mut bridge, target) = setup();
    let named = client(&bridge, "Named bot");
    let legacy = Client {
        id: agent_mcp::LEGACY_CONNECTION_ID.into(),
        bearer: bridge
            .server
            .as_ref()
            .unwrap()
            .auth
            .token_for_user()
            .as_str()
            .into(),
    };
    bridge
        .set_connection_access(&named.id, &target, true, false)
        .unwrap();
    let (mut req, rx) = request(&bridge, &legacy, "list_sessions", json!({}));
    req.access_key = None;
    bridge.handle_and_drain(req, |_, _| panic!("reading consent cannot type"));
    assert!(
        rx.recv().unwrap().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "consent for one named connection must not expose the session to a different bearer"
    );
    assert!(bridge.connection_permission(&named.id, &target).read);
    bridge
        .set_connection_access(&legacy.id, &target, true, false)
        .unwrap();
    let (mut req, rx) = request(&bridge, &legacy, "list_sessions", json!({}));
    req.access_key = None;
    bridge.handle_and_drain(req, |_, _| panic!("legacy read consent cannot type"));
    assert_eq!(
        rx.recv().unwrap().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn granting_named_input_does_not_widen_existing_legacy_read_consent() {
    let (mut bridge, target) = setup();
    bridge.share(&target, true);
    let named = client(&bridge, "Named input bot");
    let legacy = Client {
        id: agent_mcp::LEGACY_CONNECTION_ID.into(),
        bearer: bridge
            .server
            .as_ref()
            .unwrap()
            .auth
            .token_for_user()
            .as_str()
            .into(),
    };
    bridge
        .set_connection_access(&named.id, &target, true, true)
        .unwrap();
    let (mut req, rx) = request(
        &bridge,
        &legacy,
        "send_text",
        json!({"session_id":target.id,"generation":target.generation,"operation_id":"legacy-not-approved","text":"unexpected input"}),
    );
    req.access_key = None;
    bridge.handle_and_drain(req, |_, _| panic!("unselected legacy actor cannot write"));
    assert_eq!(rx.recv().unwrap().unwrap_err(), "client_input_not_allowed");
    assert!(bridge.connection_permission(&legacy.id, &target).read);
    assert!(!bridge.connection_permission(&legacy.id, &target).input);
    assert!(bridge.connection_permission(&named.id, &target).input);
}

#[test]
fn same_external_id_has_two_independent_original_session_answers_and_receipts() {
    let (mut bridge, target) = setup();
    let a = client(&bridge, "A");
    let b = client(&bridge, "B");
    for actor in [&a, &b] {
        bridge
            .set_connection_access(&actor.id, &target, true, false)
            .unwrap();
        let (req, rx) = request(
            &bridge,
            actor,
            "notify",
            json!({"session_id":target.id,"generation":target.generation,"operation_id":"same","message":actor.id}),
        );
        let notice = bridge
            .handle_and_drain(req, |_, _| panic!("notify cannot type"))
            .unwrap();
        assert_eq!(notice.target.id, target.id);
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "stored");
    }
    assert_eq!(bridge.answers.len(), 2);
    assert_ne!(bridge.answers[0].id, bridge.answers[1].id);
    assert_ne!(bridge.answers[0].connection, bridge.answers[1].connection);
    for actor in [&a, &b] {
        let (req, rx) = request(
            &bridge,
            actor,
            "get_operation",
            json!({"session_id":target.id,"generation":target.generation,"operation_id":"same"}),
        );
        bridge.handle_and_drain(req, |_, _| panic!("receipt lookup cannot type"));
        let result = rx.recv().unwrap().unwrap();
        assert!(result.to_string().contains("stored"));
    }
    bridge.revoke_connection(&a.id);
    assert!(
        bridge
            .connection_infos()
            .iter()
            .any(|info| info.principal.id == b.id
                && info.state == agent_mcp::ConnectionState::Active)
    );
    assert!(bridge.connection_permission(&b.id, &target).read);
}

#[test]
fn explicit_consent_and_one_writer_are_required_for_new_credentials() {
    let (mut bridge, target) = setup();
    let a = client(&bridge, "A");
    let b = client(&bridge, "B");
    let (req, rx) = request(&bridge, &a, "list_sessions", json!({}));
    bridge.handle_and_drain(req, |_, _| panic!());
    assert_eq!(
        rx.recv().unwrap().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    for actor in [&a, &b] {
        bridge
            .set_connection_access(&actor.id, &target, true, true)
            .unwrap();
    }
    let (req, rx) = request(
        &bridge,
        &a,
        "send_text",
        json!({"session_id":target.id,"generation":target.generation,"operation_id":"one","text":"x"}),
    );
    bridge.handle_and_drain(req, |_, _| panic!("unleased actor typed"));
    assert_eq!(
        rx.recv().unwrap().unwrap_err(),
        "control_required_no_effect"
    );
    let args = json!({"session_id":target.id,"generation":target.generation});
    let (req, rx) = request(&bridge, &a, "acquire_control", args.clone());
    bridge.handle_and_drain(req, |_, _| panic!("control acquisition cannot type"));
    assert_eq!(rx.recv().unwrap().unwrap()["control"], "acquired");
    let (req, rx) = request(&bridge, &b, "acquire_control", args);
    bridge.handle_and_drain(req, |_, _| panic!());
    assert_eq!(rx.recv().unwrap().unwrap_err(), "session_control_busy");
    bridge.take_session_control(&target);
    assert!(bridge.connection_lease(&target).is_none());
    assert!(!bridge.connection_permission(&a.id, &target).input);
    let mut next = target.clone();
    next.generation = "g2".into();
    bridge.set_targets(vec![next.clone()]);
    assert!(!bridge.connection_permission(&a.id, &next).read);
}

#[test]
fn history_is_correlated_bounded_redacted_and_cursor_cannot_cross_actor() {
    use terminal::TerminalBackend;
    let (mut bridge, target) = setup();
    let a = client(&bridge, "A");
    let b = client(&bridge, "B");
    for actor in [&a, &b] {
        bridge
            .set_connection_access(&actor.id, &target, true, false)
            .unwrap();
    }
    let secret = secret::SecretString::new("private-history-key-12345".into());
    let _lease = bridge.redaction.acquire_rotating(&secret).unwrap();
    let mut backend = terminal::AlacrittyBackend::new(80, 3, 600);
    for i in 0..400 {
        backend
            .feed(format!("older-{i} private-history-key-12345\r\n").as_bytes())
            .unwrap();
    }
    let (req, rx) = request(
        &bridge,
        &a,
        "read_history",
        json!({"session_id":target.id,"generation":target.generation,"max_bytes":1024}),
    );
    let mut effect = None;
    bridge.handle(req, |_, e| {
        effect = Some(e);
        Ok(())
    });
    assert!(rx.try_recv().is_err());
    let Effect::History {
        operation_id,
        query,
    } = effect.unwrap()
    else {
        panic!("history must be a read-only query")
    };
    let native_offset = backend.viewport_snapshot().unwrap().scroll_offset;
    let snapshot = backend.history_snapshot(query).unwrap();
    assert_eq!(
        backend.viewport_snapshot().unwrap().scroll_offset,
        native_offset
    );
    bridge.observe_input(
        target.runtime,
        &[runtime::RuntimeEvent::TerminalHistoryResult {
            session: target.session,
            operation_id,
            snapshot: Some(std::sync::Arc::new(snapshot.snapshot)),
            expired: snapshot.expired,
        }],
    );
    let value = rx.recv().unwrap().unwrap();
    assert_eq!(value["source"], "terminal_history");
    assert!(value["screen"].as_str().unwrap().len() <= 1024);
    assert!(!value.to_string().contains("private-history-key-12345"));
    assert_eq!(value["truncated"], true);
    let cursor = value["next_cursor"].as_u64().unwrap();
    let (req, rx) = request(
        &bridge,
        &b,
        "read_history",
        json!({"session_id":target.id,"generation":target.generation,"cursor":cursor}),
    );
    bridge.handle(req, |_, _| panic!("foreign cursor must not query history"));
    assert_eq!(rx.recv().unwrap().unwrap_err(), "invalid_cursor");
}

#[test]
fn profiles_and_mobile_projection_never_restore_consent_or_contain_credentials() {
    let (mut bridge, target) = setup();
    let actor = client(&bridge, "A");
    bridge
        .set_connection_access(&actor.id, &target, true, true)
        .unwrap();
    bridge.profile_name = "stable".into();
    bridge.hostname = "deppy.example.test:8443".into();
    bridge.automatic = false;
    bridge.save_profile().unwrap();
    let config = crate::config::CloudAgentConfig {
        profiles: bridge.profiles.clone(),
        ..Default::default()
    };
    let serialized = serde_json::to_string(&config).unwrap();
    assert!(!serialized.contains(&actor.bearer));
    let restored: crate::config::CloudAgentConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored.profiles[0].public_host, "deppy.example.test:8443");
    let snapshot = bridge.cloud_snapshot();
    assert!(snapshot.validate().is_ok());
    assert!(
        !snapshot
            .encode_frame(true, true)
            .unwrap()
            .contains(&actor.bearer)
    );
    bridge.stop_connection();
    bridge.hostname.clear();
    bridge.load_profile(0).unwrap();
    assert!(bridge.grants.is_empty());
    assert!(bridge.server.is_none());
}

#[test]
fn truncated_durable_history_remains_explicit_in_mobile_projection() {
    let (mut bridge, _) = setup();
    bridge.history_truncated = true;
    assert!(bridge.cloud_snapshot().truncated);
}

#[test]
fn transient_local_listener_failure_keeps_bounded_automatic_recovery_scheduled() {
    let mut bridge = CloudAgent::memory();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    bridge.port = occupied.local_addr().unwrap().port();
    bridge.recovery.start(true);
    bridge.start_auto(&egui::Context::default(), "/unused-companion".into());
    assert!(bridge.server.is_none());
    assert!(
        bridge.recovery.due().is_some(),
        "known-unsent listener failure needs bounded recovery"
    );
    bridge.action = Some(Action::Stop);
    bridge.apply_action(&egui::Context::default());
    assert!(bridge.recovery.due().is_none());
}

#[test]
fn ended_session_keeps_read_consent_and_revokes_input_lease() {
    let (mut bridge, mut target) = setup();
    let actor = client(&bridge, "A");
    bridge
        .set_connection_access(&actor.id, &target, true, true)
        .unwrap();
    let (req, _) = request(&bridge, &actor, "acquire_control", json!({}));
    bridge.acquire_lease(&req, &target, None, 30).unwrap();
    target.live = false;
    bridge.set_targets(vec![target.clone()]);
    assert!(bridge.connection_permission(&actor.id, &target).read);
    assert!(!bridge.connection_permission(&actor.id, &target).input);
    assert!(bridge.connection_lease(&target).is_none());
}

#[test]
fn repeated_warm_projection_preserves_original_receive_time() {
    use terminal::TerminalBackend;
    let (mut bridge, target) = setup();
    let mut backend = terminal::AlacrittyBackend::new(20, 3, 10);
    backend.feed(b"old screen").unwrap();
    let events = [runtime::RuntimeEvent::Viewport {
        session: target.session,
        snapshot: std::sync::Arc::new(backend.viewport_snapshot().unwrap()),
        bracketed_paste: false,
    }];
    bridge.observe_screen_events(target.runtime, &events, 10);
    assert_eq!(
        bridge.observed_screen_at(target.runtime, target.session),
        Some(10)
    );
    for _ in 0..3 {
        bridge.set_targets(vec![target.clone()]);
    }
    assert_eq!(
        bridge.observed_screen_at(target.runtime, target.session),
        Some(10)
    );
    bridge.observe_screen_events(target.runtime, &events, 25);
    assert_eq!(
        bridge.observed_screen_at(target.runtime, target.session),
        Some(25)
    );
}

#[test]
fn run_list_accepts_the_advertised_byte_budget() {
    let (mut bridge, target) = setup();
    let actor = client(&bridge, "A");
    bridge
        .set_connection_access(&actor.id, &target, true, false)
        .unwrap();
    let (req, rx) = request(&bridge, &actor, "list_runs", json!({"max_bytes":4096}));
    bridge.handle_and_drain(req, |_, _| panic!("list cannot type"));
    assert!(rx.recv().unwrap().is_ok());
}

#[test]
fn rotated_independent_credentials_do_not_exhaust_issuance_capacity() {
    let (mut bridge, _) = setup();
    for _ in 0..3 {
        for index in 0..32 {
            bridge
                .issue_manual_connection(&format!("bot-{index}"), false)
                .unwrap();
        }
        bridge.action = Some(Action::Rotate);
        bridge.apply_action(&egui::Context::default());
        assert!(bridge.issued_leases.is_empty());
    }
}

#[test]
fn normal_idle_frame_gap_preserves_approved_input_and_control_lease() {
    let (mut bridge, target) = setup();
    let actor = client(&bridge, "Idle bot");
    bridge
        .set_connection_access(&actor.id, &target, true, true)
        .unwrap();
    let (req, _) = request(&bridge, &actor, "acquire_control", json!({}));
    let lease = bridge.acquire_lease(&req, &target, None, 120).unwrap();
    bridge.observe_continuity(Some(continuity::ClockSample {
        continuous: Duration::from_secs(100),
        awake: Duration::from_secs(50),
    }));
    bridge.observe_continuity(Some(continuity::ClockSample {
        continuous: Duration::from_secs(220),
        awake: Duration::from_secs(170),
    }));
    assert!(bridge.connection_permission(&actor.id, &target).input);
    assert_eq!(bridge.connection_lease(&target).unwrap().epoch, lease.epoch);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn unavailable_frame_clock_rejects_fresh_legacy_input_before_next_sample() {
    check_unavailable_frame_clock_rejects_fresh_input(false);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn unavailable_frame_clock_rejects_fresh_named_input_before_next_sample() {
    check_unavailable_frame_clock_rejects_fresh_input(true);
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn check_unavailable_frame_clock_rejects_fresh_input(named: bool) {
    let (mut bridge, target) = setup();
    let actor = if named {
        client(&bridge, "Resume bot")
    } else {
        Client {
            id: agent_mcp::LEGACY_CONNECTION_ID.into(),
            bearer: bridge
                .server
                .as_ref()
                .unwrap()
                .auth
                .token_for_user()
                .as_str()
                .into(),
        }
    };
    bridge
        .set_connection_access(&actor.id, &target, true, true)
        .unwrap();
    let (mut lease_request, _) = request(&bridge, &actor, "acquire_control", json!({}));
    if !named {
        lease_request.access_key = None;
    }
    bridge
        .acquire_lease(&lease_request, &target, None, 120)
        .unwrap();
    bridge.observe_continuity(Some(continuity::ClockSample {
        continuous: Duration::from_secs(100),
        awake: Duration::from_secs(50),
    }));
    bridge.observe_continuity(None);

    // A new request can sample valid clocks after resume, before the next App
    // frame compares them with the pre-suspend baseline. Its own fresh sample
    // cannot prove that the previous input approval survived sleep safely.
    assert!(continuity::clock_sample().is_some());
    let (mut req, rx) = request(
        &bridge,
        &actor,
        "send_text",
        json!({"session_id":target.id,"generation":target.generation,"operation_id":"fresh-after-clock-failure","text":"unexpected input"}),
    );
    if !named {
        req.access_key = None;
    }
    let mut dispatched = 0;
    bridge.handle_and_drain(req, |_, _| {
        dispatched += 1;
        Err("unexpected_input_dispatch".into())
    });
    let result = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(dispatched, 0, "unknown frame clocks must fence fresh input");
    assert_eq!(result.unwrap_err(), "input_not_allowed_or_session_exited");
    assert!(bridge.connection_permission(&actor.id, &target).read);
    assert!(!bridge.connection_permission(&actor.id, &target).input);
    assert!(bridge.connection_lease(&target).is_none());
    assert!(bridge.lease_permits.is_empty());
    bridge.observe_continuity(Some(continuity::ClockSample {
        continuous: Duration::from_secs(140),
        awake: Duration::from_secs(51),
    }));
    assert!(!bridge.connection_permission(&actor.id, &target).input);
}

#[test]
fn auth_rotation_retires_old_named_permissions_without_exhausting_capacity() {
    let (mut bridge, _) = setup();
    let targets: Vec<_> = (0..MAX_TARGETS)
        .map(|index| Target::fixture(&format!("session-{index}"), "generation"))
        .collect();
    bridge.set_targets(targets.clone());
    for cycle in 0..4 {
        for index in 0..8 {
            let actor = client(&bridge, &format!("cycle-{cycle}-bot-{index}"));
            for target in &targets {
                bridge
                    .set_connection_access(&actor.id, target, true, false)
                    .unwrap_or_else(|error| {
                        panic!("cycle {cycle}, bot {index}, {}: {error}", target.id)
                    });
            }
        }
        let obsolete: Vec<_> = bridge
            .connection_infos()
            .into_iter()
            .filter(|info| info.principal.id != agent_mcp::LEGACY_CONNECTION_ID)
            .map(|info| info.principal.id)
            .collect();
        bridge.action = Some(Action::Rotate);
        bridge.apply_action(&egui::Context::default());
        for id in obsolete {
            assert!(
                !bridge.connection_permission(&id, &targets[0]).read,
                "rotation must remove the retired principal's permission"
            );
        }
    }
}

#[test]
fn auth_rotation_preserves_effective_legacy_read_consent_and_denials() {
    for (read, explicit, revoked) in [
        (true, false, false),
        (true, true, false),
        (false, true, false),
        (false, false, true),
    ] {
        let (mut bridge, target) = setup();
        bridge.share(&target, true);
        if explicit {
            bridge
                .set_connection_access(agent_mcp::LEGACY_CONNECTION_ID, &target, read, false)
                .unwrap();
        }
        if revoked {
            bridge.revoke_connection(agent_mcp::LEGACY_CONNECTION_ID);
        }
        let actor = client(&bridge, "Before rotation");
        bridge
            .set_connection_access(&actor.id, &target, true, true)
            .unwrap();
        let (req, _) = request(&bridge, &actor, "acquire_control", json!({}));
        bridge.acquire_lease(&req, &target, None, 120).unwrap();
        bridge.action = Some(Action::Rotate);
        bridge.apply_action(&egui::Context::default());
        assert_eq!(
            bridge
                .connection_permission(agent_mcp::LEGACY_CONNECTION_ID, &target)
                .read,
            read,
            "legacy read consent must survive rotation without widening"
        );
        assert!(!bridge.connection_permission(&actor.id, &target).input);
        assert!(bridge.connection_lease(&target).is_none());
        assert!(bridge.lease_permits.is_empty());
        assert!(bridge.grants.contains_key(&target.id));
    }
}

#[test]
fn rendered_wrapped_credentials_are_redacted_before_cloud_snapshot_byte_limits() {
    use terminal::TerminalBackend;
    let (bridge, _) = setup();
    let token = "synthetic-credential-1234567890abcdefghijklmnopqrstuvwxyz-ABCDEFG";
    let _lease = bridge
        .redaction
        .acquire_rotating(&secret::SecretString::new(token.into()))
        .unwrap();
    let mut backend = terminal::AlacrittyBackend::new(20, 6, 10);
    backend
        .feed(format!("before\r\n{token}\r\nafter").as_bytes())
        .unwrap();
    let raw = super::screen_text(&backend.viewport_snapshot().unwrap());
    let clean = bridge.redact_screen(&raw);
    assert!(!clean.replace(['\n', '\r'], "").contains(token));
    assert!(clean.contains("[REDACTED]"));
    assert!(clean.contains("before") && clean.contains("after"));
}

#[test]
fn review_20261015_list_sessions_redacts_known_secret_display_metadata() {
    let (mut bridge, mut target) = setup();
    let secret_value = "review-known-cloud-secret-access-token-20261015";
    let secret = secret::SecretString::new(secret_value.into());
    let _lease = bridge.redaction.acquire_rotating(&secret).unwrap();
    target.title = format!("Terminal {secret_value}");
    target.workspace_name = format!("Project {secret_value}");
    bridge.set_targets(vec![target.clone()]);
    let actor = client(&bridge, "Read-only reviewer");
    bridge
        .set_connection_access(&actor.id, &target, true, false)
        .unwrap();
    let (req, rx) = request(&bridge, &actor, "list_sessions", json!({}));
    bridge.handle_and_drain(req, |_, _| panic!("listing must not dispatch input"));
    let reply = rx.recv().unwrap().unwrap();
    let row = &reply["sessions"][0];
    assert_eq!(
        row["session_id"], target.id,
        "routing identity must be retained"
    );
    assert_eq!(row["generation"], target.generation);
    assert_eq!(
        row["input_allowed"], false,
        "the recipient only has read consent"
    );
    assert_eq!(
        row["title"], "Terminal [REDACTED]",
        "known credential leaked through session display title"
    );
    assert_eq!(
        row["workspace_name"], "Project [REDACTED]",
        "known credential leaked through workspace display name"
    );
    assert!(!reply.to_string().contains(secret_value));
}

#[test]
fn full_fix_cloud_20261015_list_metadata_rescrubs_late_registered_wrapped_secret() {
    let (mut bridge, mut target) = setup();
    let value = "late-metadata-secret-token";
    target.title = "Task late-metadata-\nsecret-token".into();
    target.workspace_name = format!("Project {value}");
    bridge.set_targets(vec![target.clone()]);
    let actor = client(&bridge, "Read-only metadata viewer");
    bridge
        .set_connection_access(&actor.id, &target, true, false)
        .unwrap();
    let (req, rx) = request(&bridge, &actor, "list_sessions", json!({}));
    bridge.handle_and_drain(req, |_, _| panic!("metadata cannot type"));
    assert_eq!(
        rx.recv().unwrap().unwrap()["sessions"][0]["workspace_name"],
        target.workspace_name
    );
    let _lease = bridge
        .redaction
        .acquire_rotating(&secret::SecretString::new(value.into()))
        .unwrap();
    let (req, rx) = request(&bridge, &actor, "list_sessions", json!({}));
    bridge.handle_and_drain(req, |_, _| panic!("metadata cannot type"));
    let reply = rx.recv().unwrap().unwrap();
    assert_eq!(reply["sessions"][0]["title"], "Task [REDACTED]");
    assert_eq!(reply["sessions"][0]["workspace_name"], "Project [REDACTED]");
    assert_eq!(reply["sessions"][0]["session_id"], target.id);
    assert_eq!(reply["sessions"][0]["generation"], target.generation);
    assert_eq!(
        bridge.targets[0].title, target.title,
        "source metadata remains raw for future corpus updates"
    );
}

#[test]
fn full_fix_cloud_20261015_list_metadata_is_bounded_after_redaction() {
    let (mut bridge, mut target) = setup();
    let value = "boundary-metadata-secret-token";
    let _lease = bridge
        .redaction
        .acquire_rotating(&secret::SecretString::new(value.into()))
        .unwrap();
    target.title = format!("{}{value}", "한".repeat(168));
    target.workspace_name = target.title.clone();
    bridge.set_targets(vec![target.clone()]);
    let actor = client(&bridge, "Bounded metadata viewer");
    bridge
        .set_connection_access(&actor.id, &target, true, false)
        .unwrap();
    let (req, rx) = request(&bridge, &actor, "list_sessions", json!({}));
    bridge.handle_and_drain(req, |_, _| panic!("metadata cannot type"));
    let reply = rx.recv().unwrap().unwrap();
    for field in ["title", "workspace_name"] {
        let display = reply["sessions"][0][field].as_str().unwrap();
        assert!(display.len() <= 512);
        assert!(display.starts_with(&"한".repeat(168)));
        assert!(
            !display.contains("boundary"),
            "redact the complete secret before clipping"
        );
    }
}
