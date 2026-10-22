//! Opt-in end-to-end proof against scripts/ssh-linux-fixture.sh, never user SSH configuration.
#[cfg(test)]
mod tests {
    use super::super::*;
    use runtime::{RuntimeCommand, RuntimeCommandSink, RuntimeEvent, RuntimeEventStream};

    struct Registration(PathBuf);
    impl SshWorkspaceRegistration for Registration {
        fn register(
            &self,
            name: &str,
            profile: &SshConnectionProfile,
            target: &SshWorkspaceTarget,
        ) -> Result<storage::WorkspaceTargetRow, SshWorkspaceError> {
            let db = storage::Db::open(&self.0).map_err(|_| SshWorkspaceError::RequestFailed)?;
            db.upsert_ssh_connection_profile(profile)
                .map_err(|_| SshWorkspaceError::RequestFailed)?;
            db.find_or_create_ssh_workspace(name, target)
                .map(|result| result.row)
                .map_err(|_| SshWorkspaceError::RequestFailed)
        }
    }
    struct FixtureHost {
        commands: SyncSender<HostCommand>,
        events: Receiver<SshHostEvent>,
        cancellation: Arc<Cancellation>,
        thread: std::thread::JoinHandle<Result<(), SshWorkspaceError>>,
    }
    impl FixtureHost {
        fn start(
            profile: SshConnectionProfile,
            target: Option<SshWorkspaceTarget>,
            db: PathBuf,
            program: PathBuf,
            generation: u64,
        ) -> Self {
            Self::start_with_resources(profile, target, db, program, generation, PathBuf::new())
        }
        fn start_with_resources(
            profile: SshConnectionProfile,
            target: Option<SshWorkspaceTarget>,
            db: PathBuf,
            program: PathBuf,
            generation: u64,
            resources: PathBuf,
        ) -> Self {
            let (commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
            let (output, events) = mpsc::sync_channel(EVENTS_MAX);
            let cancellation = Arc::new(Cancellation {
                cancelled: AtomicBool::new(false),
                interrupt: Mutex::new(None),
            });
            let mut owner = Owner {
                profile,
                generation,
                target,
                db_path: db.clone(),
                resources,
                requests,
                output,
                cancellation: cancellation.clone(),
                wake: Arc::new(|| {}),
                next_request: FIRST_INTERNAL_REQUEST,
                registrar: Arc::new(Registration(db)),
                test_program: Some(program),
            };
            let thread = std::thread::spawn(move || {
                let result = owner.run();
                if let Err(error) = &result {
                    owner.ui(SshWorkspaceUpdate::Failed(*error));
                }
                result
            });
            Self {
                commands,
                events,
                cancellation,
                thread,
            }
        }
        fn wait(&self, mut predicate: impl FnMut(SshHostEvent) -> bool) {
            let until = Instant::now() + Duration::from_secs(20);
            while Instant::now() < until {
                if let Ok(event) = self.events.recv_timeout(Duration::from_millis(20)) {
                    if let SshHostEvent::Ui(SshWorkspaceSnapshot {
                        update: SshWorkspaceUpdate::Failed(error),
                        ..
                    }) = &event
                    {
                        panic!("isolated Linux host failed: {error:?}");
                    }
                    if predicate(event) {
                        return;
                    }
                }
            }
            panic!("isolated Linux host event deadline");
        }
        fn connected(&self) -> (Arc<SshRuntimeClient>, SshWorkspaceTarget) {
            let mut connected = None;
            self.wait(|event| {
                if let SshHostEvent::Connected { client, target, .. } = event {
                    connected = Some((client, target));
                    true
                } else {
                    false
                }
            });
            connected.unwrap()
        }
        fn rpc(&self, id: u64, request: Request) -> Reply {
            self.commands
                .send(HostCommand::Rpc {
                    request_id: id,
                    request,
                })
                .unwrap();
            let mut reply = None;
            self.wait(|event| {
                if let SshHostEvent::Rpc { reply: value, .. } = event {
                    assert_eq!(value.context.request_id, id);
                    reply = Some(decode(value));
                    true
                } else {
                    false
                }
            });
            reply.unwrap()
        }
        fn stop(self) {
            self.cancellation.cancel();
            self.thread.join().unwrap().unwrap();
        }
    }

