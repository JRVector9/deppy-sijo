
#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/core/src/fs.rs"]
mod production_fs;
#[cfg(unix)]
#[test]
fn full_review_atomic_write_preserves_preexisting_temp_symlink_target() {
    use std::os::unix::fs::symlink;
    let dir = std::env::temp_dir().join(format!("deppy-atomic-symlink-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let intended = dir.join("config.toml");
    let victim = dir.join("other-user-file");
    let temporary = dir.join("config.toml.deppytmp");
    std::fs::write(&intended, b"old config").unwrap();
    std::fs::write(&victim, b"must preserve").unwrap();
    symlink(&victim, &temporary).unwrap();
    production_fs::atomic_write(&intended, b"new config").unwrap();
    let victim_content = std::fs::read(&victim).unwrap();
    let published_is_symlink = std::fs::symlink_metadata(&intended).unwrap().file_type().is_symlink();
    eprintln!("production helper result: victim={:?}, published_config_is_symlink={published_is_symlink}", String::from_utf8_lossy(&victim_content));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(victim_content, b"must preserve", "atomic save truncated a different existing user file through its fixed temp symlink");
    assert!(!published_is_symlink, "save published the temp symlink as its config");
}
#[test]
fn full_review_atomic_write_parallel_saves_have_distinct_temporary_objects() {
    use std::sync::{Arc, Barrier};
    let dir = std::env::temp_dir().join(format!("deppy-atomic-concurrent-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = Arc::new(dir.join("config.toml"));
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4).map(|n| {
        let target = Arc::clone(&target);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let bytes = vec![b'A' + n; 1024 * 1024];
            let mut errors = 0usize;
            for _ in 0..30 {
                barrier.wait();
                if production_fs::atomic_write(&target, &bytes).is_err() { errors += 1; }
                barrier.wait();
            }
            errors
        })
    }).collect();
    let errors: usize = workers.into_iter().map(|worker| worker.join().unwrap()).sum();
    let bytes = std::fs::read(&*target).unwrap();
    let valid = bytes.len() == 1024 * 1024 && bytes.iter().all(|byte| *byte == bytes[0]);
    eprintln!("actual concurrent production saves: failed={errors}/120 final_payload_valid={valid}");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(errors, 0, "unrelated successful writers share one temporary name and fail rename");
    assert!(valid, "parallel saves mixed/truncated payloads");
}
