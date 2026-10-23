#![cfg(unix)]
use runtime::{
    RuntimeCommand, RuntimeCommandSink, RuntimeEvent, RuntimeEventStream, SshClientHello,
    SshRuntimeClient, SshTargetIdentity,
};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Fixture {
    home: std::path::PathBuf,
    identity: SshTargetIdentity,
}
impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!(
            "dpsh-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(&home).unwrap();
        let root = std::fs::canonicalize(&home).unwrap();
        let account = Command::new("id").arg("-un").output().unwrap();
        Self {
            home,
            identity: SshTargetIdentity {
                remote_workspace_id: deppy_remote_host::services::workspace_id(
                    String::from_utf8_lossy(&account.stdout).trim(),
                    root.to_str().unwrap(),
                ),
                account: String::from_utf8(account.stdout).unwrap().trim().into(),
                canonical_root: root.to_str().unwrap().into(),
                host_key_sha256: "SHA256:fixture".into(),
            },
        }
    }
    fn attach(
        &self,
        generation: u64,
        resume: Option<runtime::SshResume>,
    ) -> (SshRuntimeClient, Arc<Mutex<Child>>) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_deppy-remote-host"))
            .arg("--stdio")
            .env("HOME", &self.home)
            .env("SHELL", "/bin/sh")
            .env("PATH", "/usr/bin:/bin")
            .env("DEPPY_REMOTE_LEASE_SECONDS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let child = Arc::new(Mutex::new(child));
        let cancel = Arc::clone(&child);
        let mut hello = SshClientHello::new(self.identity.clone(), generation);
        hello.resume = resume;
        let client = SshRuntimeClient::connect(
            output,
            input,
            hello,
            Arc::new(move || {
                if let Ok(mut child) = cancel.lock() {
                    let _ = child.kill();
                }
            }),
        )
        .unwrap();
        (client, child)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
#[track_caller]
fn await_event(
    receiver: &runtime::RuntimeEventReceiver,
    mut check: impl FnMut(&RuntimeEvent) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let events = receiver.drain();
        seen.extend(
            events
                .iter()
                .map(|event| postcard::to_stdvec(event).unwrap()[0]),
        );
        assert!(
            !receiver.is_disconnected(),
            "helper disconnected, seen event tags: {seen:?}"
        );
        for event in &events {
            if let RuntimeEvent::SpawnFailed { message, .. } = event {
                panic!("helper spawn failed: {message:?}");
            }
        }
        if events.iter().any(&mut check) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("helper runtime event deadline; tags: {seen:?}");
}

#[test]
fn helper_reports_workspace_version() {
    let result = Command::new(env!("CARGO_BIN_EXE_deppy-remote-host"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        format!("deppy-remote-host {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn helper_image_rpc_stages_remote_bytes_through_authenticated_stdio() {
    use deppy_remote_host::{
        clipboard::ImageChunk,
        services::{Reply, Request, Response},
    };
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let (mut client, process) = fixture.attach(1, None);
    let data = b"\x89PNG\r\n\x1a\nremote-wire-image-fixture";
    let request = Request::StageClipboardImage(ImageChunk {
        upload_id: uuid::Uuid::new_v4().to_string(),
        offset: 0,
        total: data.len() as u64,
        sha256: Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        extension: "png".into(),
        data: data.to_vec(),
    });
    client
        .send_rpc(51, serde_json::to_vec(&request).unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let reply = loop {
        if let Some(reply) = client.try_recv_rpc() {
            break reply;
        }
        assert!(Instant::now() < deadline, "image RPC deadline");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reply.context.identity, fixture.identity);
    assert_eq!(reply.context.connection_generation, 1);
    assert_eq!(reply.context.request_id, 51);
    let Ok(Response::ClipboardImage(receipt)) =
        serde_json::from_slice::<Reply>(&reply.result.unwrap()).unwrap()
    else {
        panic!("image receipt")
    };
    let path = std::path::PathBuf::from(receipt.path.unwrap());
    assert!(path.starts_with(&fixture.home));
    assert_eq!(std::fs::read(path).unwrap(), data);
    client.shutdown();
    process.lock().unwrap().wait().unwrap();
}

#[test]
fn helper_discovery_and_created_pty_share_the_home_agent_path() {
    use deppy_remote_host::services::{Reply, Request, Response};
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let bin = fixture.home.join(".local/bin");
    let node_bin = fixture.home.join(".nvm/versions/node/v99.0.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&node_bin).unwrap();
    for (path, script) in [
        (
            bin.join("codex"),
            "#!/usr/bin/env deppy-node-fixture\nfixture\n",
        ),
        (
            node_bin.join("deppy-node-fixture"),
            "#!/bin/sh\nprintf inherited-path-ok > \"$HOME/path-proof\"\n",
        ),
    ] {
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (mut client, process) = fixture.attach(1, None);
    client
        .send_rpc(52, serde_json::to_vec(&Request::InspectAgents).unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let reply = loop {
        if let Some(reply) = client.try_recv_rpc() {
            break reply;
        }
        assert!(Instant::now() < deadline, "agent discovery deadline");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reply.context.identity, fixture.identity);
    let Ok(Response::Agents(agents)) =
        serde_json::from_slice::<Reply>(&reply.result.unwrap()).unwrap()
    else {
        panic!("agent catalogue");
    };
    assert!(
        agents.iter().any(|agent| agent.kind == "codex"
            && agent.executable == bin.join("codex").to_str().unwrap())
    );
    let events = client.subscribe();
    client
        .send_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        })
        .unwrap();
    let mut session = None;
    await_event(&events, |event| {
        if let RuntimeEvent::ShellSpawned { session: id } = event {
            session = Some(*id);
            true
        } else {
            false
        }
    });
    client
        .send_command(RuntimeCommand::WriteInput {
            session: session.unwrap(),
            bytes: b"printf '%s\\n' \"$PATH\" \"$HOME\" > path-environment-proof; codex > path-output-proof 2> path-error-proof\n".to_vec(),
        })
        .unwrap();
    let marker = fixture.home.join("path-proof");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read(marker).unwrap_or_else(|error| {
            panic!(
                "missing fixture marker: {error}; environment={:?}; stderr={:?}",
                std::fs::read_to_string(fixture.home.join("path-environment-proof")),
                std::fs::read_to_string(fixture.home.join("path-error-proof"))
            )
        }),
        b"inherited-path-ok"
    );
    client.shutdown();
    process.lock().unwrap().wait().unwrap();
}

#[test]
fn helper_supervisor_retains_pty_across_stdio_exit_and_expires_lease_without_replay() {
    let fixture = Fixture::new();
    let (mut first, process) = fixture.attach(1, None);
    let events = first.subscribe();
    first
        .send_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        })
        .unwrap();
    let mut session = None;
    await_event(&events, |event| {
        if let RuntimeEvent::ShellSpawned { session: id } = event {
            session = Some(*id);
            true
        } else {
            false
        }
    });
    let session = session.unwrap();
    let marker = fixture.home.join("input-count");
    first
        .send_command(RuntimeCommand::WriteInput {
            session,
            bytes: format!("printf X >> '{}'\n", marker.display()).into_bytes(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read(&marker).unwrap(), b"X");
    let resume = first.resume();
    let runtime_id = first.instance_token().to_owned();
    first.shutdown();
    process.lock().unwrap().wait().unwrap();
    let (mut second, process) = fixture.attach(1, Some(resume.clone()));
    assert_eq!(second.instance_token(), runtime_id);
    let events = second.subscribe();
    await_event(
        &events,
        |event| matches!(event, RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.iter().any(|tab| tab.panes.iter().any(|pane| pane.session_id == Some(session)))),
    );
    second
        .send_command(RuntimeCommand::Resize {
            session,
            cols: 91,
            rows: 29,
        })
        .unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), b"X");
    second.shutdown();
    process.lock().unwrap().wait().unwrap();
    std::thread::sleep(Duration::from_millis(2300));
    let mut child = Command::new(env!("CARGO_BIN_EXE_deppy-remote-host"))
        .arg("--stdio")
        .env("HOME", &fixture.home)
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut hello = SshClientHello::new(fixture.identity.clone(), 3);
    hello.resume = Some(resume);
    let bytes = postcard::to_stdvec(&hello).unwrap();
    runtime::write_ssh_frame(child.stdin.as_mut().unwrap(), &bytes, 65536).unwrap();
    drop(child.stdin.take());
    let mut result = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut result)
        .unwrap();
    assert!(
        !child.wait().unwrap().success(),
        "expired resume must not spawn a replacement supervisor"
    );
    let bytes = runtime::read_ssh_frame(&mut std::io::Cursor::new(result), 65536)
        .unwrap()
        .unwrap();
    let rejection =
        postcard::from_bytes::<Result<runtime::SshServerHello, runtime::SshHandshakeError>>(&bytes)
            .unwrap();
    assert!(matches!(
        rejection,
        Err(runtime::SshHandshakeError::ResumeExpired)
    ));
}
