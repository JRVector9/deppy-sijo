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
