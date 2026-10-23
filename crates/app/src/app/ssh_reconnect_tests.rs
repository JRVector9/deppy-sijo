use super::*;
use crate::ssh_workspace_host::{SshHostEvent, SshWorkspaceHost};
use std::time::{Duration, Instant};

struct ReconnectFixture {
    workspace: String,
    generation: u64,
    target: SshWorkspaceTarget,
    profile: SshConnectionProfile,
    client: Arc<runtime::SshRuntimeClient>,
    output: std::sync::mpsc::SyncSender<SshHostEvent>,
}

fn install_established_host(app: &mut App) -> ReconnectFixture {
    let workspace = app.active.id.clone();
    let WorkspaceTarget::Ssh(target) = app.workspace_targets[&workspace].clone() else {
        panic!("remote fixture");
    };
    let mut profile = app
        .db
        .ssh_connection_profile(&target.connection_id)
        .unwrap()
        .unwrap();
    // Even a regression that starts an unwanted worker can contact only loopback.
    profile.destination = "fixture@127.0.0.1".into();
    profile.port = Some(1);
    app.db.upsert_ssh_connection_profile(&profile).unwrap();
    let WorkspaceRuntimeClient::Remote { client, .. } = &app.active.runtime else {
        panic!("connected fixture");
    };
    let client = Arc::clone(client);
    let mut dialog = crate::workspace_add::WorkspaceAddUi::new(
        crate::workspace_add::WorkspaceAddPurpose::SwitchRuntime,
        PathBuf::new(),
    );
    dialog.open_ssh_draft(App::ssh_draft(&profile));
    let generation = dialog.ssh_mut().generation();
    let (host, output) = SshWorkspaceHost::event_fixture(generation);
    let mut reconnect = crate::ssh_reconnect::SshReconnect::default();
    reconnect.connected();
    app.ssh_hosts.insert(
        workspace.clone(),
        SshBoundHost {
            runtime_instance: app.active.runtime_instance,
            target: target.clone(),
            host,
            authentication_ui: Some(dialog),
            connection_pending: false,
            reconnect,
        },
    );
    ReconnectFixture {
        workspace,
        generation,
        target,
        profile,
        client,
        output,
    }
}

fn send_failure(fixture: &ReconnectFixture, error: crate::ssh_workspace_add::SshWorkspaceError) {
    fixture
        .output
        .send(SshHostEvent::Ui(
            crate::ssh_workspace_add::SshWorkspaceSnapshot {
                generation: fixture.generation,
                update: crate::ssh_workspace_add::SshWorkspaceUpdate::Failed(error),
            },
        ))
        .unwrap();
    fixture
        .output
        .send(SshHostEvent::Offline {
            generation: fixture.generation,
        })
        .unwrap();
}

#[test]
fn ssh_reconnect_transient_disconnect_retries_same_workspace_automatically() {
    ssh_integration_tests::with_connected_ssh_app_fixture(|app, root| {
        let id = app.active.id.clone();
        let WorkspaceTarget::Ssh(target) = app.workspace_targets[&id].clone() else {
            panic!("remote fixture");
        };
        let mut profile = app
            .db
            .ssh_connection_profile(&target.connection_id)
            .unwrap()
            .unwrap();
        // A retry may contact only this closed loopback port, never a real user host.
        profile.destination = "fixture@127.0.0.1".into();
        profile.port = Some(1);
        app.db.upsert_ssh_connection_profile(&profile).unwrap();
        let WorkspaceRuntimeClient::Remote { client, .. } = &app.active.runtime else {
            panic!("connected fixture");
        };
        let client = Arc::clone(client);
        // Connected events replace an old/offline attachment, never the new client itself.
        app.active.runtime = WorkspaceRuntimeClient::offline();
        let (host, output) = SshWorkspaceHost::event_fixture(901);
        app.ssh_hosts.insert(
            id.clone(),
            SshBoundHost {
                runtime_instance: app.active.runtime_instance,
                target: target.clone(),
                reconnect: Default::default(),
                host,
                authentication_ui: None,
                connection_pending: true,
            },
        );
        output
            .send(SshHostEvent::Connected {
                generation: 901,
                client: Arc::clone(&client),
                profile,
                home: target.canonical_root.clone(),
                target,
                agents: Vec::new(),
            })
            .unwrap();
        app.poll_ssh_workspace_hosts();
        client.disconnect();
        output
            .send(SshHostEvent::Ui(
                crate::ssh_workspace_add::SshWorkspaceSnapshot {
                    generation: 901,
                    update: crate::ssh_workspace_add::SshWorkspaceUpdate::Failed(
                        crate::ssh_workspace_add::SshWorkspaceError::ConnectionFailed,
                    ),
                },
            ))
            .unwrap();
        output
            .send(SshHostEvent::Offline { generation: 901 })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.ssh_hosts[&id].host.generation() == 901 && Instant::now() < deadline {
            app.poll_ssh_workspace_hosts();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            app.ssh_hosts[&id].host.generation(),
            901,
            "a formerly connected session must retry after a transient disconnect"
        );
        assert!(
            app.workspace_add_ui.is_none(),
            "transient loss should not interrupt with an authentication modal"
        );
        assert!(root.exists());
    });
}

