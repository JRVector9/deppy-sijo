mod pinned_cwd_tests {
    #[test]
    fn pinned_directory_survives_path_replacement_without_fallback() {
        use portable_pty::{CommandBuilder, PtySize};
        use std::os::fd::OwnedFd;
        let root = std::env::temp_dir().join(format!("portable-pty-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        let fallback = root.join("fallback");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let directory: OwnedFd = std::fs::File::open(&original).unwrap().into();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir_all(&original).unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "printf pinned > marker"]);
        command.cwd(&fallback);
        command.cwd_dir_fd(std::sync::Arc::new(directory));
        let mut child = pair.slave.spawn_command(command).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(moved.join("marker")).unwrap(),
            "pinned"
        );
        assert!(!original.join("marker").exists());
        assert!(!fallback.join("marker").exists());
        drop(pair);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn non_directory_descriptor_aborts_spawn_without_path_fallback() {
        use portable_pty::{CommandBuilder, PtySize};
        use std::os::fd::OwnedFd;
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd: OwnedFd = file.into();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let mut command = CommandBuilder::new("/bin/true");
        command.cwd(std::env::temp_dir());
        command.cwd_dir_fd(std::sync::Arc::new(fd));
        assert!(pair.slave.spawn_command(command).is_err());
    }
}
