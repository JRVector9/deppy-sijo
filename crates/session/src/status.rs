//! Status Detector (설계문서 PR-12).
//! stream line regex + 화면 텍스트 패턴 + output idle heuristic 3단 병행.
//! 감지 주기는 output batch 단위 — 호출측(runtime worker)이 tick마다 부른다.

use std::time::{Duration, Instant};

/// agent의 감지 상태. Exited는 lifecycle 소관이라 여기 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionStatus {
    Running,
    /// 입력 대기 (regex 매치 또는 idle heuristic)
    Waiting,
    NeedsApproval,
    Error,
    Done,
}

impl SessionStatus {
    /// 표시 우선순위 (높을수록 주의 필요) — UI가 여러 pane 상태를 요약할 때 사용.
    pub fn urgency(&self) -> u8 {
        priority(*self)
    }
}

/// agent_configs의 *_regex 4종 (있는 것만 컴파일).
pub struct StatusPatterns {
    waiting: Option<regex::Regex>,
    approval: Option<regex::Regex>,
    error: Option<regex::Regex>,
    done: Option<regex::Regex>,
}

impl StatusPatterns {
    /// 잘못된 regex는 무시하고 경고만 남긴다 (agent 실행 자체를 막지 않는다).
    pub fn compile(
        waiting: Option<&str>,
        approval: Option<&str>,
        error: Option<&str>,
        done: Option<&str>,
    ) -> Self {
        let compile = |kind: &str, pattern: Option<&str>| {
            pattern.and_then(|p| match regex::Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!("{kind} regex 컴파일 실패 — 무시: {e}");
                    None
                }
            })
        };
        Self {
            waiting: compile("waiting", waiting),
            approval: compile("approval", approval),
            error: compile("error", error),
            done: compile("done", done),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_none()
            && self.approval.is_none()
            && self.error.is_none()
            && self.done.is_none()
    }

    /// 화면 텍스트를 라인 단위로 매칭한다 — `^`/`$` 앵커가 stream 라인
    /// 매칭과 동일하게 동작하도록 (전체 문자열 매칭이면 앵커가 어긋난다).
    ///
    /// 스캔 범위는 **마지막 비어있지 않은 5줄** — 활성 프롬프트가 사는 영역이다.
    /// 화면 전체를 보면 이미 응답한 옛 프롬프트가 계속 재감지되어
    /// 상태가 고착된다 (heuristic — 화면 중앙 고정 다이얼로그는 놓칠 수 있음).
    /// 꼬리 5줄에서 매치된 상태 목록 (같은 상태가 여러 줄이면 중복 포함 — 개수 의미).
    fn match_screen(&self, text: &str) -> Vec<SessionStatus> {
        const SCAN_TAIL_LINES: usize = 5;
        text.lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(SCAN_TAIL_LINES)
            .filter_map(|line| self.match_text(line.trim_end()))
            .collect()
    }

    /// 우선순위: error > approval > done > waiting (안전한 쪽 우선).
    fn match_text(&self, text: &str) -> Option<SessionStatus> {
        if self.error.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::Error);
        }
        if self.approval.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::NeedsApproval);
        }
        if self.done.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::Done);
        }
        if self.waiting.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::Waiting);
        }
        None
    }
}

fn priority(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::Error => 4,
        SessionStatus::NeedsApproval => 3,
        SessionStatus::Done => 2,
        SessionStatus::Waiting => 1,
        SessionStatus::Running => 0,
    }
}

/// 출력이 이 시간 동안 없으면 입력 대기로 추정한다 (약한 신호 — regex 우선).
const IDLE_THRESHOLD: Duration = Duration::from_secs(10);
/// stream line 버퍼 상한 (개행 없는 폭주 출력 대비)
const LINE_BUF_CAP: usize = 8 * 1024;

pub struct StatusDetector {
    patterns: StatusPatterns,
    /// 완성되지 않은 마지막 라인 (chunk 경계 대응).
    /// 바이트로 보관 — UTF-8 문자가 chunk 경계에 걸려도 라인 완성 시 온전하다.
    line_buf: Vec<u8>,
    last_output: Instant,
    /// 마지막으로 확정한 상태
    status: SessionStatus,
    /// 마지막으로 호출측에 보고한 상태 — evaluate가 변화 판정에 쓴다
    last_reported: SessionStatus,
    /// 사용자 입력으로 "응답 처리된" 화면 매치 (상태 → 소비 시점의 화면 출현 개수).
    /// 현재 개수가 이보다 크면(같은/다른 프롬프트가 새로 등장) 다시 활성.
    /// echo로 프롬프트 라인 텍스트가 바뀌어도 개수는 그대로라 재발화하지 않는다.
    consumed_counts: std::collections::HashMap<SessionStatus, usize>,
    /// 직전 evaluate의 화면 매치 개수 (on_input 소비 기준)
    last_screen_counts: std::collections::HashMap<SessionStatus, usize>,
    /// 현재 상태가 idle heuristic에서 온 것인가 (출력이 오면 해제되는 약한 신호)
    idle_waiting: bool,
    /// 현재 상태가 화면 패턴에서 온 것인가 (매치 라인이 화면에서 사라지면 해제 —
    /// 프롬프트 timeout/auto-continue 대응). stream/idle 유래면 false.
    screen_derived: bool,
}