#[test]
fn ssh_reconnect_target_change_retires_queued_authentication_and_connected_events() {
    ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
        let fixture = install_established_host(app);
        let old_instance = app.active.runtime_instance;
        fixture
            .output
            .send(SshHostEvent::Ui(
                crate::ssh_workspace_add::SshWorkspaceSnapshot {
                    generation: fixture.generation,
                    update: crate::ssh_workspace_add::SshWorkspaceUpdate::Prompt(
                        ssh_transport::AuthPrompt {
                            id: 1,
                            kind: ssh_transport::PromptKind::Authentication,
                            text: "Retired target password:".into(),
                        },
                    ),
                },
            ))
            .unwrap();
        fixture
            .output
            .send(SshHostEvent::Connected {
                generation: fixture.generation,
                client: Arc::clone(&fixture.client),
                profile: fixture.profile.clone(),
                target: fixture.target.clone(),
                home: fixture.target.canonical_root.clone(),
                agents: Vec::new(),
            })
            .unwrap();
        let mut next_target = fixture.target.clone();
        next_target.canonical_root = deppy_core::workspace_target::RemotePath::new(format!(
            "{}/different-root",
            fixture.target.canonical_root.as_str().trim_end_matches('/')
        ))
        .unwrap();
        next_target.remote_workspace_id = deppy_remote_host::services::workspace_id(
            &next_target.account,
            next_target.canonical_root.as_str(),
        );
        app.workspace_targets
            .insert(fixture.workspace.clone(), WorkspaceTarget::Ssh(next_target));

        app.poll_ssh_workspace_hosts();
        app.poll_ssh_reconnects(Instant::now() + Duration::from_secs(60));

        assert!(
            app.workspace_add_ui.is_none(),
            "a retired target's queued prompt must not open an authentication modal"
        );
        assert!(
            app.ssh_hosts
                .get(&fixture.workspace)
                .is_none_or(|owner| owner.target != fixture.target),
            "changing the exact target must drop its old transport owner"
        );
        assert_eq!(
            app.active.runtime_instance, old_instance,
            "a queued connection for the retired target must not replace the runtime"
        );
    });
}

#[test]
fn ssh_reconnect_confirmed_active_workspace_close_stops_later_automatic_retry() {
    ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
        let fixture = install_established_host(app);
        assert!(app.close_workspace_sessions(&fixture.workspace));
        // Model the matching remote close acknowledgement. The production close
        // completion retains an active empty attachment for explicit reopening.
        app.ssh_pending_closes
            .get_mut(&fixture.workspace)
            .expect("SSH close must await remote acknowledgement")
            .confirmed = true;
        app.poll_ssh_workspace_closes(Instant::now());
        assert!(matches!(
            app.closed_workspaces.get(&fixture.workspace),
            Some(ClosedWorkspaceState::Persisted)
        ));
        assert!(!app.ssh_pending_closes.contains_key(&fixture.workspace));
        fixture.client.disconnect();
        // A close implementation may also retire the transport immediately.
        let _ = fixture.output.send(SshHostEvent::Offline {
            generation: fixture.generation,
        });

        app.poll_ssh_workspace_hosts();
        app.poll_ssh_reconnects(Instant::now() + Duration::from_secs(60));

        assert!(
            app.ssh_hosts
                .get(&fixture.workspace)
                .is_none_or(|owner| owner.host.generation() == fixture.generation),
            "a confirmed user close must not start another SSH attachment"
        );
        assert!(app.workspace_add_ui.is_none());
    });
}

