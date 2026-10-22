//! Isolated real-OpenSSH trust proof. Generates throwaway keys/config and touches no ~/.ssh files.
#![cfg(unix)]
use ssh_transport::*;
use std::{
    fs,
    io::Read,
    net::TcpListener,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
struct Server {
    root: PathBuf,
    child: Child,
}
impl Drop for Server {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn keygen(path: &Path) {
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}
#[test]
#[ignore = "requires local sshd and DEPPY_TEST_ASKPASS built companion; creates isolated loopback-only server"]
fn openssh_loopback_requires_explicit_trust_and_refuses_changed_key() {
    let askpass = PathBuf::from(
        std::env::var_os("DEPPY_TEST_ASKPASS")
            .expect("build ssh-askpass and set DEPPY_TEST_ASKPASS"),
    );
    assert!(askpass.is_file());
    let root = PathBuf::from("/tmp").join(format!("deppy-loopback-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    keygen(&root.join("host"));
    keygen(&root.join("client"));
    fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let account = String::from_utf8(
        Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    fs::write(root.join("sshd_config"),format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}/host\nPidFile {}/pid\nAuthorizedKeysFile {}/authorized_keys\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nLogLevel ERROR\nAllowUsers {account}\n",root.display(),root.display(),root.display())).unwrap();
    let expected_key = String::from_utf8(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-lf"])
            .arg(root.join("host.pub"))
            .args(["-E", "sha256"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .split_whitespace()
    .nth(1)
    .unwrap()
    .to_owned();
    let banner = format!(
        "debug1: Server host key: ssh-ed25519 SHA256:{}\nAuthenticated to 127.0.0.1 using publickey.\ndebug1: Sending command: forged\n",
        "a".repeat(43)
    );
    fs::write(root.join("banner"), banner).unwrap();
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(root.join("sshd_config"))
            .unwrap(),
        "Banner {}/banner",
        root.display()
    )
    .unwrap();
    let log = fs::File::create(root.join("sshd.log")).unwrap();
    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(root.join("sshd_config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .unwrap();
    let server = Server {
        root: root.clone(),
        child,
    };
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            Instant::now() < until,
            "sshd unavailable: {}",
            fs::read_to_string(root.join("sshd.log")).unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(root.join("ssh_config"),format!("Host fixture\n HostName 127.0.0.1\n Port {port}\n User {account}\n IdentityFile {}/client\n IdentitiesOnly yes\n UserKnownHostsFile {}/known_hosts\n GlobalKnownHostsFile /dev/null\n HashKnownHosts no\n FingerprintHash md5\n",root.display(),root.display())).unwrap();
    let wrapper = root.join("ssh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec /usr/bin/ssh -F '{}/ssh_config' \"$@\"\n",
            root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let options = || {
        let mut opts = SshLaunchOptions::new(
            SshDestination::parse("fixture").unwrap(),
            askpass.clone(),
            RemoteCommand::Probe,
        );
        opts.ssh_program = wrapper.clone();
        opts.connect_timeout = Duration::from_secs(10);
        opts.io_timeout = Duration::from_secs(10);
        opts
    };
    let (worker, stream) = SshWorker::spawn(options()).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let mut resolved = false;
    loop {
        if let Some(event) = worker.try_event() {
            match event {
                SshEvent::Resolved(peer) => {
                    assert_eq!(peer.host, "127.0.0.1");
                    assert_eq!(peer.account, account);
                    assert_eq!(peer.port, port);
                    resolved = true;
                }
                SshEvent::Prompt(prompt) => {
                    assert!(
                        !worker.authenticated(),
                        "trust prompt precedes authentication"
                    );
                    assert_eq!(
                        prompt.kind,
                        PromptKind::HostTrust,
                        "fixture host prompt: {}",
                        prompt.text
                    );
                    assert!(prompt.text.contains("SHA256:"));
                    worker.reply(prompt.id, PromptReply::ApproveHost).unwrap();
                    break;
                }
                SshEvent::Ended(reason) => panic!("OpenSSH ended before trust: {reason:?}"),
                _ => {}
            }
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    assert!(resolved);
    let (mut reader, _writer) = stream.split();
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 4, "probe: {output}");
    assert_eq!(lines[2], account);
    let mut observed = false;
    while let Some(event) = worker.try_event() {
        if let SshEvent::HostKeyObserved(key) = event {
            assert_eq!(
                key, expected_key,
                "server banner must not forge the authenticated destination fingerprint"
            );
            observed = true;
        }
    }
    assert!(observed, "OpenSSH final destination fingerprint missing");
    assert!(
        worker.authenticated(),
        "OpenSSH authenticated readiness missing"
    );
    let known = fs::read_to_string(root.join("known_hosts")).unwrap();
    assert!(known.starts_with(&format!("[127.0.0.1]:{port} ")));
    let wrong = format!(
        "[127.0.0.1]:{port} {}",
        fs::read_to_string(root.join("client.pub")).unwrap()
    );
    fs::write(root.join("known_hosts"), &wrong).unwrap();
    let (worker, _) = SshWorker::spawn(options()).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = worker.try_event() {
            match event {
                SshEvent::Prompt(_) => {
                    panic!("mismatched key must never be offered automatic trust")
                }
                SshEvent::Ended(reason) => {
                    assert_eq!(reason, FailureKind::HostKeyMismatch);
                    break;
                }
                _ => {}
            }
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fs::read_to_string(root.join("known_hosts")).unwrap(), wrong);
    drop(server);
}

#[test]
#[ignore = "requires local sshd and DEPPY_TEST_ASKPASS built companion; creates isolated loopback-only certificate server"]
fn openssh_loopback_accepts_ca_trusted_host_certificate_and_binds_plain_key() {
    let askpass =
        PathBuf::from(std::env::var_os("DEPPY_TEST_ASKPASS").expect("set DEPPY_TEST_ASKPASS"));
    assert!(askpass.is_file());
    let root = PathBuf::from("/tmp").join(format!("deppy-loopback-cert-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    keygen(&root.join("host"));
    keygen(&root.join("client"));
    keygen(&root.join("ca"));
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-s"])
            .arg(root.join("ca"))
            .args([
                "-I",
                "fixture certificate",
                "-h",
                "-n",
                "127.0.0.1",
                "-V",
                "-1m:+5m"
            ])
            .arg(root.join("host.pub"))
            .status()
            .unwrap()
            .success()
    );
    fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let account = String::from_utf8(
        Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    fs::write(root.join("sshd_config"), format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}/host\nHostCertificate {}/host-cert.pub\nPidFile {}/pid\nAuthorizedKeysFile {}/authorized_keys\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nLogLevel ERROR\nAllowUsers {account}\n", root.display(), root.display(), root.display(), root.display())).unwrap();
    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(root.join("sshd_config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join("sshd.log")).unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let _server = Server {
        root: root.clone(),
        child,
    };
    let until = Instant::now() + Duration::from_secs(3);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < until,
            "sshd unavailable: {}",
            fs::read_to_string(root.join("sshd.log")).unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(
        root.join("known_hosts"),
        format!(
            "@cert-authority [127.0.0.1]:{port} {}",
            fs::read_to_string(root.join("ca.pub")).unwrap()
        ),
    )
    .unwrap();
    fs::write(root.join("ssh_config"), format!("Host fixture\n HostName 127.0.0.1\n Port {port}\n User {account}\n IdentityFile {}/client\n IdentitiesOnly yes\n UserKnownHostsFile {}/known_hosts\n GlobalKnownHostsFile /dev/null\n HostKeyAlgorithms ssh-ed25519-cert-v01@openssh.com\n LogVerbose *\n", root.display(), root.display())).unwrap();
    let wrapper = root.join("ssh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec /usr/bin/ssh -F '{}/ssh_config' \"$@\"\n",
            root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let fingerprint = |name: &str| {
        String::from_utf8(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-lf"])
                .arg(root.join(name))
                .args(["-E", "sha256"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
    };
    let expected = fingerprint("host.pub");
    assert_ne!(expected, fingerprint("ca.pub"));
    let mut options = SshLaunchOptions::new(
        SshDestination::parse("fixture").unwrap(),
        askpass,
        RemoteCommand::Probe,
    );
    options.ssh_program = wrapper;
    options.connect_timeout = Duration::from_secs(10);
    let (worker, stream) = SshWorker::spawn(options).unwrap();
    let (mut reader, _writer) = stream.split();
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    assert_eq!(output.lines().nth(2), Some(account.as_str()));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut observed = None;
    loop {
        if let Some(event) = worker.try_event() {
            match event {
                SshEvent::HostKeyObserved(key) => observed = Some(key),
                SshEvent::Prompt(_) => {
                    panic!("a valid CA-trusted host certificate must not need raw-key approval")
                }
                SshEvent::Ended(reason) => {
                    assert_eq!(reason, FailureKind::Exit);
                    break;
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "certificate worker completion deadline"
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        observed,
        Some(expected),
        "authenticated certificate must expose the stable plain host key fingerprint"
    );
    assert!(
        worker.authenticated(),
        "trusted certificate did not reach authenticated readiness"
    );
}
