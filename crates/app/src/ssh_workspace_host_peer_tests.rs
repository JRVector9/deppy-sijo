//! Regression fixtures for the boundary between authenticated SSH and helper I/O.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct NoRegistration;
impl SshWorkspaceRegistration for NoRegistration {
    fn register(
        &self,
        _: &str,
        _: &SshConnectionProfile,
        _: &SshWorkspaceTarget,
    ) -> Result<storage::WorkspaceTargetRow, SshWorkspaceError> {
        Err(SshWorkspaceError::RequestFailed)
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from("/tmp").join(format!("deppy-host-peer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_changed_peer_receives_no_resume(host: &str, account: &str, port: u16, key: &str) {
    let scratch = Scratch::new();
    let captured = scratch.0.join("helper-stdin");
    let phases = scratch.0.join("phases");
    std::fs::write(&captured, []).unwrap();
    let program = scratch.0.join("ssh");
    let quoted = |path: &std::path::Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
    let script = format!(
        r#"#!/usr/bin/python3
import os,sys
state={state}
captured={captured}
try:
    with open(state) as stream: phase=int(stream.read())
except FileNotFoundError:
    phase=0
host,account,port,key=('host.test','fixture',22,'{original_key}') if phase==0 else ({host},{account},{port},{key})
if '-G' in sys.argv:
    print('hostname '+host+'\nuser '+account+'\nport '+str(port))
    sys.exit(0)
with open(state,'w') as stream: stream.write(str(phase+1))
debug=open(sys.argv[sys.argv.index('-E')+1],'w',buffering=1)
debug.write('debug1: Server host key: ssh-ed25519 SHA256:'+key+'\nAuthenticated to '+host+' using publickey.\ndebug1: Sending command: fixed helper\n')
debug.flush()
if phase==0:
    os.write(1,b'Linux\nx86_64\nfixture\n/project\n')
    sys.exit(0)
data=os.read(0,65536)
with open(captured,'wb') as stream: stream.write(data)
sys.exit(1)
"#,
        state = quoted(&phases),
        captured = quoted(&captured),
        original_key = "a".repeat(43),
        host = serde_json::to_string(host).unwrap(),
        account = serde_json::to_string(account).unwrap(),
        port = port,
        key = serde_json::to_string(key).unwrap(),
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = SshConnectionProfile {
        id: "profile".into(),
        destination: "fixture-alias".into(),
        port: None,
        identity_file: None,
    };
    let target = SshWorkspaceTarget::new(
        profile.id.clone(),
        "host.test".into(),
        22,
        "fixture".into(),
        format!("SHA256:{}", "a".repeat(43)),
        services::workspace_id("fixture", "/project"),
        RemotePath::new("/project").unwrap(),
    )
    .unwrap();
    crate::ssh_resume_store::save(
        &scratch.0,
        &target,
        &runtime::SshResume {
            runtime_instance: "fixture-resume-instance".into(),
            lease: "synthetic-fixture-private-capability".into(),
        },
    )
    .unwrap();
    let (_commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
    let (output, _events) = mpsc::sync_channel(EVENTS_MAX);
    let mut owner = Owner {
        resume_required: false,
        profile,
        generation: 1,
        target: Some(target),
        db_path: scratch.0.join("test.sqlite"),
        resources: PathBuf::new(),
        requests,
        output,
        cancellation: Arc::new(Cancellation {
            cancelled: AtomicBool::new(false),
            interrupt: Mutex::new(None),
        }),
        wake: Arc::new(|| {}),
        next_request: FIRST_INTERNAL_REQUEST,
        registrar: Arc::new(NoRegistration),
        test_program: Some(program),
    };
    let result = owner.run();
    assert_eq!(
        std::fs::read_to_string(phases).unwrap(),
        "2",
        "fixture must reach the second authenticated SSH phase"
    );
    assert!(
        std::fs::read(captured).unwrap().is_empty(),
        "a changed authenticated peer received helper hello bytes before identity verification"
    );
    assert!(
        matches!(
            result,
            Err(SshWorkspaceError::HostKeyChanged | SshWorkspaceError::AuthenticationFailed)
        ),
        "a changed peer must fail before running helper I/O"
    );
}

#[test]
fn ssh_workspace_host_never_sends_resume_to_a_changed_resolved_peer() {
    assert_changed_peer_receives_no_resume("other.test", "fixture", 22, &"a".repeat(43));
    assert_changed_peer_receives_no_resume("host.test", "other", 22, &"a".repeat(43));
    assert_changed_peer_receives_no_resume("host.test", "fixture", 2222, &"a".repeat(43));
}

#[test]
fn ssh_workspace_host_never_sends_resume_to_a_changed_authenticated_key() {
    assert_changed_peer_receives_no_resume("host.test", "fixture", 22, &"b".repeat(43));
}

struct NoSecrets;
impl runtime::RuntimeSecretResolver for NoSecrets {
    fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
        anyhow::bail!("fixture has no credentials")
    }
}

#[test]
fn ssh_workspace_host_rejects_helper_metadata_from_another_architecture() {
    use std::os::unix::net::UnixListener;
    let scratch = Scratch::new();
    let socket = scratch.0.join("helper.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let program = scratch.0.join("ssh");
    let script = format!(
        r#"#!/usr/bin/python3
import os,select,socket,sys
if '-G' in sys.argv:
    print('hostname host.test\nuser fixture\nport 22')
    sys.exit(0)
debug=open(sys.argv[sys.argv.index('-E')+1],'w',buffering=1)
debug.write('debug1: Server host key: ssh-ed25519 SHA256:{key}\nAuthenticated to host.test using publickey.\ndebug1: Sending command: fixed helper\n')
debug.flush()
if 'uname -s' in sys.argv[-1]:
    os.write(1,b'Linux\nx86_64\nfixture\n/project\n')
    sys.exit(0)
stream=socket.socket(socket.AF_UNIX)
stream.connect({socket})
inputs=[0,stream]
while True:
    ready,_,_=select.select(inputs,[],[],1)
    for source in ready:
        if source==0:
            data=os.read(0,16384)
            if not data:
                inputs.remove(0)
                stream.shutdown(socket.SHUT_WR)
            else:
                stream.sendall(data)
        else:
            data=stream.recv(16384)
            if not data:
                sys.exit(0)
            os.write(1,data)
"#,
        key = "a".repeat(43),
        socket = serde_json::to_string(socket.to_str().unwrap()).unwrap(),
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = SshConnectionProfile {
        id: "profile".into(),
        destination: "fixture-alias".into(),
        port: None,
        identity_file: None,
    };
    let target = SshWorkspaceTarget::new(
        profile.id.clone(),
        "host.test".into(),
        22,
        "fixture".into(),
        format!("SHA256:{}", "a".repeat(43)),
        services::workspace_id("fixture", "/project"),
        RemotePath::new("/project").unwrap(),
    )
    .unwrap();
    let metadata_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = metadata_calls.clone();
    let expected_identity = identity(&target);
    let logs = scratch.0.clone();
    let (server_result_tx, server_result_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= until {
                        let _ =
                            server_result_tx.send(Err(anyhow::anyhow!("fixture_accept_timeout")));
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = server_result_tx.send(Err(error.into()));
                    return;
                }
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let backend = runtime::InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(NoSecrets),
            logs,
            secret::RedactionService::new(),
            None,
            None,
            vec![],
        )
        .unwrap();
        let config =
            runtime::SshServerConfig::new(expected_identity, backend.instance_token().into());
        let close = stream.try_clone().unwrap();
        let result = runtime::serve_ssh_stream(
            &backend,
            stream.try_clone().unwrap(),
            stream,
            &config,
            Arc::new(move |_, payload| {
                let reply: services::Reply = match serde_json::from_slice::<Request>(payload)? {
                    Request::Metadata => {
                        calls.fetch_add(1, Ordering::AcqRel);
                        Ok(Response::Metadata(services::ServerMetadata {
                            account: "fixture".into(),
                            home: "/project".into(),
                            os: "linux".into(),
                            architecture: "aarch64".into(),
                            helper_version: env!("CARGO_PKG_VERSION").into(),
                        }))
                    }
                    Request::InspectAgents => Ok(Response::Agents(vec![])),
                    _ => Err(services::ServiceError::InvalidRequest),
                };
                Ok(serde_json::to_vec(&reply)?)
            }),
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
        );
        let _ = server_result_tx.send(result);
    });
    let (_commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
    let (output, events) = mpsc::sync_channel(EVENTS_MAX);
    let cancellation = Arc::new(Cancellation {
        cancelled: AtomicBool::new(false),
        interrupt: Mutex::new(None),
    });
    let mut owner = Owner {
        resume_required: false,
        profile,
        generation: 1,
        target: Some(target),
        db_path: scratch.0.join("test.sqlite"),
        resources: PathBuf::new(),
        requests,
        output,
        cancellation: cancellation.clone(),
        wake: Arc::new(|| {}),
        next_request: FIRST_INTERNAL_REQUEST,
        registrar: Arc::new(NoRegistration),
        test_program: Some(program),
    };
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = owner.run();
        let _ = result_tx.send(result);
    });
    let until = Instant::now() + Duration::from_secs(5);
    let mut connected = false;
    let result = loop {
        while let Ok(event) = events.try_recv() {
            if matches!(event, SshHostEvent::Connected { .. }) {
                connected = true;
                cancellation.cancel();
            }
        }
        if let Ok(result) = result_rx.try_recv() {
            break result;
        }
        if Instant::now() >= until {
            cancellation.cancel();
            panic!("metadata fixture failed to complete boundedly");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    cancellation.cancel();
    drop(events);
    worker.join().unwrap();
    let _ = server_result_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    server.join().unwrap();
    assert_eq!(metadata_calls.load(Ordering::Acquire), 1);
    assert!(
        !connected,
        "an x86_64 probe must not accept an aarch64 helper metadata reply"
    );
    assert!(matches!(result, Err(SshWorkspaceError::HelperFailed)));
}

#[test]
fn ssh_workspace_host_clears_only_expired_target_proof_after_explicit_helper_install() {
    use sha2::{Digest, Sha256};
    let scratch = Scratch::new();
    let phases = scratch.0.join("phases");
    let program = scratch.0.join("ssh");
    let mut rejection = Vec::new();
    runtime::write_ssh_handshake_rejection(
        &mut rejection,
        runtime::SshHandshakeError::ResumeExpired,
    )
    .unwrap();
    let script = format!(
        r#"#!/usr/bin/python3
import os,sys,time
state={state}
if '-G' in sys.argv:
    print('hostname host.test\nuser fixture\nport 22')
    sys.exit(0)
try:
    with open(state) as stream: phase=int(stream.read())
except FileNotFoundError:
    phase=0
with open(state,'w') as stream: stream.write(str(phase+1))
debug=open(sys.argv[sys.argv.index('-E')+1],'w',buffering=1)
debug.write('debug1: Server host key: ssh-ed25519 SHA256:{key}\nAuthenticated to host.test using publickey.\ndebug1: Sending command: fixed helper\n')
debug.flush()
if phase==0:
    os.write(1,b'Linux\nx86_64\nfixture\n/project\n')
    sys.exit(0)
if phase==1:
    sys.exit(127)
if phase==2:
    data=sys.stdin.buffer.read()
    if len(data)!=64:
        sys.exit(1)
    os.write(1,b'DEPPY_HELPER_READY {version}\n')
    sys.exit(0)
if phase==3:
    os.read(0,65536)
    os.write(1,bytes({rejection:?}))
    time.sleep(0.1)
    sys.exit(1)
sys.exit(1)
"#,
        state = serde_json::to_string(phases.to_str().unwrap()).unwrap(),
        key = "a".repeat(43),
        version = env!("CARGO_PKG_VERSION"),
        rejection = rejection,
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = SshConnectionProfile {
        id: "profile".into(),
        destination: "fixture-alias".into(),
        port: None,
        identity_file: None,
    };
    let target = SshWorkspaceTarget::new(
        profile.id.clone(),
        "host.test".into(),
        22,
        "fixture".into(),
        format!("SHA256:{}", "a".repeat(43)),
        services::workspace_id("fixture", "/project"),
        RemotePath::new("/project").unwrap(),
    )
    .unwrap();
    let mut other_target = target.clone();
    other_target.canonical_root = RemotePath::new("/other-project").unwrap();
    other_target.remote_workspace_id = services::workspace_id("fixture", "/other-project");
    for scope in [&target, &other_target] {
        crate::ssh_resume_store::save(
            &scratch.0,
            scope,
            &runtime::SshResume {
                runtime_instance: "fixture-retained-runtime".into(),
                lease: "synthetic-private-retained-lease".into(),
            },
        )
        .unwrap();
    }
    let resources = scratch.0.join("Resources");
    let artifacts = resources.join("remote-host");
    std::fs::create_dir_all(artifacts.join("linux-x86_64")).unwrap();
    let mut binary = vec![0u8; 64];
    binary[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    binary[16..18].copy_from_slice(&2u16.to_le_bytes());
    binary[18..20].copy_from_slice(&62u16.to_le_bytes());
    let digest: String = Sha256::digest(&binary)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    std::fs::write(artifacts.join("linux-x86_64/deppy-remote-host"), binary).unwrap();
    std::fs::write(
        artifacts.join("manifest.json"),
        serde_json::to_vec(&vec![crate::remote_bootstrap::BundledHelperManifest {
            version: env!("CARGO_PKG_VERSION").into(),
            target: "linux-x86_64".into(),
            sha256: digest,
            bytes: 64,
        }])
        .unwrap(),
    )
    .unwrap();
    let (commands, requests) = mpsc::sync_channel(COMMANDS_MAX);
    let (output, events) = mpsc::sync_channel(EVENTS_MAX);
    let done = Arc::new(AtomicBool::new(false));
    let ui_done = done.clone();
    let approvals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ui_approvals = approvals.clone();
    let ui = std::thread::spawn(move || {
        while !ui_done.load(Ordering::Acquire) {
            if let Ok(SshHostEvent::Ui(snapshot)) = events.recv_timeout(Duration::from_millis(10))
                && matches!(snapshot.update, SshWorkspaceUpdate::InstallRequired { .. })
            {
                ui_approvals.fetch_add(1, Ordering::AcqRel);
                commands
                    .send(HostCommand::Ui(SshWorkspaceIntent::InstallHelper {
                        generation: snapshot.generation,
                    }))
                    .unwrap();
            }
        }
    });
    let cancellation = Arc::new(Cancellation {
        cancelled: AtomicBool::new(false),
        interrupt: Mutex::new(None),
    });
    let watchdog_cancel = cancellation.clone();
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let watchdog = std::thread::spawn(move || {
        if finished_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            watchdog_cancel.cancel();
        }
    });
    let mut owner = Owner {
        resume_required: false,
        profile,
        generation: 1,
        target: Some(target.clone()),
        db_path: scratch.0.join("test.sqlite"),
        resources,
        requests,
        output,
        cancellation,
        wake: Arc::new(|| {}),
        next_request: FIRST_INTERNAL_REQUEST,
        registrar: Arc::new(NoRegistration),
        test_program: Some(program),
    };
    let result = owner.run();
    done.store(true, Ordering::Release);
    let _ = finished_tx.send(());
    ui.join().unwrap();
    watchdog.join().unwrap();
    let completed_phases = std::fs::read_to_string(phases).unwrap_or_default();
    assert_eq!(
        approvals.load(Ordering::Acquire),
        1,
        "install fixture result={result:?}, completed_phases={completed_phases}"
    );
    assert_eq!(completed_phases, "4", "install fixture result={result:?}");
    assert!(
        matches!(result, Err(SshWorkspaceError::SessionExpired)),
        "install fixture result={result:?}"
    );
    assert!(
        crate::ssh_resume_store::load(&scratch.0, &other_target)
            .unwrap()
            .is_some(),
        "expiry must not erase another target's proof"
    );
    assert!(
        crate::ssh_resume_store::load(&scratch.0, &target)
            .unwrap()
            .is_none(),
        "typed expiry after a consented install must clear the exact stale proof"
    );
}
