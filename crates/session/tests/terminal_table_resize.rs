//! Exercise the real PTY, SIGWINCH and terminal backend without starting an agent
//! or touching an existing desktop session.
#![cfg(unix)]

use deppy_core::SessionId;
use pty::CommandSpec;
use session::{Session, SessionKind, SessionLifecycle};
use std::time::{Duration, Instant};
use terminal::{AlacrittyBackend, TerminalBackend};

const RESPONSIVE_TABLE: &str = r#"
draw() {
    set -- $(stty size)
    printf '\033[H\033[2JWIDTH=%s ROWS=%s\n' "$2" "$1"
    printf '| %-*s |\n' "$(($2 - 6))" 'agent table'
    printf '| %-*s |\n' "$(($2 - 6))" 'table content'
    printf 'READY=%sx%s\n' "$2" "$1"
}
trap 'draw' WINCH
draw
while :; do
    read -r command || continue
    case "$command" in quit) exit 0 ;; esac
done
"#;

fn wait_for_redraw(session: &mut Session, cols: u16, rows: u16) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let expected = format!("READY={cols}x{rows}");
    loop {
        session.pump(|_| {});
        let text = session.screen_text();
        if text.contains(&expected) {
            return text;
        }
        assert!(Instant::now() < deadline, "missing {expected}: {text:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn pane_resize_reaches_foreground_pty_and_redraws_table_at_each_width() {
    let mut session = Session::spawn_with_spec(
        SessionId(1),
        SessionKind::Agent,
        &CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), RESPONSIVE_TABLE.into()],
            env: vec![],
            cwd: None,
        },
        80,
        24,
        100,
    )
    .expect("isolated PTY");

    for (cols, rows) in [(80, 24), (42, 20), (120, 32), (80, 24)] {
        let applied = session.resize_checked(cols, rows).expect("resize");
        assert_eq!((applied.cols, applied.rows), (cols, rows));
        let text = wait_for_redraw(&mut session, cols, rows);
        let table_row = text.lines().find(|line| line.starts_with('|')).unwrap();
        assert_eq!(table_row.trim_end().chars().count(), usize::from(cols - 2));
        let snapshot = session.take_snapshot().unwrap();
        assert_eq!((snapshot.cols, snapshot.rows), (cols, rows));
    }
    // Drop closes only this test-owned process and PTY.
}

#[test]
fn terminal_reflows_soft_wrapped_text_but_preserves_explicit_table_columns() {
    let mut backend = AlacrittyBackend::new(24, 16, 100);
    backend
        .feed(b"abcdefghijklmnopqrstuvwxyz0123456789\r\n| A    | B    |\r\n| one  | two  |\r\n")
        .unwrap();
    // Shrinking wraps the original rows, but growing again must restore their
    // original explicit columns rather than inventing new table widths.
    backend.resize(10, 16).unwrap();
    backend.resize(80, 16).unwrap();
    let text = backend
        .screen_text()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
    assert!(text.contains("| A    | B    |\n| one  | two  |"));
    assert_eq!(backend.grid_dimensions().unwrap(), (80, 16));
}

/// Manual diagnostic for a test-owned CLI with a synthetic saved conversation.
/// The caller must isolate the CLI configuration and deny external networking.
/// Never point this at a user's live session or transcript.
#[test]
#[ignore = "requires an explicitly isolated CLI and synthetic transcript"]
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
    let temp_dir = std::env::temp_dir();
    assert!(
        root.starts_with("/private/tmp")
            || root.starts_with(&temp_dir)
            || root.join("tmp") == temp_dir,
        "output must belong to the disposable fixture"
    );
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
    let mut manual_selection_sent = None;
    let mut child_exited = false;
    let mut child_exit_code = None;
    for (stage, cols) in [80, 42, 120, 80].into_iter().enumerate() {
        session.resize_checked(cols, 40).expect("resize");
        let duration = if stage == 0 { 8 } else { 2 };
        let deadline = Instant::now() + Duration::from_secs(duration);
        while Instant::now() < deadline {
            session.pump(|chunk| raw.extend_from_slice(chunk));
            if let SessionLifecycle::Exited { exit_code } = session.lifecycle() {
                child_exited = true;
                child_exit_code = exit_code;
                break;
            }
            assert!(raw.len() <= 2 * 1024 * 1024, "probe output cap");
            if stage == 0 && fixture_bootstrap {
                let screen = session.screen_text();
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
                    // Selection and confirmation are distinct TUI events. A
                    // combined Down+Enter previously left this modal active.
                    if manual_selection_sent.is_none() {
                        session
                            .write_input(b"\x1b[B")
                            .expect("select manual fixture permissions");
                        manual_selection_sent = Some(Instant::now());
                    } else if manual_selection_sent
                        .is_some_and(|sent| sent.elapsed() >= Duration::from_millis(350))
                        && screen.contains("❯ No, keep manual mode")
                    {
                        session
                            .write_input(b"\r")
                            .expect("retain manual fixture permissions");
                        kept_manual_permissions = true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = session.screen_text();
        // A narrow table may split even the row identifier across cell lines.
        saw_table |= text.contains("Fixture paragraph:") && text.contains("ready");
        std::fs::write(root.join(format!("stage-{stage}-{cols}.txt")), text).unwrap();
        if let Some(history) = session.history_snapshot(terminal::TerminalHistoryQuery {
            rows: Some(terminal::HISTORY_QUERY_MAX_ROWS),
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
        if child_exited {
            break;
        }
    }
    std::fs::write(root.join("output.ansi"), raw).unwrap();
    std::fs::write(root.join("metadata.tsv"), metadata).unwrap();
    std::fs::write(
        root.join("probe-status.tsv"),
        format!(
            "child_exited\tchild_exit_code\tcleanup\n{child_exited}\t{}\tSession drop owns test process tree\n",
            child_exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ),
    )
    .unwrap();
    assert!(
        !child_exited,
        "fixture child exited early: {child_exit_code:?}"
    );
    assert!(
        saw_table,
        "synthetic table was not displayed; inspect probe output"
    );
}
