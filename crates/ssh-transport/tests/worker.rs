#![cfg(unix)]
use ssh_transport::*;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    program: PathBuf,
}
impl Fixture {
    fn new(body: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "deppy-ssh-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let program = root.join("ssh");
        let script = format!(
            "#!/usr/bin/python3\nimport os,sys,time,signal,socket,struct,subprocess\nif '-G' in sys.argv:\n print('hostname resolved.example.test\\nuser deploy\\nport 2200');sys.exit(0)\nopen({:?},'w').write(str(os.getpid()))\n{}\n",
            root.join("pid").to_str().unwrap(),
            body
        );
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, program }
    }
    fn options(&self) -> SshLaunchOptions {
        let mut opts = SshLaunchOptions::new(
            SshDestination {
                host: "production".into(),
                user: None,
                port: None,
            },
            "/unused/test-askpass".into(),
            RemoteCommand::Probe,
        );
        opts.ssh_program = self.program.clone();
        opts.connect_timeout = Duration::from_secs(2);
        opts.io_timeout = Duration::from_secs(2);
        opts
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn until(worker: &SshWorker, mut predicate: impl FnMut(&SshEvent) -> bool) -> SshEvent {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = worker.try_event()
            && predicate(&event)
        {
            return event;
        }
        assert!(Instant::now() < end, "event deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn resolves_config_and_bridges_child_stdio_without_exposing_stderr() {
    let fixture = Fixture::new(
        "sys.stderr.write('debug1: Server host key: ssh-ed25519 SHA256:abcdefghijklmnopqrstuv12345678901234567890123\\n');sys.stderr.flush()\ndata=os.read(0,16384);os.write(1,data)",
    );
    let (worker, stream) = SshWorker::spawn(fixture.options()).unwrap();
    assert_eq!(
        until(&worker, |e| matches!(e, SshEvent::Resolved(_))),
        SshEvent::Resolved(ResolvedIdentity {
            host: "resolved.example.test".into(),
            account: "deploy".into(),
            port: 2200
        })
    );
    let (mut reader, mut writer) = stream.split();
    writer.write_all(b"binary\0payload").unwrap();
    let mut got = [0; 14];
    reader.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"binary\0payload");
}
#[test]
fn authenticated_readiness_waits_for_authentication_and_verified_destination_key() {
    let fixture = Fixture::new(
        "sys.stderr.write('debug1: Server host key: ssh-ed25519 SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n');sys.stderr.flush()\nos.write(1,b'key-only');time.sleep(0.15)\nsys.stderr.write('Authenticated to resolved.example.test ([127.0.0.1]:2200) using \"publickey\".\\n');sys.stderr.flush()\nsys.stderr.write('debug1: Sending command: fixed helper\\n');sys.stderr.flush()\ntime.sleep(30)",
    );
    let (worker, stream) = SshWorker::spawn(fixture.options()).unwrap();
    assert!(!worker.authenticated());
    let (mut reader, _writer) = stream.split();
    let mut marker = [0; 8];
    reader.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"key-only");
    assert!(
        !worker.authenticated(),
        "host key observation is not authentication"
    );
    until(&worker, |event| {
        matches!(event, SshEvent::HostKeyObserved(_))
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !worker.authenticated() {
        assert!(Instant::now() < deadline, "authenticated readiness missing");
        thread::sleep(Duration::from_millis(2));
    }
    worker.cancel();
}
#[test]
fn deadline_and_cancel_reap_owned_child() {
    for timeout in [false, true] {
        let fixture = Fixture::new("time.sleep(30)");
        let mut opts = fixture.options();
        if timeout {
            opts.connect_timeout = Duration::from_millis(1000);
        }
        let (worker, _stream) = SshWorker::spawn(opts).unwrap();
        until(&worker, |e| matches!(e, SshEvent::Resolved(_)));
        let end = Instant::now() + Duration::from_secs(1);
        while !fixture.root.join("pid").exists() {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        let pid: i32 = fs::read_to_string(fixture.root.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        if !timeout {
            worker.cancel();
        }
        let expected = if timeout {
            FailureKind::Timeout
        } else {
            FailureKind::Cancelled
        };
        assert_eq!(
            until(&worker, |e| matches!(e, SshEvent::Ended(_))),
            SshEvent::Ended(expected)
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child not reaped");
    }
}
#[test]
fn changed_host_key_stops_without_any_automatic_acceptance() {
    let fixture = Fixture::new(
        "assert 'StrictHostKeyChecking=ask' in sys.argv\nassert 'UpdateHostKeys=no' in sys.argv\nassert 'ServerAliveInterval=15' in sys.argv\nassert 'ServerAliveCountMax=3' in sys.argv\nassert 'TCPKeepAlive=yes' in sys.argv\nsys.stderr.write('@@@@@@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@@@@@@\\n');sys.exit(255)",
    );
    let (worker, _) = SshWorker::spawn(fixture.options()).unwrap();
    assert_eq!(
        until(&worker, |e| matches!(e, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::HostKeyMismatch)
    );
}
#[test]
fn helper_command_rejects_shell_syntax_versions() {
    assert!(HelperVersion::parse("1.2.3").is_ok());
    for bad in ["1.2.3;id", "../1.2.3", "$(id)", "1.2", "1.2.3\n"] {
        assert!(HelperVersion::parse(bad).is_err());
    }
}
#[test]
fn secret_text_cannot_approve_host_trust_and_cancel_reclaims_socket() {
    let fixture = Fixture::new(
        "s=socket.socket(socket.AF_UNIX);s.connect(os.environ['DEPPY_SSH_ASKPASS_SOCKET'])\np=b'\\x01'+bytes([len(os.environ['DEPPY_SSH_ASKPASS_TOKEN'])])+os.environ['DEPPY_SSH_ASKPASS_TOKEN'].encode()+b'The authenticity of host resolved.example.test cannot be established. ED25519 key fingerprint is SHA256:fixture. Continue connecting (yes/no/[fingerprint])?'\ns.sendall(struct.pack('>I',len(p))+p)\ntime.sleep(30)",
    );
    let (worker, _) = SshWorker::spawn(fixture.options()).unwrap();
    let event = until(&worker, |e| matches!(e, SshEvent::Prompt(_)));
    let SshEvent::Prompt(prompt) = event else {
        unreachable!()
    };
    assert_eq!(prompt.kind, PromptKind::HostTrust);
    worker
        .reply(
            prompt.id,
            PromptReply::Secret(Secret::new(b"yes".to_vec()).unwrap()),
        )
        .unwrap();
    assert_eq!(
        until(&worker, |e| matches!(e, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::HostVerification)
    );
}
#[test]
fn authentication_replies_are_ephemeral_and_never_put_in_child_argv() {
    let fixture = Fixture::new(
        "assert not any('fixture-sensitive-value' in x for x in sys.argv)\ns=socket.socket(socket.AF_UNIX);s.connect(os.environ['DEPPY_SSH_ASKPASS_SOCKET'])\np=b'\\x00'+bytes([len(os.environ['DEPPY_SSH_ASKPASS_TOKEN'])])+os.environ['DEPPY_SSH_ASKPASS_TOKEN'].encode()+b'Password:'\ns.sendall(struct.pack('>I',len(p))+p)\nheader=s.recv(4);size=struct.unpack('>I',header)[0];response=b''\nwhile len(response)<size: response+=s.recv(size-len(response))\nassert response==b'fixture-sensitive-value'\nos.write(1,b'authenticated')",
    );
    let (worker, stream) = SshWorker::spawn(fixture.options()).unwrap();
    let event = until(&worker, |e| matches!(e, SshEvent::Prompt(_)));
    let SshEvent::Prompt(prompt) = event else {
        unreachable!()
    };
    assert_eq!(prompt.kind, PromptKind::Authentication);
    worker
        .reply(
            prompt.id,
            PromptReply::Secret(Secret::new(b"fixture-sensitive-value".to_vec()).unwrap()),
        )
        .unwrap();
    let (mut reader, _writer) = stream.split();
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    assert_eq!(output, "authenticated");
}
#[test]
fn bounded_output_queue_still_allows_prompt_cancellation() {
    let fixture = Fixture::new(
        "while True:\n os.write(1,b'x'*16384)\n os.write(2,b'diagnostic flood\\n'*1000)",
    );
    let (worker, _stream) = SshWorker::spawn(fixture.options()).unwrap();
    until(&worker, |e| matches!(e, SshEvent::Resolved(_)));
    thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    worker.cancel();
    assert_eq!(
        until(&worker, |e| matches!(e, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::Cancelled)
    );
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn authentication_confirmation_is_distinct_from_host_trust() {
    let fixture = Fixture::new(
        "s=socket.socket(socket.AF_UNIX);s.connect(os.environ['DEPPY_SSH_ASKPASS_SOCKET'])\np=b'\\x01'+bytes([len(os.environ['DEPPY_SSH_ASKPASS_TOKEN'])])+os.environ['DEPPY_SSH_ASKPASS_TOKEN'].encode()+b'Confirm use of authentication key?'\ns.sendall(struct.pack('>I',len(p))+p)\ntime.sleep(30)",
    );
    let (worker, _) = SshWorker::spawn(fixture.options()).unwrap();
    let event = until(&worker, |e| matches!(e, SshEvent::Prompt(_)));
    let SshEvent::Prompt(prompt) = event else {
        unreachable!()
    };
    assert_ne!(
        prompt.kind,
        PromptKind::HostTrust,
        "non-host confirmations cannot approve host keys"
    );
    worker.cancel();
}
#[test]
fn a_stalled_stream_consumer_has_a_bounded_backpressure_deadline() {
    let fixture = Fixture::new("while True: os.write(1,b'x'*16384)");
    let mut options = fixture.options();
    options.io_timeout = Duration::from_millis(100);
    let (worker, _stream) = SshWorker::spawn(options).unwrap();
    assert_eq!(
        until(&worker, |event| matches!(event, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::Backpressure)
    );
}
#[test]
fn healthy_idle_ssh_stream_does_not_expire_at_the_io_backpressure_deadline() {
    let fixture = Fixture::new("time.sleep(0.15);os.write(1,b'alive')");
    let mut options = fixture.options();
    options.io_timeout = Duration::from_millis(20);
    let (_worker, stream) = SshWorker::spawn(options).unwrap();
    let (mut reader, _writer) = stream.split();
    let mut bytes = [0; 5];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"alive");
}
#[test]
fn a_nonzero_ssh_process_exit_is_not_a_successful_probe() {
    let fixture = Fixture::new("sys.exit(42)");
    let (worker, _stream) = SshWorker::spawn(fixture.options()).unwrap();
    assert_eq!(
        until(&worker, |event| matches!(event, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::RemoteExit(42))
    );
}
#[test]
fn failing_ssh_config_resolution_cannot_supply_an_identity() {
    let fixture = Fixture::new("sys.exit(0)");
    fs::write(&fixture.program,"#!/usr/bin/python3\nprint('hostname invalid.test\\nuser deploy\\nport 22')\nraise SystemExit(2)\n").unwrap();
    let (worker, _stream) = SshWorker::spawn(fixture.options()).unwrap();
    assert_eq!(
        until(&worker, |_event| true),
        SshEvent::Ended(FailureKind::Resolve)
    );
}
#[test]
fn stream_flush_waits_until_queued_bytes_reach_the_child_pipe() {
    let fixture = Fixture::new(
        "time.sleep(0.15)\ncount=0\nwhile count<262144: count+=len(os.read(0,16384))\nos.write(1,b'flushed')",
    );
    let (_worker, stream) = SshWorker::spawn(fixture.options()).unwrap();
    let (mut reader, mut writer) = stream.split();
    writer.write_all(&vec![b'x'; 262144]).unwrap();
    let start = Instant::now();
    writer.flush().unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "flush returned while data remained in the transport queue"
    );
    let mut got = [0; 7];
    reader.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"flushed");
}
#[test]
fn a_child_that_closes_stdout_cannot_keep_the_stream_reader_alive_forever() {
    let fixture = Fixture::new("os.write(1,b'last');os.close(1);time.sleep(30)");
    let (worker, stream) = SshWorker::spawn(fixture.options()).unwrap();
    let (mut reader, _writer) = stream.split();
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"last");
    assert_eq!(
        until(&worker, |event| matches!(event, SshEvent::Ended(_))),
        SshEvent::Ended(FailureKind::Timeout)
    );
    let mut eof = [0; 1];
    assert_eq!(reader.read(&mut eof).unwrap(), 0);
}
