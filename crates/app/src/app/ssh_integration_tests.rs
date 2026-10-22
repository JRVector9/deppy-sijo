//! App integration against the production framed SSH client and a disposable,
//! non-persistent helper-shaped runtime. No GUI, credentials, or live SSH host.
#[cfg(test)]
pub(super) fn with_connected_ssh_app_fixture(run: impl FnOnce(&mut super::App, &std::path::Path)) {
    tests::with_connected_ssh_app_fixture(run);
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use runtime::RuntimeEventStream as _;
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    struct NoCredentials;
    impl runtime::RuntimeSecretResolver for NoCredentials {
        fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
            anyhow::bail!("SSH App fixture has no credentials")
        }
    }

    struct StalledFixtureCredentials {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl runtime::RuntimeSecretResolver for StalledFixtureCredentials {
        fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
            let _ = self.entered.try_send(());
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            anyhow::bail!("fixture command stall has no credentials")
        }
    }
    struct ReleaseFixtureStall(std::sync::mpsc::SyncSender<()>);
    impl Drop for ReleaseFixtureStall {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    struct RemoteBackend {
        runtime: Arc<runtime::InProcessRuntimeClient>,
        config: runtime::SshServerConfig,
        root: PathBuf,
    }

    #[derive(Default)]
    struct WriteGate {
        paused: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }
    impl WriteGate {
        fn pause(&self) {
            *self.paused.lock().unwrap() = true;
        }
        fn release(&self) {
            *self.paused.lock().unwrap() = false;
            self.changed.notify_all();
        }
    }
    struct FixtureWriter {
        stream: UnixStream,
        gate: Option<Arc<WriteGate>>,
    }
    impl Write for FixtureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(gate) = &self.gate {
                let guard = gate.paused.lock().unwrap();
                drop(gate.changed.wait_while(guard, |paused| *paused).unwrap());
            }
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.stream.flush()
        }
    }

    impl RemoteBackend {
        fn new(target: &SshWorkspaceTarget, root: &Path) -> Self {
            Self::with_resolver(target, root, Arc::new(NoCredentials))
        }

        fn with_resolver(
            target: &SshWorkspaceTarget,
            root: &Path,
            resolver: Arc<dyn runtime::RuntimeSecretResolver>,
        ) -> Self {
            std::fs::create_dir_all(root).unwrap();
            let runtime = Arc::new(
                runtime::InProcessRuntimeClient::try_new_with_resolver(
                    5,
                    resolver,
                    root.join("helper-logs"),
                    secret::RedactionService::new(),
                    // Production remote-host deliberately has no local persistence DB.
                    None,
                    Some(root.to_owned()),
                    vec![],
                )
                .unwrap(),
            );
            let config = runtime::SshServerConfig::new(
                runtime::SshTargetIdentity {
                    remote_workspace_id: target.remote_workspace_id.clone(),
                    account: target.account.clone(),
                    canonical_root: target.canonical_root.as_str().to_owned(),
                    host_key_sha256: target.host_key_sha256.clone(),
                },
                runtime.instance_token().to_owned(),
            );
            Self {
                runtime,
                config,
                root: root.to_owned(),
            }
        }

        fn attach(&self, generation: u64, resume: Option<runtime::SshResume>) -> RemoteAttachment {
            self.attach_with_gate(generation, resume, None)
        }

        fn attach_with_gate(
            &self,
            generation: u64,
            resume: Option<runtime::SshResume>,
            gate: Option<Arc<WriteGate>>,
        ) -> RemoteAttachment {
            let mut config = self.config.clone();
            config.require_resume = resume.is_some();
            let mut hello = runtime::SshClientHello::new(config.identity.clone(), generation);
            hello.resume = resume;
            let (peer, server) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(45)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(45)))
                .unwrap();
            let server_close = server.try_clone().unwrap();
            let fixture_close = server.try_clone().unwrap();
            let backend = Arc::clone(&self.runtime);
            let server_thread = thread::spawn(move || {
                runtime::serve_ssh_stream(
                    &backend,
                    server.try_clone().unwrap(),
                    server,
                    &config,
                    Arc::new(|_, bytes| Ok(bytes.to_vec())),
                    Arc::new(move || {
                        let _ = server_close.shutdown(Shutdown::Both);
                    }),
                )
            });
            let close = peer.try_clone().unwrap();
            let interrupt_gate = gate.clone();
            let interrupts = Arc::new(AtomicUsize::new(0));
            let interrupt_result = Arc::new(AtomicI32::new(i32::MIN));
            let observed_interrupts = Arc::clone(&interrupts);
            let observed_result = Arc::clone(&interrupt_result);
            let client = Arc::new(
                runtime::SshRuntimeClient::connect(
                    peer.try_clone().unwrap(),
                    FixtureWriter { stream: peer, gate },
                    hello,
                    Arc::new(move || {
                        let result = close
                            .shutdown(Shutdown::Both)
                            .err()
                            .map_or(0, |error| error.raw_os_error().unwrap_or(-1));
                        observed_result.store(result, Ordering::Release);
                        observed_interrupts.fetch_add(1, Ordering::Release);
                        if let Some(gate) = &interrupt_gate {
                            gate.release();
                        }
                    }),
                )
                .unwrap(),
            );
            RemoteAttachment {
                client,
                server_thread: Some(server_thread),
                fixture_close,
                interrupts,
                interrupt_result,
            }
        }
    }

    struct RemoteAttachment {
        client: Arc<runtime::SshRuntimeClient>,
        server_thread: Option<JoinHandle<anyhow::Result<()>>>,
        fixture_close: UnixStream,
        interrupts: Arc<AtomicUsize>,
        interrupt_result: Arc<AtomicI32>,
    }
    impl RemoteAttachment {
        fn disconnect(&mut self) {
            self.client.disconnect();
            let until = Instant::now() + Duration::from_secs(1);
            while self.interrupts.load(Ordering::Acquire) == 0 && Instant::now() < until {
                thread::sleep(Duration::from_millis(1));
            }
            // Capture the client result BEFORE the independent fixture cleanup;
            // it must not conceal a broken production interrupt callback.
            let interrupts = self.interrupts.load(Ordering::Acquire);
            let interrupt_result = self.interrupt_result.load(Ordering::Acquire);
            let _ = self.fixture_close.shutdown(Shutdown::Both);
            if let Some(server) = self.server_thread.take() {
                let until = Instant::now() + Duration::from_secs(2);
                while !server.is_finished() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    server.is_finished(),
                    "fixture SSH server failed bounded cleanup"
                );
                let _ = server.join().unwrap();
            }
            assert!(
                interrupts > 0,
                "SSH client disconnect callback was not invoked"
            );
            assert_eq!(interrupt_result, 0, "SSH client socket shutdown failed");
        }
    }
    impl Drop for RemoteAttachment {
        fn drop(&mut self) {
            self.disconnect();
        }
    }

    fn install_remote_attachment(app: &mut App, client: &Arc<runtime::SshRuntimeClient>) {
        app.active.runtime.shutdown();
        app.active.runtime = WorkspaceRuntimeClient::remote(Arc::clone(client));
        app.active.events = App::subscribe_runtime_events(&app.active.runtime, &app.egui_ctx);
    }

    pub(super) fn with_connected_ssh_app_fixture(run: impl FnOnce(&mut App, &Path)) {
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let WorkspaceTarget::Ssh(target) = app.workspace_targets[&app.active.id].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let backend = RemoteBackend::new(&target, &root);
        let attachment = backend.attach(7, None);
        install_remote_attachment(&mut app, &attachment.client);
        run(&mut app, &root);
    }

    fn input_recorder_command() -> runtime::RuntimeCommand {
        runtime::RuntimeCommand::SpawnAgent {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                agent_config_id: None,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '%s\\n' \"$$\" > recorder.pid; /bin/stty -icanon -echo; : > ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> input.txt; done"
                        .into(),
                ],
                env_plain: vec![],
                env_secrets: vec![],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
                transient_args: vec![],
        }
    }

    fn spawn_input_recorder(client: &runtime::SshRuntimeClient, root: &Path) -> runtime::SessionId {
        let events = client.subscribe_with_wake_background(Arc::new(|| {}));
        client
            .send_command_owned(input_recorder_command())
            .map_err(|(error, _)| error)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            for event in events.drain() {
                if let runtime::RuntimeEvent::AgentSpawned { session } = event {
                    let ready_until = Instant::now() + Duration::from_secs(5);
                    while !root.join("ready").is_file() {
                        assert!(Instant::now() < ready_until, "remote recorder not ready");
                        thread::sleep(Duration::from_millis(5));
                    }
                    return session;
                }
            }
            assert!(Instant::now() < until, "remote recorder did not spawn");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn attach_app_dashboard(app: &mut App) -> web_remote::dashboard::DashboardHandle {
        let server = web_remote::WebRemoteServer::serve(
            "127.0.0.1:0".parse().unwrap(),
            web_remote::ServeOptions {
                token: "isolated-ssh-app-fixture".into(),
                allowed_host: None,
                repository: None,
                vapid: None,
                uploads_dir: None,
            },
        )
        .unwrap();
        let dashboard = server.core().dashboard().clone();
        app.web = Some(WebRemoteState {
            server,
            token: "isolated-ssh-app-fixture".into(),
        });
        app.rebind_web_dashboard();
        dashboard
    }

    fn wait_watchable_session(
        dashboard: &web_remote::dashboard::DashboardHandle,
        workspace: &str,
    ) -> String {
        wait_watchable_session_after(dashboard, workspace, 0)
    }

    fn wait_watchable_session_after(
        dashboard: &web_remote::dashboard::DashboardHandle,
        workspace: &str,
        previous_version: u64,
    ) -> String {
        let until = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        loop {
            if let Some((_, frame)) = dashboard.dashboard_if_newer(previous_version) {
                let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
                if let Some(active) = value["workspaces"].as_array().and_then(|rows| {
                    rows.iter().find(|row| {
                        row["id"].as_str() == Some(workspace)
                            && row["state"].as_str() == Some("active")
                    })
                }) && let Some(session) =
                    active["sessions"].as_array().and_then(|rows| rows.first())
                {
                    let id = session["id"].as_str().expect(
                        "live SSH sessions must expose a PWA watch UUID without a persistence DB",
                    );
                    uuid::Uuid::parse_str(id).expect("PWA session identity must be a UUID");
                    return id.to_owned();
                }
                last = frame;
            }
            assert!(Instant::now() < until, "missing remote PWA session: {last}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_recorded(root: &Path, expected: &str) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let actual = std::fs::read_to_string(root.join("input.txt")).unwrap_or_default();
            if actual == expected {
                return;
            }
            assert!(
                Instant::now() < until,
                "remote input mismatch: expected {expected:?}, got {actual:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ssh_workspace_actual_app_pwa_can_watch_and_input_nonpersistent_helper_session() {
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let WorkspaceTarget::Ssh(target) = app.workspace_targets[&app.active.id].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let backend = RemoteBackend::new(&target, &root);
        let attachment = backend.attach(7, None);
        install_remote_attachment(&mut app, &attachment.client);
        spawn_input_recorder(&attachment.client, &backend.root);
        let dashboard = attach_app_dashboard(&mut app);
        let connection = dashboard.register_connection();
        let id = wait_watchable_session(&dashboard, &app.active.id);
        let generation = dashboard.rebind_watch_for_connection(None, Some(&id), 0);
        assert!(dashboard.watch_binding_is_current(&id, generation));
        let text = "원격 PWA 한글 입력과 공백 🙂 ".repeat(128);
        dashboard.send_input_for_connection(&id, generation, &text, true);
        wait_recorded(&backend.root, &format!("{text}\n"));
        resize_watched_session(&dashboard, connection.id(), &id, generation, 1, (97, 31));
    }

    fn resize_watched_session(
        dashboard: &web_remote::dashboard::DashboardHandle,
        connection: u64,
        id: &str,
        generation: u64,
        request: u32,
        geometry: (u16, u16),
    ) {
        dashboard.resize_control_for_connection(
            connection,
            id,
            generation,
            web_remote::protocol::ResizeControlAction::Acquire,
            request,
        );
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((_, frame, _)) = dashboard.terminal_control_if_newer(connection, 0) {
                let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
                if value["request"].as_u64() == Some(request as u64)
                    && value["owned"].as_bool() == Some(true)
                {
                    break;
                }
            }
            assert!(
                Instant::now() < until,
                "remote PWA resize control not acquired"
            );
            thread::sleep(Duration::from_millis(5));
        }
        dashboard
            .resize_for_connection(connection, id, generation, request, geometry.0, geometry.1);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((_, snapshot)) = dashboard.viewport_if_newer(id, 0)
                && (snapshot.cols, snapshot.rows) == geometry
            {
                return;
            }
            assert!(Instant::now() < until, "remote PWA geometry not applied");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn prepare_warm_remote(
        app: &mut App,
        workspace: &str,
        client: &Arc<runtime::SshRuntimeClient>,
    ) {
        let instance = app.next_runtime_instance;
        app.next_runtime_instance += 1;
        let mut warm = App::make_runtime(
            &app.config,
            &app.logs_base,
            workspace,
            instance,
            &app.db_path,
            app.runtime_host_factory.as_ref(),
            &app.db,
            &app.egui_ctx,
        );
        warm.runtime.shutdown();
        warm.runtime = WorkspaceRuntimeClient::remote(Arc::clone(client));
        warm.events = App::subscribe_runtime_events(&warm.runtime, &app.egui_ctx);
        app.warm.insert(workspace.to_owned(), warm);
        app.warm_order.push(workspace.to_owned());
    }

    #[test]
    fn ssh_workspace_actual_app_pwa_switch_scopes_reused_session_ids_and_old_watches() {
        use deppy_core::workspace_target::RemotePath;
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let workspace_a = app.active.id.clone();
        let WorkspaceTarget::Ssh(target_a) = app.workspace_targets[&workspace_a].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let root_b = root.parent().unwrap().join("other-remote-root");
        std::fs::create_dir(&root_b).unwrap();
        let mut target_b = target_a.clone();
        target_b.host = "other-server.example".into();
        target_b.remote_workspace_id = "other-remote-workspace".into();
        target_b.canonical_root = RemotePath::new(root_b.to_str().unwrap()).unwrap();
        let workspace_b = app
            .db
            .find_or_create_ssh_workspace("Other SSH target", &target_b)
            .unwrap()
            .row
            .row
            .id;
        app.refresh_workspaces();
        let backend_a = RemoteBackend::new(&target_a, &root);
        let backend_b = RemoteBackend::new(&target_b, &root_b);
        let attachment_a = backend_a.attach(7, None);
        let attachment_b = backend_b.attach(7, None);
        install_remote_attachment(&mut app, &attachment_a.client);
        prepare_warm_remote(&mut app, &workspace_b, &attachment_b.client);
        let session_a = spawn_input_recorder(&attachment_a.client, &backend_a.root);
        let session_b = spawn_input_recorder(&attachment_b.client, &backend_b.root);
        assert_eq!(
            session_a, session_b,
            "independent helpers intentionally reuse numeric IDs"
        );
        let token_a = app.active.runtime.instance_token().to_owned();
        let dashboard = attach_app_dashboard(&mut app);
        let connection = dashboard.register_connection();
        let id_a = wait_watchable_session(&dashboard, &workspace_a);
        let watch_a = dashboard.rebind_watch_for_connection(None, Some(&id_a), 0);
        dashboard.send_input_for_connection(&id_a, watch_a, "대상 A 첫 입력", true);
        wait_recorded(&backend_a.root, "대상 A 첫 입력\n");

        app.switch_workspace(&workspace_b);
        assert_eq!(app.active.id, workspace_b);
        assert_ne!(app.active.runtime.instance_token(), token_a);
        let id_b = wait_watchable_session(&dashboard, &workspace_b);
        assert_ne!(
            id_a, id_b,
            "same numeric IDs from distinct helpers must never alias"
        );
        assert!(!dashboard.watch_binding_is_current(&id_a, watch_a));
        dashboard.send_input_for_connection(&id_a, watch_a, "WRONG TARGET", true);
        let watch_b = dashboard.rebind_watch_for_connection(None, Some(&id_b), 0);
        dashboard.send_input_for_connection(&id_b, watch_b, "대상 B 입력", true);
        wait_recorded(&backend_b.root, "대상 B 입력\n");
        resize_watched_session(&dashboard, connection.id(), &id_b, watch_b, 1, (103, 29));

        app.switch_workspace(&workspace_a);
        assert_eq!(app.active.id, workspace_a);
        assert_eq!(app.active.runtime.instance_token(), token_a);
        assert_eq!(wait_watchable_session(&dashboard, &workspace_a), id_a);
        assert!(!dashboard.watch_binding_is_current(&id_b, watch_b));
        assert!(!dashboard.watch_binding_is_current(&id_a, watch_a));
        dashboard.send_input_for_connection(&id_b, watch_b, "WRONG RETURN TARGET", true);
        dashboard.send_input_for_connection(&id_a, watch_a, "STALE WATCH", true);
        let fresh_a = dashboard.rebind_watch_for_connection(None, Some(&id_a), 0);
        assert_ne!(fresh_a, watch_a);
        dashboard.send_input_for_connection(&id_a, fresh_a, "대상 A 둘째 입력", true);
        wait_recorded(&backend_a.root, "대상 A 첫 입력\n대상 A 둘째 입력\n");
        assert_eq!(
            std::fs::read_to_string(backend_b.root.join("input.txt")).unwrap(),
            "대상 B 입력\n"
        );
    }

    #[test]
    fn ssh_workspace_actual_app_pwa_reattach_keeps_session_uuid_but_revokes_old_watch_without_replay()
     {
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let WorkspaceTarget::Ssh(target) = app.workspace_targets[&app.active.id].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let backend = RemoteBackend::new(&target, &root);
        let mut first = backend.attach(77, None);
        install_remote_attachment(&mut app, &first.client);
        let session = spawn_input_recorder(&first.client, &backend.root);
        let token_before = app.active.runtime.instance_token().to_owned();
        let server_before = first.client.instance_token().to_owned();
        let dashboard = attach_app_dashboard(&mut app);
        let connection = dashboard.register_connection();
        let id = wait_watchable_session(&dashboard, &app.active.id);
        let old_watch = dashboard.rebind_watch_for_connection(None, Some(&id), 0);
        dashboard.send_input_for_connection(&id, old_watch, "재연결 전 입력", true);
        wait_recorded(&backend.root, "재연결 전 입력\n");
        let resume = first.client.resume();
        first.disconnect();
        let next = backend.attach(1, Some(resume));
        assert_eq!(next.client.instance_token(), server_before);
        install_remote_attachment(&mut app, &next.client);
        assert_ne!(app.active.runtime.instance_token(), token_before);
        let previous_version = dashboard.dashboard_if_newer(0).unwrap().0;
        app.rebind_web_dashboard();
        assert!(!dashboard.watch_binding_is_current(&id, old_watch));
        let after = wait_watchable_session_after(&dashboard, &app.active.id, previous_version);
        assert_eq!(
            after, id,
            "the same retained server session must keep its UUID"
        );
        dashboard.send_input_for_connection(&id, old_watch, "OLD ATTACHMENT", true);
        let fresh_watch = dashboard.rebind_watch_for_connection(None, Some(&id), 0);
        assert_ne!(old_watch, fresh_watch);
        dashboard.send_input_for_connection(&id, fresh_watch, "재연결 후 입력", true);
        wait_recorded(&backend.root, "재연결 전 입력\n재연결 후 입력\n");
        resize_watched_session(&dashboard, connection.id(), &id, fresh_watch, 1, (111, 33));
        // The existing PTY is still the only session; reconnect sends no spawn or input replay.
        let events = next.client.subscribe_with_wake_background(Arc::new(|| {}));
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(ids) = events.drain().into_iter().find_map(|event| {
                if let runtime::RuntimeEvent::MuxUpdated { snapshot } = event {
                    Some(
                        snapshot
                            .tabs
                            .iter()
                            .flat_map(|tab| &tab.panes)
                            .filter_map(|pane| pane.session_id)
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                }
            }) {
                assert_eq!(ids, vec![session]);
                break;
            }
            assert!(Instant::now() < until, "retained remote mux not observed");
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct WarmCloseFixture {
        app: App,
        backend: RemoteBackend,
        gate: Arc<WriteGate>,
        attachment: RemoteAttachment,
        workspace_a: String,
        session: runtime::SessionId,
        recorder_pid: i32,
    }

    fn warm_close_fixture() -> WarmCloseFixture {
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let workspace_a = app.active.id.clone();
        let WorkspaceTarget::Ssh(target_a) = app.workspace_targets[&workspace_a].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let workspace_b = app
            .workspace_targets
            .iter()
            .find_map(|(id, target)| {
                (id != &workspace_a && matches!(target, WorkspaceTarget::Ssh(_)))
                    .then(|| id.clone())
            })
            .expect("fixture contains a second SSH workspace");
        let backend = RemoteBackend::new(&target_a, &root);
        let gate = Arc::new(WriteGate::default());
        let attachment = backend.attach_with_gate(7, None, Some(Arc::clone(&gate)));
        install_remote_attachment(&mut app, &attachment.client);
        let session = spawn_input_recorder(&attachment.client, &backend.root);
        // Apply the real wire snapshot so this exercises the close command's
        // delivery, rather than an App that has not learned its pane IDs yet.
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let events = app.active.events.drain();
            app.active
                .workspace_ui
                .apply_warm_events(&events, &app.i18n);
            if app.active.workspace_ui.mux().is_some_and(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.session_id == Some(session))
            }) {
                break;
            }
            assert!(
                Instant::now() < until,
                "App did not observe its live remote pane"
            );
            thread::sleep(Duration::from_millis(5));
        }
        app.switch_workspace(&workspace_b);
        assert_eq!(app.active.id, workspace_b);
        assert!(
            app.warm[&workspace_a]
                .workspace_ui
                .mux()
                .unwrap()
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.session_id == Some(session))
        );
        let recorder_pid: i32 = std::fs::read_to_string(root.join("recorder.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(recorder_pid > 1);
        assert_eq!(
            unsafe { libc::kill(recorder_pid, 0) },
            0,
            "fixture PTY must be alive before close"
        );
        WarmCloseFixture {
            app,
            backend,
            gate,
            attachment,
            workspace_a,
            session,
            recorder_pid,
        }
    }

    fn pump_warm_close(app: &mut App, workspace: &str) {
        if let Some(warm) = app.warm.get_mut(workspace) {
            let events = warm.events.drain();
            if let Some(pending) = app.ssh_pending_closes.get_mut(workspace) {
                pending.observe_events(
                    warm.runtime_instance,
                    warm.workspace_ui.mux(),
                    &events,
                    !warm.events.take_overflowed()
                        && !warm.event_overflow_pending
                        && !warm.events.is_disconnected(),
                );
            }
            warm.workspace_ui.apply_warm_events(&events, &app.i18n);
        }
        app.poll_ssh_workspace_closes(Instant::now());
    }

    #[test]
    fn ssh_workspace_actual_app_warm_close_reaches_remote_pty_before_detaching() {
        let WarmCloseFixture {
            mut app,
            backend,
            gate,
            attachment,
            workspace_a,
            session,
            recorder_pid,
        } = warm_close_fixture();
        let remote_events = backend.runtime.subscribe();
        gate.pause();
        // A slow SSH writer is a normal bounded-queue condition. Closing must
        // not disconnect and discard its accepted ClosePane command.
        app.close_workspace_sessions(&workspace_a);
        assert!(app.warm.contains_key(&workspace_a));
        assert!(app.ssh_pending_closes.contains_key(&workspace_a));
        assert!(attachment.client.is_connected());
        gate.release();
        backend
            .runtime
            .send_command(runtime::RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut empty = false;
        loop {
            pump_warm_close(&mut app, &workspace_a);
            for event in remote_events.drain() {
                if let runtime::RuntimeEvent::MuxUpdated { snapshot } = event {
                    empty = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .all(|pane| pane.session_id != Some(session));
                }
            }
            let exited = unsafe { libc::kill(recorder_pid, 0) } != 0;
            if exited
                && empty
                && !app.ssh_pending_closes.contains_key(&workspace_a)
                && !app.warm.contains_key(&workspace_a)
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "closing a warm SSH workspace detached before its remote PTY exited (exited={exited}, empty={empty})"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ssh_workspace_actual_app_warm_close_includes_a_spawn_admitted_before_capture() {
        let WarmCloseFixture {
            mut app,
            backend,
            gate,
            attachment,
            workspace_a,
            session,
            recorder_pid,
        } = warm_close_fixture();
        let remote_events = backend.runtime.subscribe();
        let client_events = attachment
            .client
            .subscribe_with_wake_background(Arc::new(|| {}));
        gate.pause();
        // The bounded client accepts this spawn before Close Workspace starts,
        // but the App cannot capture its new pane until the writer progresses.
        attachment
            .client
            .send_command_owned(input_recorder_command())
            .map_err(|(error, _)| error)
            .unwrap();
        assert!(app.close_workspace_sessions(&workspace_a));
        assert!(app.ssh_pending_closes.contains_key(&workspace_a));
        assert!(app.warm.contains_key(&workspace_a));
        gate.release();

        let until = Instant::now() + Duration::from_secs(5);
        // Let the admitted spawn materialize before App event polling resumes,
        // so even a correct close can prove which real process it must retire.
        let late_pid = loop {
            if let Ok(text) = std::fs::read_to_string(backend.root.join("recorder.pid"))
                && let Ok(pid) = text.trim().parse::<i32>()
                && pid > 1
                && pid != recorder_pid
            {
                break pid;
            }
            assert!(
                Instant::now() < until,
                "pre-close admitted PTY did not start"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(unsafe { libc::kill(late_pid, 0) }, 0);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            // This subscriber follows the App receiver in the same dispatch.
            // Observing the new pane proves App polling can consume it now.
            if client_events.drain().into_iter().any(|event| {
                matches!(event, runtime::RuntimeEvent::MuxUpdated { snapshot }
                    if snapshot.tabs.iter().flat_map(|tab| &tab.panes)
                        .any(|pane| pane.session_id.is_some_and(|id| id != session)))
            }) {
                break;
            }
            assert!(
                Instant::now() < until,
                "new remote pane was not dispatched to the App attachment"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let until = Instant::now() + Duration::from_secs(5);
        let mut empty = false;
        loop {
            pump_warm_close(&mut app, &workspace_a);
            for event in remote_events.drain() {
                if let runtime::RuntimeEvent::MuxUpdated { snapshot } = event {
                    empty = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .next()
                        .is_none();
                }
            }
            let original_exited = unsafe { libc::kill(recorder_pid, 0) } != 0;
            let late_exited = unsafe { libc::kill(late_pid, 0) } != 0;
            let retired = !app.ssh_pending_closes.contains_key(&workspace_a)
                && !app.warm.contains_key(&workspace_a);
            if original_exited && late_exited && empty && retired {
                break;
            }
            assert!(
                Instant::now() < until,
                "Close Workspace missed a pre-close admitted spawn (late_pid={late_pid}, original_exited={original_exited}, late_exited={late_exited}, empty={empty}, retired={retired})"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ssh_workspace_actual_app_empty_warm_close_waits_for_preclose_spawn_acknowledgement() {
        let WarmCloseFixture {
            mut app,
            backend,
            gate,
            attachment,
            workspace_a,
            session,
            recorder_pid,
        } = warm_close_fixture();
        let pane = app.warm[&workspace_a]
            .workspace_ui
            .mux()
            .unwrap()
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| pane.session_id == Some(session))
            .unwrap()
            .id
            .clone();
        attachment
            .client
            .send_command_owned(runtime::RuntimeCommand::ClosePane { pane })
            .map_err(|(error, _)| error)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            pump_warm_close(&mut app, &workspace_a);
            if app.warm[&workspace_a]
                .workspace_ui
                .mux()
                .is_some_and(|mux| mux.tabs.iter().flat_map(|tab| &tab.panes).next().is_none())
                && unsafe { libc::kill(recorder_pid, 0) } != 0
            {
                break;
            }
            assert!(Instant::now() < until, "baseline PTY did not close");
            thread::sleep(Duration::from_millis(5));
        }
        let remote_events = backend.runtime.subscribe();
        let client_events = attachment
            .client
            .subscribe_with_wake_background(Arc::new(|| {}));
        gate.pause();
        app.warm[&workspace_a]
            .runtime
            .send_command_owned(input_recorder_command())
            .map_err(|(error, _)| error)
            .unwrap();
        assert!(app.close_workspace_sessions(&workspace_a));
        // A cached empty mux predates the admitted spawn. No command can reach
        // the server while paused, so it cannot acknowledge this close yet.
        app.poll_ssh_workspace_closes(Instant::now());
        assert!(app.ssh_pending_closes.contains_key(&workspace_a));
        assert!(app.warm.contains_key(&workspace_a));
        assert!(attachment.client.is_connected());
        gate.release();

        let until = Instant::now() + Duration::from_secs(5);
        let late_pid = loop {
            if let Ok(text) = std::fs::read_to_string(backend.root.join("recorder.pid"))
                && let Ok(pid) = text.trim().parse::<i32>()
                && pid > 1
                && pid != recorder_pid
            {
                break pid;
            }
            assert!(Instant::now() < until, "admitted PTY did not start");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(unsafe { libc::kill(late_pid, 0) }, 0);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if client_events.drain().into_iter().any(|event| {
                matches!(event, runtime::RuntimeEvent::MuxUpdated { snapshot }
                    if snapshot.tabs.iter().flat_map(|tab| &tab.panes)
                        .any(|pane| pane.session_id.is_some_and(|id| id != session)))
            }) {
                break;
            }
            assert!(Instant::now() < until, "admitted pane was not dispatched");
            thread::sleep(Duration::from_millis(5));
        }
        let until = Instant::now() + Duration::from_secs(5);
        let mut empty = false;
        loop {
            pump_warm_close(&mut app, &workspace_a);
            for event in remote_events.drain() {
                if let runtime::RuntimeEvent::MuxUpdated { snapshot } = event {
                    empty = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .next()
                        .is_none();
                }
            }
            let late_exited = unsafe { libc::kill(late_pid, 0) } != 0;
            let retired = !app.ssh_pending_closes.contains_key(&workspace_a)
                && !app.warm.contains_key(&workspace_a);
            if late_exited && empty && retired {
                break;
            }
            assert!(
                Instant::now() < until,
                "an empty cached mux retired before the admitted PTY closed (late_exited={late_exited}, empty={empty}, retired={retired})"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ssh_workspace_actual_app_warm_close_deadline_reports_uncertain_without_claiming_pty_exit() {
        let WarmCloseFixture {
            mut app,
            backend,
            gate,
            attachment,
            workspace_a,
            session,
            recorder_pid,
        } = warm_close_fixture();
        let remote_events = backend.runtime.subscribe();
        gate.pause();
        assert!(app.close_workspace_sessions(&workspace_a));
        assert!(app.ssh_pending_closes.contains_key(&workspace_a));
        app.poll_ssh_workspace_closes(Instant::now() + Duration::from_secs(16));
        assert!(!app.ssh_pending_closes.contains_key(&workspace_a));
        assert!(!app.warm.contains_key(&workspace_a));
        assert!(!attachment.client.is_connected());
        let expected = app.i18n.t("workspace.ssh.close_unconfirmed", &[]);
        assert_eq!(
            app.web_notice.as_ref().map(|(text, _)| text),
            Some(&expected)
        );
        assert_eq!(
            unsafe { libc::kill(recorder_pid, 0) },
            0,
            "a close timeout must not claim that the remote PTY exited"
        );
        remote_events.drain();
        backend
            .runtime
            .send_command(runtime::RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if remote_events.drain().into_iter().any(|event| {
                matches!(event, runtime::RuntimeEvent::MuxUpdated { snapshot }
                    if snapshot.tabs.iter().flat_map(|tab| &tab.panes)
                        .any(|pane| pane.session_id == Some(session)))
            }) {
                break;
            }
            assert!(Instant::now() < until, "retained server PTY not observed");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ssh_workspace_actual_app_restarted_close_rejects_old_accepted_barrier() {
        let (mut app, root) = super::super::tests::ssh_workspace_isolated_app_fixture();
        let workspace_a = app.active.id.clone();
        let WorkspaceTarget::Ssh(target) = app.workspace_targets[&workspace_a].clone() else {
            panic!("SSH fixture must select a remote target")
        };
        let workspace_b = app
            .workspace_targets
            .keys()
            .find(|id| **id != workspace_a)
            .unwrap()
            .clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let backend = RemoteBackend::with_resolver(
            &target,
            &root,
            Arc::new(StalledFixtureCredentials {
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
            }),
        );
        let release = ReleaseFixtureStall(release_tx);
        let mut old = backend.attach(7, None);
        install_remote_attachment(&mut app, &old.client);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let events = app.active.events.drain();
            app.active
                .workspace_ui
                .apply_warm_events(&events, &app.i18n);
            if app.active.workspace_ui.mux().is_some() {
                break;
            }
            assert!(Instant::now() < until, "empty remote mux not observed");
            thread::sleep(Duration::from_millis(5));
        }
        // The resolver is a test-owned scheduler gate: no secret is returned and
        // no process is spawned. Hold the backend FIFO while the OLD attachment
        // sends its barrier, rather than fabricating an incoming runtime event.
        let mut stall = input_recorder_command();
        if let runtime::RuntimeCommand::SpawnAgent { env_secrets, .. } = &mut stall {
            env_secrets.push(("FIXTURE_STALL".into(), "fixture-only".into()));
        }
        old.client
            .send_command_owned(stall)
            .map_err(|(error, _)| error)
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        old.client
            .send_command_owned(runtime::RuntimeCommand::DurableEventBarrier { correlation_id: 1 })
            .map_err(|(error, _)| error)
            .unwrap();
        let marker = b"old barrier accepted".to_vec();
        old.client.send_rpc(9001, marker.clone()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reply) = old.client.try_recv_rpc() {
                assert_eq!(reply.context.request_id, 9001);
                assert_eq!(reply.result.unwrap(), marker);
                break;
            }
            assert!(Instant::now() < until, "old barrier acceptance not fenced");
            thread::sleep(Duration::from_millis(5));
        }
        // The later RPC is handled only after the server admitted the old
        // barrier into the blocked backend FIFO. Detach and resume a new bridge.
        let resume = old.client.resume();
        old.disconnect();
        let gate = Arc::new(WriteGate::default());
        let next = backend.attach_with_gate(1, Some(resume), Some(Arc::clone(&gate)));
        install_remote_attachment(&mut app, &next.client);
        app.switch_workspace(&workspace_b);
        let events = next.client.subscribe_with_wake_background(Arc::new(|| {}));
        gate.pause();
        app.warm[&workspace_a]
            .runtime
            .send_command_owned(input_recorder_command())
            .map_err(|(error, _)| error)
            .unwrap();
        assert!(app.close_workspace_sessions(&workspace_a));
        app.poll_ssh_workspace_closes(Instant::now());
        assert!(app.ssh_pending_closes[&workspace_a].barrier.is_some());
        release.0.try_send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if events.drain().into_iter().any(|event| {
                matches!(
                    event,
                    runtime::RuntimeEvent::DurableEventBarrierReached { correlation_id: 1 }
                )
            }) {
                break;
            }
            assert!(
                Instant::now() < until,
                "old accepted barrier did not reach the new bridge"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // The current writer is still paused. Its close barrier cannot have
        // reached the server, even though the old correlation was re-enveloped.
        pump_warm_close(&mut app, &workspace_a);
        assert!(
            app.ssh_pending_closes.contains_key(&workspace_a),
            "an old attachment barrier falsely acknowledged the new close"
        );
        assert!(app.warm.contains_key(&workspace_a));
        assert!(next.client.is_connected());
        assert!(!root.join("recorder.pid").exists());
    }
}
