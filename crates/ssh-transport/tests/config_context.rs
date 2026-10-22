#![cfg(unix)]

use ssh_transport::{
    HelperVersion, RemoteCommand, SshDestination, SshEvent, SshLaunchOptions, SshWorker,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn config_resolution_uses_the_same_options_and_command_context_as_execution() {
    let root = Scratch(std::env::temp_dir().join(format!(
        "deppy-ssh-config-context-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    fs::create_dir(&root.0).unwrap();
    let program = root.0.join("ssh");
    fs::write(&program, "#!/usr/bin/python3\nimport os,sys\nresolve='-G' in sys.argv\nname='resolve.args' if resolve else 'execute.args'\nwith open(os.path.join(os.path.dirname(__file__),name),'wb') as f:\n f.write(b'\\0'.join(os.fsencode(a) for a in sys.argv[1:]))\nif resolve: print('hostname fixture.invalid\\nuser deploy\\nport 2200')\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    for command in [
        RemoteCommand::Probe,
        RemoteCommand::Helper {
            version: HelperVersion::parse("0.13.0").unwrap(),
        },
    ] {
        let mut options = SshLaunchOptions::new(
            SshDestination {
                host: "fixture".into(),
                user: None,
                port: None,
            },
            "/unused/fixture-askpass".into(),
            command,
        );
        options.ssh_program = program.clone();
        let (worker, _stream) = SshWorker::spawn(options).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(worker.try_event(), Some(SshEvent::Ended(_))) {
                break;
            }
            assert!(Instant::now() < deadline, "fixture did not complete");
            thread::sleep(Duration::from_millis(2));
        }
        let args = |name: &str| -> Vec<String> {
            fs::read(root.0.join(name))
                .unwrap()
                .split(|b| *b == 0)
                .map(|b| String::from_utf8(b.to_vec()).unwrap())
                .collect()
        };
        let mut resolved = args("resolve.args");
        resolved.retain(|arg| arg != "-G");
        let mut executed = args("execute.args");
        let diagnostics = executed.iter().position(|arg| arg == "-E").unwrap();
        executed.drain(diagnostics..diagnostics + 2);
        assert_eq!(
            resolved, executed,
            "Match command/sessiontype must see the actual invocation context"
        );
    }
}