impl StatusDetector {
    pub fn new(patterns: StatusPatterns) -> Self {
        Self {
            patterns,
            line_buf: Vec::new(),
            last_output: Instant::now(),
            status: SessionStatus::Running,
            last_reported: SessionStatus::Running,
            consumed_counts: std::collections::HashMap::new(),
            last_screen_counts: std::collections::HashMap::new(),
            idle_waiting: false,
            screen_derived: false,
        }
    }

    pub fn status(&self) -> SessionStatus {
        self.status
    }

    /// 사용자 입력 수신 — 현재 화면의 매치 프롬프트를 "응답됨"으로 소비한다.
    /// 소비는 직전 evaluate가 기록한 화면 매치 개수 기준.
    pub fn on_input(&mut self) {
        self.consumed_counts = self.last_screen_counts.clone();
        self.status = SessionStatus::Running;
        self.idle_waiting = false;
        self.screen_derived = false;
        // 개행 없이 떠 있던 프롬프트가 입력 echo로 라인 완성되며 재매치되는 것 방지
        self.line_buf.clear();
        // 입력도 활동이다 — idle 타이머 리셋 (즉시 Waiting 재발화 방지)
        self.last_output = Instant::now();
    }

    /// output chunk 수신 — stream line regex 단계.
    ///
    /// regex로 잡힌 상태는 **latch**된다: 뒤따르는 무매치 출력(스택트레이스 등)이
    /// 상태를 지우지 않는다 — 해제는 사용자 입력(on_input)으로만.
    /// idle heuristic이 만든 Waiting만 출력으로 해제된다 (활동 재개).
    pub fn on_output(&mut self, chunk: &[u8]) {
        self.last_output = Instant::now();
        if self.idle_waiting {
            self.status = SessionStatus::Running;
            self.idle_waiting = false;
        }
        self.line_buf.extend_from_slice(chunk);
        // 완성된 라인들 평가, 미완 꼬리는 유지 (\n은 ASCII라 UTF-8 문자를 가르지 않는다)
        while let Some(pos) = self.line_buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            // >= : 같은 상태여도 stream 매치면 latch로 굳힌다 (화면에서 사라져도 유지).
            // 우선순위는 상태별 고유값이라 다른 상태로의 다운그레이드는 없다.
            if let Some(status) = self.patterns.match_text(line.trim_end())
                && priority(status) >= priority(self.status)
            {
                self.status = status;
                self.screen_derived = false; // stream 유래로 전환 (latch)
            }
        }
        if self.line_buf.len() > LINE_BUF_CAP {
            let cut = self.line_buf.len() - LINE_BUF_CAP;
            self.line_buf.drain(..cut);
        }
    }

    /// batch tick 평가 — 화면 텍스트 패턴(2단) + idle heuristic(3단)을 반영하고,
    /// stream 단계(on_output)에서의 변화까지 포함해 "마지막 보고 이후 바뀌었으면" Some.
    /// `screen_text`는 경량 grid 조회 결과 (snapshot 아님).
    pub fn evaluate(&mut self, screen_text: Option<&str>) -> Option<SessionStatus> {
        // 화면 패턴: TUI처럼 개행 없이 화면에만 나타나는 프롬프트 감지 (라인 단위).
        // edge-trigger — 같은 프롬프트 라인이 화면에 남아 있어도 재발화하지 않는다
        // (응답 후 상태 고착 방지). 진짜 계속 대기 중이면 idle이 백스톱.
        if let Some(text) = screen_text {
            let mut counts: std::collections::HashMap<SessionStatus, usize> =
                std::collections::HashMap::new();
            for status in self.patterns.match_screen(text) {
                *counts.entry(status).or_insert(0) += 1;
            }
            self.last_screen_counts = counts.clone();
            // level-trigger: 매치가 화면에 남아 있는 동안 상태 유지. 단 사용자 입력으로
            // 소비된 개수만큼은 무시 — echo로 라인 텍스트가 바뀌어도 개수는 그대로라
            // 고착 안 되고, 개수가 늘면(반복 프롬프트) 재활성.
            let mut best: Option<SessionStatus> = None;
            for (status, count) in &counts {
                let consumed = self.consumed_counts.get(status).copied().unwrap_or(0);
                if *count > consumed {
                    best = Some(match best {
                        Some(prev) if priority(prev) >= priority(*status) => prev,
                        _ => *status,
                    });
                }
            }
            // 사라진 상태의 소비 기록 정리 (재등장 시 새 프롬프트)
            self.consumed_counts
                .retain(|status, _| counts.contains_key(status));
            if let Some(status) = best {
                if priority(status) > priority(self.status) || self.screen_derived {
                    self.status = status;
                    // Done/Error는 결과 상태 — stream 매치처럼 latch한다.
                    // Waiting/NeedsApproval만 화면에서 사라지면 해제되는 transient.
                    self.screen_derived = matches!(
                        status,
                        SessionStatus::Waiting | SessionStatus::NeedsApproval
                    );
                }
            } else if self.screen_derived {
                // transient 화면 프롬프트가 사라짐 → 해제 (timeout/auto-continue/진행 재개)
                self.status = SessionStatus::Running;
                self.screen_derived = false;
            }
        }
        // idle: 어떤 신호도 없고 출력이 멎었으면 입력 대기 추정 (약한 신호 — 출력으로 해제)
        if self.status == SessionStatus::Running && self.last_output.elapsed() >= IDLE_THRESHOLD {
            self.status = SessionStatus::Waiting;
            self.idle_waiting = true;
        }
        if self.status != self.last_reported {
            self.last_reported = self.status;
            Some(self.status)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> StatusPatterns {
        StatusPatterns::compile(
            Some("WAITING_FOR_INPUT"),
            Some("APPROVE\\?"),
            Some("(?i)error:"),
            Some("ALL DONE"),
        )
    }

    #[test]
    fn stream_line_감지와_chunk_경계() {
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"working...\nWAITING_");
        assert_eq!(d.status(), SessionStatus::Running); // 라인 미완성
        d.on_output(b"FOR_INPUT\n");
        assert_eq!(d.status(), SessionStatus::Waiting);
    }

    #[test]
    fn utf8_문자가_chunk_경계에_걸려도_매칭() {
        let p = StatusPatterns::compile(Some("입력 대기"), None, None, None);
        let mut d = StatusDetector::new(p);
        let bytes = "상태: 입력 대기\n".as_bytes();
        // 멀티바이트 문자 중간에서 분할
        let (a, b) = bytes.split_at(9);
        d.on_output(a);
        d.on_output(b);
        assert_eq!(d.status(), SessionStatus::Waiting);
    }

    #[test]
    fn 우선순위_error_우선() {
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"ALL DONE but Error: failed\n");
        assert_eq!(d.status(), SessionStatus::Error);
    }

    #[test]
    fn 매치는_후속_출력에_유실되지_않고_입력으로_해제() {
        let mut d = StatusDetector::new(patterns());
        // 매치 라인 뒤에 무매치 출력(스택트레이스류)이 chunk로 이어져도 latch 유지
        d.on_output(b"Error: failed\n");
        d.on_output(b"  at line 42\n  at main\n");
        assert_eq!(d.status(), SessionStatus::Error);
        assert_eq!(d.evaluate(None), Some(SessionStatus::Error));
        // 사용자 입력이 상태를 해제한다
        d.on_input();
        assert_eq!(d.evaluate(None), Some(SessionStatus::Running));
    }

    #[test]
    fn 화면_텍스트_패턴_감지() {
        let mut d = StatusDetector::new(patterns());
        // 개행 없는 TUI 프롬프트 — stream 단계에선 미감지
        d.on_output(b"APPROVE? [y/n] ");
        assert_eq!(d.status(), SessionStatus::Running);
        let changed = d.evaluate(Some("some ui\nAPPROVE? [y/n]"));
        assert_eq!(changed, Some(SessionStatus::NeedsApproval));
        // 같은 상태 재평가는 변화 없음
        assert_eq!(d.evaluate(Some("some ui\nAPPROVE? [y/n]")), None);
    }

    #[test]
    fn 화면_유래_대기를_같은상태_stream이_latch() {
        // 화면 프롬프트로 Waiting → 같은 Waiting을 stream 라인으로도 확정하면 latch
        let p = StatusPatterns::compile(Some("WAIT"), None, None, None);
        let mut d = StatusDetector::new(p);
        d.evaluate(Some("WAIT")); // screen_derived Waiting
        d.on_output(b"WAIT now\n"); // stream 매치 (같은 상태) → latch
        // 프롬프트가 화면에서 사라져도 유지 (stream latch)
        assert_eq!(d.evaluate(Some("gone")), None);
        assert_eq!(d.status(), SessionStatus::Waiting);
    }

    #[test]
    fn 화면_유래_결과상태는_소멸해도_유지() {
        let p = StatusPatterns::compile(None, None, Some("FATAL"), None);
        let mut d = StatusDetector::new(p);
        d.evaluate(Some("FATAL: crash")); // 개행 없는 TUI 에러
        assert_eq!(d.status(), SessionStatus::Error);
        // 화면이 바뀌어 라인이 사라져도 결과 상태(❌)는 유지 (SessionExited 전까지)
        assert_eq!(d.evaluate(Some("other screen")), None);
        assert_eq!(d.status(), SessionStatus::Error);
    }

    #[test]
    fn 화면_프롬프트_소멸시_해제() {
        let p = StatusPatterns::compile(Some("WAIT>"), None, None, None);
        let mut d = StatusDetector::new(p);
        d.evaluate(Some("WAIT>"));
        assert_eq!(d.status(), SessionStatus::Waiting);
        // 프롬프트가 화면에서 사라지면(auto-continue 등) 입력 없이도 해제
        assert_eq!(
            d.evaluate(Some("progress...")),
            Some(SessionStatus::Running)
        );
    }

    #[test]
    fn 화면_패턴_앵커는_라인_기준() {
        let p = StatusPatterns::compile(Some("^대기중$"), None, None, None);
        let mut d = StatusDetector::new(p);
        // 화면 중간 라인에 있는 앵커 패턴 — 전체 문자열 매칭이면 실패했을 케이스
        let changed = d.evaluate(Some("헤더\n대기중\n푸터"));
        assert_eq!(changed, Some(SessionStatus::Waiting));
    }

    #[test]
    fn 프롬프트_잔존시_유지_입력으로_소비() {
        let p = StatusPatterns::compile(Some("PRESS_ANY_KEY"), None, None, None);
        let mut d = StatusDetector::new(p);
        d.evaluate(Some("PRESS_ANY_KEY"));
        assert_eq!(d.status(), SessionStatus::Waiting);
        // TUI가 다른 줄을 재도장(출력) — 프롬프트가 화면에 살아 있으면 유지
        d.on_output(b"redraw\n");
        d.evaluate(Some("PRESS_ANY_KEY\nredraw"));
        assert_eq!(d.status(), SessionStatus::Waiting);
        // 사용자 입력 → 응답 처리. 텍스트가 남아 있어도 Running 복귀
        d.on_input();
        d.on_output(b"echo\n");
        assert_eq!(
            d.evaluate(Some("PRESS_ANY_KEY\necho")),
            Some(SessionStatus::Running)
        );
        // 프롬프트가 사라졌다가 다시 나타나면 재감지
        d.evaluate(Some("working..."));
        assert_eq!(
            d.evaluate(Some("PRESS_ANY_KEY")),
            Some(SessionStatus::Waiting)
        );
    }

    #[test]
    fn 같은_프롬프트의_새_복사본은_재감지() {
        let p = StatusPatterns::compile(None, Some("APPROVE\\?"), None, None);
        let mut d = StatusDetector::new(p);
        assert_eq!(
            d.evaluate(Some("APPROVE?")),
            Some(SessionStatus::NeedsApproval)
        );
        d.on_input(); // 응답 — 현재 매치(1회)를 소비
        d.on_output(b"y\n");
        // 같은 텍스트 프롬프트가 하나 더 출현 (2회 > 소비 1회) → 재감지
        d.evaluate(Some("APPROVE?\ny\nAPPROVE?"));
        assert_eq!(d.status(), SessionStatus::NeedsApproval);
    }

    #[test]
    fn 입력은_미완_라인과_idle_타이머를_리셋() {
        let p = StatusPatterns::compile(Some("PROMPT>"), None, None, None);
        let mut d = StatusDetector::new(p);
        d.on_output(b"PROMPT>"); // 개행 없는 프롬프트 — line_buf에 잔존
        d.on_input();
        // 응답 echo가 개행으로 라인을 완성해도 옛 프롬프트 텍스트는 재매치 안 됨
        d.on_output(b" y\n");
        assert_eq!(d.status(), SessionStatus::Running);
        // idle 타이머도 리셋 — 입력 직후 평가에서 Waiting 재발화 없음
        let mut d2 = StatusDetector::new(StatusPatterns::compile(None, None, None, None));
        d2.last_output = Instant::now() - Duration::from_secs(11);
        d2.on_input();
        assert_eq!(d2.evaluate(None), None);
    }

    #[test]
    fn idle_heuristic() {
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"busy\n");
        d.last_output = Instant::now() - Duration::from_secs(11);
        assert_eq!(d.evaluate(None), Some(SessionStatus::Waiting));
    }

    #[test]
    fn 잘못된_regex는_무시() {
        let p = StatusPatterns::compile(Some("(unclosed"), None, None, None);
        assert!(p.is_empty());
    }
}
