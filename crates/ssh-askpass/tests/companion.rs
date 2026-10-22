#![cfg(unix)]
use ssh_askpass::*;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
    thread,
};
#[test]
fn companion_passes_one_reply_only_on_stdout_and_never_stderr() {
    let root = std::env::temp_dir().join(format!("deppy-askpass-test-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("askpass");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let bytes = read_frame(&mut stream, MAX_REQUEST_BYTES).unwrap();
        assert_eq!(
            decode_request(&bytes).unwrap(),
            ("aabbccdd".into(), "Password:".into(), false)
        );
        write_frame(&mut stream, b"fixture-secret").unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_deppy-ssh-askpass"))
        .arg("Password:")
        .env(SOCKET_ENV, &socket)
        .env(TOKEN_ENV, "aabbccdd")
        .env_remove("SSH_ASKPASS_PROMPT")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fixture-secret\n");
    assert!(output.stderr.is_empty());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn companion_fails_closed_without_endpoint_and_reports_build_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_deppy-ssh-askpass"))
        .arg("Password:")
        .env_remove(SOCKET_ENV)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let version = Command::new(env!("CARGO_BIN_EXE_deppy-ssh-askpass"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("deppy-ssh-askpass {}\n", env!("CARGO_PKG_VERSION"))
    );
}