#[test]
fn ssh_reconnect_authentication_key_expiry_and_helper_failures_require_user_action() {
    use crate::ssh_workspace_add::SshWorkspaceError;
    for error in [
        SshWorkspaceError::AuthenticationFailed,
        SshWorkspaceError::HostKeyChanged,
        SshWorkspaceError::SessionExpired,
        SshWorkspaceError::HelperFailed,
    ] {
        ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
            let fixture = install_established_host(app);
            fixture.client.disconnect();
            send_failure(&fixture, error);

            app.poll_ssh_workspace_hosts();
            app.poll_ssh_reconnects(Instant::now() + Duration::from_secs(60));

            let owner = &app.ssh_hosts[&fixture.workspace];
            assert_eq!(owner.host.generation(), fixture.generation, "{error:?}");
            assert!(!owner.reconnect.automatic(), "{error:?}");
            assert_eq!(owner.reconnect.delay(Instant::now()), None, "{error:?}");
            assert!(
                app.workspace_add_ui.is_some(),
                "{error:?} must be available for explicit recovery"
            );
        });
    }
}

#[test]
fn ssh_reconnect_stale_generation_failure_and_offline_events_do_not_change_current_owner() {
    ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
        let fixture = install_established_host(app);
        let stale = fixture.generation.checked_add(1).unwrap();
        fixture
            .output
            .send(SshHostEvent::Ui(
                crate::ssh_workspace_add::SshWorkspaceSnapshot {
                    generation: stale,
                    update: crate::ssh_workspace_add::SshWorkspaceUpdate::Failed(
                        crate::ssh_workspace_add::SshWorkspaceError::AuthenticationFailed,
                    ),
                },
            ))
            .unwrap();
        fixture
            .output
            .send(SshHostEvent::Offline { generation: stale })
            .unwrap();

        app.poll_ssh_workspace_hosts();
        app.poll_ssh_reconnects(Instant::now() + Duration::from_secs(60));

        let owner = &app.ssh_hosts[&fixture.workspace];
        assert_eq!(owner.host.generation(), fixture.generation);
        assert!(owner.reconnect.automatic());
        assert_eq!(owner.reconnect.delay(Instant::now()), None);
        assert!(app.active.runtime.is_connected());
        assert!(app.workspace_add_ui.is_none());
    });
}

#[test]
fn ssh_reconnect_cancel_and_close_intents_discard_queued_retry_events() {
    for close in [false, true] {
        ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
            let fixture = install_established_host(app);
            fixture.client.disconnect();
            send_failure(
                &fixture,
                crate::ssh_workspace_add::SshWorkspaceError::ConnectionFailed,
            );
            app.ssh_modal_workspace = Some(fixture.workspace.clone());
            let intent = if close {
                crate::ssh_workspace_add::SshWorkspaceIntent::Close {
                    generation: fixture.generation,
                }
            } else {
                crate::ssh_workspace_add::SshWorkspaceIntent::Cancel {
                    generation: fixture.generation,
                }
            };
            app.handle_ssh_workspace_intent(intent);

            app.poll_ssh_workspace_hosts();
            app.poll_ssh_reconnects(Instant::now() + Duration::from_secs(60));

            assert!(!app.ssh_hosts.contains_key(&fixture.workspace));
            assert!(app.ssh_suppressed_workspaces.contains(&fixture.workspace));
            assert!(app.workspace_add_ui.is_none());
        });
    }
}

#[test]
fn ssh_reconnect_success_rotates_attachment_owner_without_opening_agent_launcher() {
    ssh_integration_tests::with_connected_ssh_app_fixture(|app, _| {
        let fixture = install_established_host(app);
        let old_instance = app.active.runtime_instance;
        let generation = fixture.client.connection_generation();
        let (host, output) = SshWorkspaceHost::event_fixture(generation);
        let owner = app.ssh_hosts.get_mut(&fixture.workspace).unwrap();
        owner.host = host;
        owner.authentication_ui = None;
        owner.connection_pending = true;
        app.active.runtime = WorkspaceRuntimeClient::offline();
        app.agent_launcher_ui = ui::agent_launcher::AgentLauncherUi::new();
        output
            .send(SshHostEvent::Connected {
                generation,
                client: Arc::clone(&fixture.client),
                profile: fixture.profile,
                target: fixture.target.clone(),
                home: fixture.target.canonical_root,
                agents: Vec::new(),
            })
            .unwrap();

        app.poll_ssh_workspace_hosts();

        assert_ne!(app.active.runtime_instance, old_instance);
        assert_eq!(
            app.ssh_hosts[&fixture.workspace].runtime_instance,
            app.active.runtime_instance
        );
        assert!(app.active.runtime.is_connected());
        assert!(!app.ssh_hosts[&fixture.workspace].connection_pending);
        assert!(app.ssh_hosts[&fixture.workspace].reconnect.automatic());
        assert!(app.workspace_add_ui.is_none());
        assert!(
            !app.agent_launcher_ui.is_open(),
            "automatic resume must return to the existing remote panes"
        );
    });
}
