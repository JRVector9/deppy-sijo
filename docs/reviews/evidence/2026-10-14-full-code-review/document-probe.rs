#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/app/src/document_io.rs"]
mod document_io;
#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/app/src/lazy_worker.rs"]
mod lazy_worker;

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn root(kind: &str) -> PathBuf {
    let path = PathBuf::from(format!("/private/tmp/deppy-doc-review-{kind}-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    path
}

fn symlink_probe() {
    let dir = root("symlink");
    let target = dir.join("target.md");
    let alias = dir.join("alias.md");
    std::fs::write(&target, b"old source").unwrap();
    std::os::unix::fs::symlink("target.md", &alias).unwrap();
    let document_io::DocumentLoadOutcome::Loaded { source, revision } =
        document_io::load_document(&document_io::DocumentLoadRequest { path: alias.clone() })
    else { panic!("production load did not admit symlink") };
    assert_eq!(source, "old source");
    let outcome = document_io::save_document(document_io::DocumentSaveRequest {
        path: alias.clone(), contents: "new source".into(), expected_revision: revision,
    });
    let still_link = std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink();
    let target_bytes = std::fs::read(&target).unwrap();
    let alias_bytes = std::fs::read(&alias).unwrap();
    println!("save={outcome:?}; alias_is_symlink={still_link}; target_bytes={target_bytes:?}; alias_bytes={alias_bytes:?}");
    let safe = still_link && target_bytes == b"new source";
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(safe, "saving an admitted symlink must update its loaded target and preserve the link");
}

fn fifo_probe() {
    let dir = root("fifo");
    let fifo = dir.join("blocked.md");
    let regular = dir.join("normal.md");
    std::fs::write(&regular, b"normal document").unwrap();
    let native = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(native.as_ptr(), 0o600) }, 0);
    let mut worker = lazy_worker::LazyBoundedWorker::new(
        "document-load-review", Duration::from_secs(30),
        || |request: document_io::DocumentLoadRequest| document_io::load_document(&request), || {},
    );
    worker.try_request(document_io::DocumentLoadRequest { path: fifo.clone() }).unwrap();
    let start = Instant::now();
    let mut early = None;
    while start.elapsed() < Duration::from_millis(250) {
        early = worker.try_recv();
        if early.is_some() { break; }
        std::thread::sleep(Duration::from_millis(2));
    }
    let next = worker.try_request(document_io::DocumentLoadRequest { path: regular.clone() });
    let blocked = early.is_none() && matches!(&next, Err(lazy_worker::LazyWorkerSubmitError::Full(_)));
    println!("after_250ms_fifo_result={early:?}; normal_document_admission={next:?}; blocked={blocked}");
    // Opening the writer unblocks the exact production File::open; keep the test bounded.
    let writer = std::thread::spawn(move || std::fs::OpenOptions::new().write(true).open(fifo));
    let until = Instant::now() + Duration::from_secs(2);
    let result = loop {
        if let Some(result) = worker.try_recv() { break result.into_result().unwrap(); }
        assert!(Instant::now() < until, "FIFO cleanup did not release the worker");
        std::thread::sleep(Duration::from_millis(2));
    };
    writer.join().unwrap().unwrap();
    println!("after_writer_release={result:?}");
    assert!(matches!(result, document_io::DocumentLoadOutcome::Failed { code: document_io::DocumentIoErrorCode::FileTypeInvalid }));
    worker.try_request(document_io::DocumentLoadRequest { path: regular }).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = worker.try_recv() {
            assert!(matches!(result.into_result().unwrap(), document_io::DocumentLoadOutcome::Loaded { .. }));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(worker);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!blocked, "special .md file must be rejected without blocking unrelated document loads");
}

fn save_race_probe() {
    let dir=root("save-race");
    let target=dir.join("doc.md");
    std::fs::write(&target,b"original").unwrap();
    let document_io::DocumentLoadOutcome::Loaded { revision, .. }=document_io::load_document(&document_io::DocumentLoadRequest { path:target.clone() }) else {panic!("load failed")};
    let temp_dir=dir.clone();
    let concurrent_target=target.clone();
    let (tx,rx)=std::sync::mpsc::channel();
    let observer=std::thread::spawn(move || {
        tx.send(()).unwrap();
        let until=Instant::now()+Duration::from_secs(2);
        loop {
            let ready=std::fs::read_dir(&temp_dir).unwrap().filter_map(Result::ok).any(|item|item.file_name().to_string_lossy().contains("deppy-doc-tmp"));
            if ready {
                std::fs::write(&concurrent_target,b"external edit after initial revision check").unwrap();
                let before_commit=std::fs::read_dir(&temp_dir).unwrap().filter_map(Result::ok).any(|item|item.file_name().to_string_lossy().contains("deppy-doc-tmp"));
                return before_commit;
            }
            if Instant::now()>=until {return false;}
            std::thread::yield_now();
        }
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let outcome=document_io::save_document(document_io::DocumentSaveRequest {path:target.clone(),contents:"n".repeat(8*1024*1024),expected_revision:revision});
    let observed=observer.join().unwrap();
    let bytes=std::fs::read(&target).unwrap();
    let lost=observed && bytes.len()==8*1024*1024 && bytes[0]==b'n' && matches!(outcome,document_io::DocumentSaveOutcome::Saved{..});
    println!("observer_write_finished_before_temp_commit={observed}; outcome={outcome:?}; final_bytes={}; external_edit_lost={lost}",bytes.len());
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!lost,"external edit made during temp write/sync must not be overwritten as Saved");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("fifo") => fifo_probe(),
        Some("symlink") => symlink_probe(),
        Some("save-race") => save_race_probe(),
        _ => panic!("expected fifo or symlink"),
    }
}