    #[test]
    #[ignore = "requires a disposable Linux fixture with its leased supervisors stopped and sealed helper artifacts"]
    fn ssh_linux_host_installs_verified_helper_only_after_current_consent() {
        let manifest =
            PathBuf::from(std::env::var_os("DEPPY_SSH_FIXTURE").expect("fixture.json required"));
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let program = PathBuf::from(fixture["ssh_program"].as_str().unwrap());
        let destination = fixture["destination"].as_str().unwrap().to_owned();
        let artifacts = PathBuf::from(
            std::env::var_os("DEPPY_SSH_HELPER_ARTIFACTS")
                .expect("sealed helper artifacts required"),
        );
        let root =
            std::env::temp_dir().join(format!("deppy-linux-install-{}", uuid::Uuid::new_v4()));
        let resources = root.join("Resources");
        let bundled = resources.join("remote-host");
        std::fs::create_dir_all(&bundled).unwrap();
        let encoded = std::fs::read(artifacts.join("manifest.json")).unwrap();
        assert!(encoded.len() <= 64 * 1024);
        let entries: Vec<crate::remote_bootstrap::BundledHelperManifest> =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(entries.len(), 2);
        for label in ["linux-aarch64", "linux-x86_64"] {
            std::fs::create_dir(bundled.join(label)).unwrap();
            std::fs::copy(
                artifacts.join(label).join("deppy-remote-host"),
                bundled.join(label).join("deppy-remote-host"),
            )
            .unwrap();
        }
        std::fs::write(bundled.join("manifest.json"), encoded).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        let installed = format!("$HOME/.local/share/deppy/remote-host/{version}");
        let executable =
            format!("$HOME/.local/share/deppy/remote-host/{version}/deppy-remote-host");
        let command = |script: &str| {
            std::process::Command::new(&program)
                .arg(&destination)
                .arg(script)
                .status()
                .unwrap()
        };
        assert!(
            command(&format!(
                "mv \"{installed}\" \"{installed}.fixture-backup\""
            ))
            .success()
        );
        struct Restore {
            program: PathBuf,
            destination: String,
            installed: String,
            result: String,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::process::Command::new(&self.program)
                    .arg(&self.destination)
                    .arg(format!(
                        "if [ -d \"{installed}\" ]; then mv \"{installed}\" \"{result}\"; fi; mv \"{installed}.fixture-backup\" \"{installed}\"",
                        installed = self.installed, result = self.result,
                    ))
                    .status();
            }
        }
        let restore = Restore {
            program: program.clone(),
            destination: destination.clone(),
            installed: installed.clone(),
            result: format!("{installed}.fixture-new-{}", uuid::Uuid::new_v4()),
        };
        let profile = SshConnectionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            destination: fixture["destination"].as_str().unwrap().into(),
            port: None,
            identity_file: None,
        };
        let host = FixtureHost::start_with_resources(
            profile,
            None,
            root.join("metadata.sqlite3"),
            program.clone(),
            44,
            resources,
        );
        host.wait(|event| {
            matches!(
                event,
                SshHostEvent::Ui(SshWorkspaceSnapshot {
                    update: SshWorkspaceUpdate::InstallRequired { .. },
                    ..
                })
            )
        });
        assert!(
            command(&format!("test ! -e \"{executable}\"")).success(),
            "connection alone must not install a helper"
        );
        host.commands
            .send(HostCommand::Ui(SshWorkspaceIntent::InstallHelper {
                generation: 43,
            }))
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            command(&format!("test ! -e \"{executable}\"")).success(),
            "stale consent must not install a helper"
        );
        host.commands
            .send(HostCommand::Ui(SshWorkspaceIntent::InstallHelper {
                generation: 44,
            }))
            .unwrap();
        let (client, target) = host.connected();
        assert_eq!(target.account, fixture["account"].as_str().unwrap());
        assert!(command(&format!("test -x \"{executable}\"")).success());
        host.stop();
        drop(client);
        drop(restore);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn wait_runtime(
        receiver: &runtime::RuntimeEventReceiver,
        mut predicate: impl FnMut(RuntimeEvent) -> bool,
    ) {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            for event in receiver.drain() {
                if predicate(event) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("isolated Linux runtime event deadline");
    }

    #[test]
    #[ignore = "requires an explicitly supplied disposable Linux/OpenSSH fixture"]
    fn ssh_linux_host_register_files_terminal_and_restart_resume() {
        let manifest = PathBuf::from(
            std::env::var_os("DEPPY_SSH_FIXTURE").expect("fixture.json path required"),
        );
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let program = PathBuf::from(fixture["ssh_program"].as_str().unwrap());
        let directory =
            std::env::temp_dir().join(format!("deppy-linux-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let db_path = directory.join("metadata.sqlite3");
        let profile = SshConnectionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            destination: fixture["destination"].as_str().unwrap().into(),
            port: None,
            identity_file: None,
        };
        let picker = FixtureHost::start(profile.clone(), None, db_path.clone(), program.clone(), 7);
        let (picker_client, picker_target) = picker.connected();
        assert_eq!(picker_target.canonical_root.as_str(), "/");
        assert_eq!(picker_target.host, fixture["host"].as_str().unwrap());
        assert_eq!(picker_target.port, fixture["port"].as_u64().unwrap() as u16);
        assert_eq!(picker_target.account, fixture["account"].as_str().unwrap());
        assert_eq!(
            picker_target.host_key_sha256,
            fixture["host_key_sha256"].as_str().unwrap()
        );
        let root = RemotePath::new(fixture["workspace"].as_str().unwrap()).unwrap();
        picker
            .commands
            .send(HostCommand::Ui(SshWorkspaceIntent::Browse {
                generation: 7,
                request_id: 1,
                path: root.clone(),
                cursor: None,
            }))
            .unwrap();
        picker.wait(|event| {
            matches!(event, SshHostEvent::Ui(SshWorkspaceSnapshot {
        update: SshWorkspaceUpdate::Directory { request_id: 1, page, .. }, ..
    }) if page.entries.iter().any(|entry| entry.name == b"document.txt"))
        });
        picker
            .commands
            .send(HostCommand::Ui(SshWorkspaceIntent::VerifyRoot {
                generation: 7,
                request_id: 2,
                path: root.clone(),
            }))
            .unwrap();
        let mut target = None;
        picker.wait(|event| {
            if let SshHostEvent::Ui(SshWorkspaceSnapshot {
                update:
                    SshWorkspaceUpdate::RootVerified {
                        request_id: 2,
                        target: value,
                    },
                ..
            }) = event
            {
                target = Some(value);
                true
            } else {
                false
            }
        });
        let target = target.unwrap();
        assert_eq!(target.canonical_root, root);
        picker
            .commands
            .send(HostCommand::Ui(SshWorkspaceIntent::Add {
                generation: 7,
                name: "Linux fixture".into(),
                profile: profile.clone(),
                target: target.clone(),
            }))
            .unwrap();
        let mut workspace = None;
        picker.wait(|event| {
            if let SshHostEvent::Registered { row, .. } = event {
                assert!(row.row.path.is_empty());
                assert_eq!(
                    row.target,
                    deppy_core::workspace_target::WorkspaceTarget::Ssh(target.clone())
                );
                workspace = Some(row.row.id);
                true
            } else {
                false
            }
        });
        picker.stop();
        drop(picker_client);

        let host = FixtureHost::start(
            profile.clone(),
            Some(target.clone()),
            db_path.clone(),
            program.clone(),
            8,
        );
        let (client, actual) = host.connected();
        assert_eq!(actual, target);
        let file = match host
            .rpc(
                1,
                Request::ReadFile {
                    path: b"document.txt".to_vec(),
                },
            )
            .unwrap()
        {
            Response::File(file) => file,
            _ => panic!("file response expected"),
        };
        let content = "원격 저장 검증: 공백과 한글\n".as_bytes().to_vec();
        assert!(matches!(
            host.rpc(
                2,
                Request::WriteFileIfVersion {
                    path: b"document.txt".to_vec(),
                    expected: file.revision.clone(),
                    content: content.clone(),
                }
            ),
            Ok(Response::Written(_))
        ));
        assert!(
            host.rpc(
                3,
                Request::WriteFileIfVersion {
                    path: b"document.txt".to_vec(),
                    expected: file.revision,
                    content: b"stale".to_vec(),
                }
            )
            .is_err(),
            "CAS must refuse an obsolete revision"
        );
        assert!(
            host.rpc(
                4,
                Request::ReadFile {
                    path: b"../.ssh/authorized_keys".to_vec()
                }
            )
            .is_err()
        );
        assert!(
            matches!(host.rpc(5, Request::GitStatus), Ok(Response::Git(bytes)) if bytes.windows(12).any(|slice| slice == b"document.txt"))
        );
        let receiver = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let mut session = None;
        wait_runtime(&receiver, |event| {
            if let RuntimeEvent::ShellSpawned { session: value } = event {
                session = Some(value);
                true
            } else {
                false
            }
        });
        let session = session.unwrap();
        let marker = format!("no-replay-{}.txt", uuid::Uuid::new_v4());
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: format!("printf '한글 입력 검증 X' >> '{marker}'\n").into_bytes(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let mut id = 6;
        loop {
            if matches!(host.rpc(id, Request::ReadFile { path: marker.as_bytes().to_vec() }), Ok(Response::File(file)) if file.content == "한글 입력 검증 X".as_bytes())
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "remote PTY input did not reach the selected workspace"
            );
            id += 1;
            std::thread::sleep(Duration::from_millis(30));
        }
        client
            .send_command(RuntimeCommand::Resize {
                session,
                cols: 100,
                rows: 30,
            })
            .unwrap();
        wait_runtime(&receiver, |event| {
            matches!(event, RuntimeEvent::Viewport { session: value, snapshot, .. }
        if value == session && snapshot.cols == 100 && snapshot.rows == 30)
        });
        let instance = client.instance_token().to_owned();
        host.stop();
        drop(client);
        // A new owner and generation 1 model an App restart using only the saved private lease.
        let resumed =
            FixtureHost::start(profile, Some(target.clone()), db_path.clone(), program, 1);
        let (client, actual) = resumed.connected();
        assert_eq!(actual, target);
        assert_eq!(client.instance_token(), instance);
        let receiver = client.subscribe();
        wait_runtime(&receiver, |event| {
            matches!(event, RuntimeEvent::MuxUpdated { snapshot }
        if snapshot.tabs.iter().any(|tab| tab.panes.iter().any(|pane| pane.session_id == Some(session))))
        });
        assert!(
            matches!(resumed.rpc(1, Request::ReadFile { path: marker.as_bytes().to_vec() }), Ok(Response::File(file)) if file.content == "한글 입력 검증 X".as_bytes()),
            "reattach must not replay input"
        );
        assert!(
            matches!(resumed.rpc(2, Request::ReadFile { path: b"document.txt".to_vec() }), Ok(Response::File(file)) if file.content == content)
        );
        client
            .send_command(RuntimeCommand::KillSession { session })
            .unwrap();
        wait_runtime(&receiver, |event| {
            matches!(event, RuntimeEvent::MuxUpdated { snapshot }
        if snapshot.tabs.iter().all(|tab| tab.panes.iter().all(|pane| pane.session_id != Some(session))))
        });
        resumed.stop();
        drop(client);
        let db = storage::Db::open(&db_path).unwrap();
        assert_eq!(
            db.workspace_target(&workspace.unwrap()).unwrap(),
            Some(deppy_core::workspace_target::WorkspaceTarget::Ssh(target))
        );
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
