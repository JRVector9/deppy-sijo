

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
    assert_eq!(row["session_id"], target.id, "routing identity must be retained");
    assert_eq!(row["generation"], target.generation);
    assert_eq!(row["input_allowed"], false, "the recipient only has read consent");
    assert_eq!(row["title"], "Terminal [REDACTED]", "known credential leaked through session display title");
    assert_eq!(row["workspace_name"], "Project [REDACTED]", "known credential leaked through workspace display name");
    assert!(!reply.to_string().contains(secret_value));
}
