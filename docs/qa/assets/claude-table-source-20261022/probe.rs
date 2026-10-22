use deppy_core::SessionId;
use pty::CommandSpec;
use session::{Session, SessionKind};
use std::time::{Duration, Instant};
#[test]
fn installed_agent_table_resize_probe() {
    use std::fmt::Write as _;
    let program = std::env::var("DEPPY_TABLE_PROBE_PROGRAM").expect("test-owned CLI required");
    let args = std::env::var("DEPPY_TABLE_PROBE_ARGS")
        .expect("explicit CLI args")
        .lines()
        .map(str::to_owned)
        .collect();
    let root = std::path::PathBuf::from(
        std::env::var("DEPPY_TABLE_PROBE_ROOT").expect("test output directory"),
    );
    assert!(root.starts_with("/private/tmp") || root.starts_with(std::env::temp_dir()));
    std::fs::create_dir_all(&root).unwrap();
    let mut session = Session::spawn_with_spec(
        SessionId(2),
        SessionKind::Agent,
        &CommandSpec {
            program,
            args,
            env: vec![],
            cwd: Some(
                std::env::var_os("DEPPY_TABLE_PROBE_CWD")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| root.clone()),
            ),
        },
        80,
        40,
        1000,
    )
    .expect("isolated CLI PTY");
    let mut raw = Vec::new();
    let mut metadata = String::from("stage\tcols\trows\talt\tbytes\n");
    let mut saw_table = false;
    let fixture_bootstrap = std::env::var("DEPPY_TABLE_PROBE_BOOTSTRAP").as_deref() == Ok("1");
    let mut approved_fixture_key = false;
    let mut trusted_fixture_directory = false;
    let mut kept_manual_permissions = false;
    let mut manual_selected_at: Option<Instant> = None;
    let mut manual_confirm_count = 0;
    for (stage, cols) in [80, 42, 120, 80].into_iter().enumerate() {
        session.resize_checked(cols, 40).expect("resize");
        let duration = if stage == 0 { 8 } else { 2 };
        let deadline = Instant::now() + Duration::from_secs(duration);
        while Instant::now() < deadline {
            session.pump(|chunk| raw.extend_from_slice(chunk));
            assert!(raw.len() <= 2 * 1024 * 1024, "probe output cap");
            if stage == 0 && fixture_bootstrap {
                let screen = session.screen_text();
                std::fs::write(root.join("bootstrap-last.txt"), &screen).unwrap();
                if !approved_fixture_key && screen.contains("Do you want to use this API key?") {
                    session
                        .write_input(b"\x1b[A\r")
                        .expect("test-owned dummy key");
                    approved_fixture_key = true;
                }
                if !trusted_fixture_directory
                    && screen.contains("Do you trust the contents of this directory?")
                {
                    session.write_input(b"y").expect("empty fixture directory");
                    trusted_fixture_directory = true;
                }
                if !kept_manual_permissions
                    && screen.contains("Make auto mode your default permission")
                {
                    if manual_selected_at.is_none() {
                        session.write_input(b"\x1b[B").expect("select manual fixture permissions");
                        manual_selected_at = Some(Instant::now());
                    } else if screen.contains("❯ No, keep manual mode")
                        && manual_selected_at.unwrap().elapsed() >= Duration::from_millis(350)
                        && manual_confirm_count < 2
                    {
                        session.write_input(b"\r").expect("retain selected manual fixture permissions");
                        manual_confirm_count += 1;
                        manual_selected_at = Some(Instant::now());
                    }
                } else if manual_selected_at.is_some()
                    && !screen.contains("Make auto mode your default permission")
                {
                    kept_manual_permissions = true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = session.screen_text();
        // A narrow table may split even the row identifier across cell lines.
        saw_table |= text.contains("Fixture paragraph:") && text.contains("ready");
        std::fs::write(root.join(format!("stage-{stage}-{cols}.txt")), text).unwrap();
        if let Some(history) = session.history_snapshot(terminal::TerminalHistoryQuery {
            rows: Some(256),
            ..terminal::TerminalHistoryQuery::live()
        }) {
            let snapshot = history.snapshot;
            let mut text = String::new();
            for index in 0..snapshot.visible_cells.len() {
                if index > 0 && index % usize::from(snapshot.cols) == 0 {
                    text.push('\n');
                }
                if !snapshot.is_trailing_wide_spacer(index) {
                    snapshot.push_cell_text(index, &mut text);
                }
            }
            saw_table |= text.contains("Fixture paragraph:") && text.contains("ready");
            std::fs::write(root.join(format!("history-{stage}-{cols}.txt")), text).unwrap();
        }
        let snapshot = session.take_snapshot().unwrap();
        if snapshot.is_alt_screen
            && std::env::var("DEPPY_TABLE_PROBE_SCROLL_TOP").as_deref() == Ok("1")
        {
            // Alternate-screen history belongs to the CLI, not native scrollback.
            // These navigation keys are only sent to the isolated saved fixture.
            for (direction, key) in [("top", b"\x1b[5~"), ("bottom", b"\x1b[6~")] {
                for _ in 0..20 {
                    session
                        .write_input(key)
                        .expect("fixture history navigation");
                }
                let deadline = Instant::now() + Duration::from_secs(1);
                while Instant::now() < deadline {
                    session.pump(|chunk| raw.extend_from_slice(chunk));
                    assert!(raw.len() <= 2 * 1024 * 1024, "probe output cap");
                    std::thread::sleep(Duration::from_millis(20));
                }
                let text = session.screen_text();
                saw_table |= text.contains("Fixture paragraph:") && text.contains("ready");
                std::fs::write(
                    root.join(format!("application-{direction}-{stage}-{cols}.txt")),
                    text,
                )
                .unwrap();
            }
        }
        writeln!(
            metadata,
            "{stage}\t{}\t{}\t{}\t{}",
            snapshot.cols,
            snapshot.rows,
            snapshot.is_alt_screen,
            raw.len()
        )
        .unwrap();
    }
    std::fs::write(root.join("output.ansi"), raw).unwrap();
    std::fs::write(root.join("bootstrap-status.txt"), format!("manual_selected={} manual_confirm_count={} dismissed={}\n", manual_selected_at.is_some(), manual_confirm_count, kept_manual_permissions)).unwrap();
    std::fs::write(root.join("metadata.tsv"), metadata).unwrap();
    assert!(
        saw_table,
        "synthetic table was not displayed; inspect probe output"
    );
}
