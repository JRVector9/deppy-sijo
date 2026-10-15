#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/runtime/src/input_admission.rs"]
mod input_admission;
use input_admission::{AgentInputGuard, AgentInputIntent, AgentPromptKind};
use std::time::{Duration, Instant};
fn main() {
    let spec = pty::CommandSpec { program: "/bin/sh".into(), args: vec!["-c".into(), "stty raw -echo; printf READY; exec /bin/cat".into()], env: vec![], cwd: None };
    let mut live = session::Session::spawn_with_spec(deppy_core::SessionId(1001), session::SessionKind::Agent, &spec, 80, 24, 100).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !live.screen_text().contains("READY") {
        assert!(Instant::now() < deadline);
        live.pump(|_| {});
        std::thread::sleep(Duration::from_millis(1));
    }
    let detector = session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None));
    let guard = AgentInputGuard { foreground_process_group: live.foreground_process_group().unwrap(), provider: AgentPromptKind::Codex, intent: AgentInputIntent::ExplicitPrompt };
    live.replay_ansi(&mut "\x1b[2J\x1b[H› \x1b[2mAsk Codex\x1b[0m\x1b[1;3H".as_bytes()).unwrap();
    let ascii = guard.allows(&live, Some(&detector));
    assert!(ascii);
    live.replay_ansi(&mut "\x1b[2J\x1b[H› \x1b[2m한글 힌트\x1b[0m\x1b[1;4H".as_bytes()).unwrap();
    let wide = guard.allows(&live, Some(&detector));
    println!("ascii_placeholder_allowed={ascii} wide_placeholder_allowed={wide}");
    assert!(wide, "current empty native hint must remain admissible at its wide cursor owner");
}
