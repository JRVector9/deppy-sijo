//! Status Detector (설계문서 PR-12).
//! stream line regex + 화면 텍스트 패턴 + output idle heuristic 3단 병행.
//! 감지 주기는 output batch 단위 — 호출측(runtime worker)이 tick마다 부른다.

use std::time::{Duration, Instant};

/// agent의 감지 상태. Exited는 lifecycle 소관이라 여기 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SessionStatus {
    Running,
    /// 입력 대기 — regex/화면 프롬프트로 감지된 "진짜 사용자 입력 필요"(needs-input).
    Waiting,
    NeedsApproval,
    /// 작업 완료 후 프롬프트 복귀(세션은 살아있음). output idle heuristic이 만든다.
    /// Waiting과 달리 대기 중인 프롬프트가 없는 "쉬는 중" 상태 (cmux Idle 대응).
    Idle,
    Error,
    Done,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionStatusView {
    pub status: SessionStatus,
    pub detected_status: SessionStatus,
    pub source: StatusSource,
    pub confidence: Option<StatusConfidence>,
    pub user_override: Option<SessionStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StatusSource {
    ProcessExit,
    StreamRegex,
    ScreenText,
    IdleHeuristic,
    UserOverride,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatusConfidence {
    pub score: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UserStatusOverride {
    Mark(SessionStatus),
    Clear,
}

impl SessionStatusView {
    pub fn detected(
        status: SessionStatus,
        source: StatusSource,
        confidence: Option<StatusConfidence>,
        user_override: Option<SessionStatus>,
    ) -> Self {
        Self {
            status: user_override.unwrap_or(status),
            detected_status: status,
            source: if user_override.is_some() {
                StatusSource::UserOverride
            } else {
                source
            },
            confidence,
            user_override,
        }
    }

    pub fn process_exit(status: SessionStatus) -> Self {
        Self {
            status,
            detected_status: status,
            source: StatusSource::ProcessExit,
            confidence: Some(StatusConfidence {
                score: 1.0,
                reason: "process_exit".into(),
            }),
            user_override: None,
        }
    }
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

struct ScreenMatchResult {
    matches: Vec<(SessionStatus, String)>,
    lines_scanned: usize,
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
    /// 꼬리 5줄에서 매치된 (상태, 라인 원문) 목록 — 라인 원문은 소비 판정
    /// (echo 연장 vs 새 프롬프트 구분)에 쓴다.
    fn match_screen(&self, text: &str) -> ScreenMatchResult {
        const SCAN_TAIL_LINES: usize = 5;
        let mut lines_scanned = 0;
        let matches = text
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(SCAN_TAIL_LINES)
            .filter_map(|line| {
                lines_scanned += 1;
                let line = line.trim_end();
                self.match_text(line)
                    .map(|status| (status, line.to_owned()))
            })
            .collect();
        ScreenMatchResult {
            matches,
            lines_scanned,
        }
    }

    /// 우선순위: error > approval > done > waiting (안전한 쪽 우선).
    /// config regex에 더해 claude/codex 공통 프롬프트 built-in 패턴도 항상 검사한다
    /// (#3b — 셸에서 수동 실행한 claude/codex도 승인/입력 대기를 감지).
    fn match_text(&self, text: &str) -> Option<SessionStatus> {
        if self.error.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::Error);
        }
        if self.approval.as_ref().is_some_and(|re| re.is_match(text))
            || BUILTIN.approval.is_match(text)
        {
            return Some(SessionStatus::NeedsApproval);
        }
        if self.done.as_ref().is_some_and(|re| re.is_match(text)) {
            return Some(SessionStatus::Done);
        }
        if self.waiting.as_ref().is_some_and(|re| re.is_match(text))
            || BUILTIN.waiting.is_match(text)
        {
            return Some(SessionStatus::Waiting);
        }
        None
    }
}

/// claude/codex 등 공통 CLI 프롬프트를 감지하는 내장 패턴 (#3b). config regex가 없는
/// 셸에서도 승인/입력 대기 상태를 잡아 상태 레일이 반응하게 한다. 화면 꼬리 라인에
/// 매칭되므로 프롬프트가 사라지면(응답) 해제된다.
struct BuiltinPatterns {
    approval: regex::Regex,
    waiting: regex::Regex,
}

static BUILTIN: std::sync::LazyLock<BuiltinPatterns> = std::sync::LazyLock::new(|| {
    BuiltinPatterns {
        // 승인/확인 프롬프트. codex/claude는 옵션 목록(❯ 1. Yes …)이 화면 위쪽에 있고
        // 스캔 대상인 꼬리 5줄엔 하단 푸터가 들어오므로, 푸터/문구를 여러 개 중복으로
        // 잡아 최소 하나가 꼬리에 걸리게 한다: "Would you like to run", "Yes, proceed",
        // "Press enter to confirm", "don't ask again", "tell Codex/Claude", y/n 등.
        approval: regex::Regex::new(
            r"(?i)(do you want to (proceed|make this edit|create|run|continue|apply)|would you like to run|yes,?\s*proceed|press enter to confirm|don'?t ask again|tell (codex|claude)\b|allow (this )?(command|edit|action|tool)|approve this|grant\s+.{0,24}permission|\[y/n\]|\(y/n\)|\by/n\?|❯\s*1\.\s*yes\b)",
        )
        .expect("built-in approval regex"),
        // 그 외 입력 대기: "Press Enter", "type ... to continue", "waiting for input".
        waiting: regex::Regex::new(
            r"(?i)(press enter to continue|type\s+.{0,24}\s+to continue|waiting for (your )?(input|response)|paste your|enter your\s)",
        )
        .expect("built-in waiting regex"),
    }
});

fn priority(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::Error => 4,
        SessionStatus::NeedsApproval => 3,
        SessionStatus::Done => 2,
        SessionStatus::Waiting => 1,
        SessionStatus::Idle => 0,
        SessionStatus::Running => 0,
    }
}

/// 출력이 이 시간 동안 없으면 입력 대기로 추정한다 (약한 신호 — regex 우선).
const IDLE_THRESHOLD: Duration = Duration::from_secs(10);
/// stream line 버퍼 상한 (개행 없는 폭주 출력 대비)
const LINE_BUF_CAP: usize = 8 * 1024;

/// 입력으로 응답 처리된 화면 프롬프트 하나 (같은 텍스트는 count로 합산).
struct ConsumedPrompt {
    status: SessionStatus,
    prefix: String,
    count: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatusDetectorStats {
    pub stream_chunks: u64,
    pub stream_bytes: u64,
    pub stream_lines: u64,
    pub screen_scans: u64,
    pub screen_lines_scanned: u64,
    pub screen_scans_skipped_empty_patterns: u64,
    pub idle_evaluations: u64,
}

pub struct StatusDetector {
    patterns: StatusPatterns,
    /// 완성되지 않은 마지막 라인 (chunk 경계 대응).
    /// 바이트로 보관 — UTF-8 문자가 chunk 경계에 걸려도 라인 완성 시 온전하다.
    line_buf: Vec<u8>,
    /// line_buf에서 이미 개행 스캔을 마친 접두 길이 — 재스캔 방지(2026-07-14 프로파일링).
    scanned: usize,
    last_output: Instant,
    /// 마지막으로 확정한 상태
    status: SessionStatus,
    /// 마지막으로 호출측에 보고한 상태 — evaluate가 변화 판정에 쓴다
    last_reported: SessionStatus,
    /// 사용자 입력으로 "응답 처리된" 화면 프롬프트.
    /// 라인 prefix 기준 — echo로 라인이 늘어난 것(prefix 연장)은 같은 프롬프트,
    /// 다른 텍스트는 (같은 상태여도) 새 프롬프트로 재감지한다.
    /// count는 같은 텍스트 반복 프롬프트(새 복사본) 구분용.
    consumed: Vec<ConsumedPrompt>,
    /// 직전 evaluate의 화면 매치 (on_input 소비 기준)
    last_screen_matches: Vec<(SessionStatus, String)>,
    /// 현재 상태가 idle heuristic에서 온 것인가 (출력이 오면 해제되는 약한 신호)
    idle_waiting: bool,
    /// 현재 상태가 화면 패턴에서 온 것인가 (매치 라인이 화면에서 사라지면 해제 —
    /// 프롬프트 timeout/auto-continue 대응). stream/idle 유래면 false.
    screen_derived: bool,
    /// 출력이 없어도 다음 tick에 화면 스캔이 필요함 (입력 직후 — worker가 소비)
    screen_scan_requested: bool,
    source: StatusSource,
    stats: StatusDetectorStats,
}

impl StatusDetector {
    pub fn new(patterns: StatusPatterns) -> Self {
        Self {
            patterns,
            line_buf: Vec::new(),
            scanned: 0,
            last_output: Instant::now(),
            status: SessionStatus::Running,
            last_reported: SessionStatus::Running,
            consumed: Vec::new(),
            last_screen_matches: Vec::new(),
            idle_waiting: false,
            screen_derived: false,
            screen_scan_requested: false,
            source: StatusSource::IdleHeuristic,
            stats: StatusDetectorStats::default(),
        }
    }

    /// 호출측이 `screen_text()`를 만들기 전에 묻는 비용 게이트.
    ///
    /// regex 패턴이 없는 세션은 idle heuristic만 필요하므로 출력 tick마다 backend grid
    /// 텍스트를 읽지 않는다. 입력 직후 예약도 이 경로에서 1회성으로 소비한다.
    pub fn should_scan_screen(&mut self, produced_output: bool) -> bool {
        // built-in claude/codex 패턴이 상시 활성이라 config가 비어도 화면을 스캔한다
        // (#3b — 셸에서 수동 실행한 claude/codex도 프롬프트 감지). 새 출력이 있거나
        // 입력 직후 재확인 요청이 있을 때만 스캔해 비용을 유계로 둔다.
        let requested = std::mem::take(&mut self.screen_scan_requested);
        produced_output || requested
    }

    pub fn status(&self) -> SessionStatus {
        self.status
    }

    pub fn stats(&self) -> StatusDetectorStats {
        self.stats
    }

    pub fn status_view(&self, user_override: Option<SessionStatus>) -> SessionStatusView {
        SessionStatusView::detected(
            self.status,
            self.source,
            Some(StatusConfidence {
                score: status_confidence(self.source),
                reason: status_source_reason(self.source).into(),
            }),
            user_override,
        )
    }

    /// 사용자 입력 수신 — 현재 화면의 매치 프롬프트를 "응답됨"으로 소비한다.
    /// 소비 단위는 (상태, 라인 텍스트) — echo로 연장된 라인은 같은 프롬프트로 본다.
    pub fn on_input(&mut self) {
        self.consumed.clear();
        for (status, line) in &self.last_screen_matches {
            match self
                .consumed
                .iter_mut()
                .find(|c| c.status == *status && c.prefix == *line)
            {
                Some(consumed) => consumed.count += 1,
                None => self.consumed.push(ConsumedPrompt {
                    status: *status,
                    prefix: line.clone(),
                    count: 1,
                }),
            }
        }
        // 응답이 화면을 다시 그리지 않아도(echo 없는 TUI) 다음 tick에 화면을
        // 재확인해야 한다 — 새 프롬프트가 이미 떠 있을 수 있다 (worker가 소비)
        self.screen_scan_requested = true; // built-in 패턴 상시 — 입력 후 항상 재확인 (#3b)
        self.status = SessionStatus::Running;
        self.source = StatusSource::IdleHeuristic;
        self.idle_waiting = false;
        self.screen_derived = false;
        // 개행 없이 떠 있던 프롬프트가 입력 echo로 라인 완성되며 재매치되는 것 방지
        self.line_buf.clear();
        self.scanned = 0;
        // 입력도 활동이다 — idle 타이머 리셋 (즉시 Waiting 재발화 방지)
        self.last_output = Instant::now();
    }

    /// hook이 보고한 새 턴 시작(UserPromptSubmit/PreToolUse) — 입력과 동일한 리셋이다.
    /// 턴이 시작됐다는 건 이전 턴의 프롬프트가 응답됐고 결과 상태도 지난 턴의 것이라는
    /// 뜻이다. 이 경로가 없으면 latch된 결과 상태(특히 error regex 오탐)는 사용자가 그
    /// pane에 직접 타이핑할 때까지 무기한 남는다.
    pub fn on_turn_start(&mut self) {
        self.on_input();
    }

    /// output chunk 수신 — stream line regex 단계.
    ///
    /// regex로 잡힌 상태는 **latch**된다: 뒤따르는 무매치 출력(스택트레이스 등)이
    /// 상태를 지우지 않는다 — 해제는 사용자 입력(on_input)으로만.
    /// idle heuristic이 만든 Waiting만 출력으로 해제된다 (활동 재개).
    pub fn on_output(&mut self, chunk: &[u8]) {
        self.stats.stream_chunks += 1;
        self.stats.stream_bytes += chunk.len() as u64;
        self.last_output = Instant::now();
        if self.idle_waiting {
            self.status = SessionStatus::Running;
            self.idle_waiting = false;
        }
        self.line_buf.extend_from_slice(chunk);
        // 완성된 라인들 평가, 미완 꼬리는 유지 (\n은 ASCII라 UTF-8 문자를 가르지 않는다).
        //
        // **이미 스캔한 구간은 다시 보지 않는다** (2026-07-14 프로파일링): 진행률 표시·
        // 스피너는 `\r`만 쓰고 `\n`을 보내지 않아 line_buf가 상한까지 차는데, 매 청크마다
        // 버퍼 전체를 처음부터 스캔하면 O(청크수 × CAP)가 된다. 실측에서 이 함수가
        // VTE 파서보다 8배 많은 CPU를 먹었다(sample: on_output 802 vs Handler::input 94).
        while let Some(rel) = self.line_buf[self.scanned..]
            .iter()
            .position(|b| *b == b'\n')
        {
            let pos = self.scanned + rel;
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            self.scanned = 0; // 앞을 잘라냈으니 남은 꼬리는 아직 안 본 구간이다
            self.stats.stream_lines += 1;
            let line = String::from_utf8_lossy(&line);
            // 시간상 나중의 stream 매치가 이전 상태를 대체한다 — 우선순위는
            // "같은 화면의 동시 매치" 충돌용이지 시간축 규칙이 아니다
            // (APPROVE? 다음 ALL DONE이 오면 최종 상태는 Done이어야 한다).
            if let Some(status) = self.patterns.match_text(line.trim_end()) {
                self.status = status;
                self.source = StatusSource::StreamRegex;
                self.screen_derived = false; // stream 유래로 전환 (latch)
            }
        }
        self.scanned = self.line_buf.len(); // 여기까진 개행이 없음이 확정됐다
        // 상한 초과 트림 — 매번 CAP만큼 memmove하지 않도록 2×CAP에서 한 번에 자른다
        // (개행 없는 스트림에서 청크마다 drain하면 memmove가 CPU를 먹었다).
        if self.line_buf.len() > LINE_BUF_CAP * 2 {
            let cut = self.line_buf.len() - LINE_BUF_CAP;
            self.line_buf.drain(..cut);
            self.scanned = self.line_buf.len();
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
            self.stats.screen_scans += 1;
            let result = self.patterns.match_screen(text);
            self.stats.screen_lines_scanned += result.lines_scanned as u64;
            let matches = result.matches;
            self.last_screen_matches = matches.clone();
            // level-trigger: 매치가 화면에 남아 있는 동안 상태 유지. 단 사용자 입력으로
            // 소비된 프롬프트는 무시한다. 판정은 라인 prefix — echo로 연장된 라인
            // ("APPROVE? [y/n]" → "APPROVE? [y/n] y")은 같은 프롬프트라 억제되고,
            // 다른 텍스트의 프롬프트나 같은 텍스트의 새 복사본(count 초과)은 재감지.
            let mut budgets: Vec<(usize, usize)> = self
                .consumed
                .iter()
                .enumerate()
                .map(|(i, c)| (i, c.count))
                .collect();
            let mut best: Option<SessionStatus> = None;
            for (status, line) in &matches {
                let suppressed = budgets.iter_mut().any(|(i, remaining)| {
                    let c = &self.consumed[*i];
                    if *remaining > 0 && c.status == *status && line.starts_with(&c.prefix) {
                        *remaining -= 1;
                        true
                    } else {
                        false
                    }
                });
                if !suppressed {
                    best = Some(match best {
                        Some(prev) if priority(prev) >= priority(*status) => prev,
                        _ => *status,
                    });
                }
            }
            // 화면에서 연장조차 사라진 소비 기록은 정리 (재등장 시 새 프롬프트)
            self.consumed.retain(|c| {
                matches
                    .iter()
                    .any(|(status, line)| *status == c.status && line.starts_with(&c.prefix))
            });
            if let Some(status) = best {
                if priority(status) > priority(self.status) || self.screen_derived {
                    self.status = status;
                    self.source = StatusSource::ScreenText;
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
                self.source = StatusSource::ScreenText;
                self.screen_derived = false;
            }
        }
        // idle: 어떤 신호도 없고 출력이 멎었으면 입력 대기 추정 (약한 신호 — 출력으로 해제)
        self.stats.idle_evaluations += 1;
        if self.status == SessionStatus::Running && self.last_output.elapsed() >= IDLE_THRESHOLD {
            // 출력이 멎었고 감지된 프롬프트가 없다 = 작업 완료·프롬프트 복귀(Idle).
            self.status = SessionStatus::Idle;
            self.source = StatusSource::IdleHeuristic;
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

fn status_confidence(source: StatusSource) -> f32 {
    match source {
        StatusSource::ProcessExit | StatusSource::UserOverride => 1.0,
        StatusSource::StreamRegex => 0.9,
        StatusSource::ScreenText => 0.8,
        StatusSource::IdleHeuristic => 0.4,
    }
}

fn status_source_reason(source: StatusSource) -> &'static str {
    match source {
        StatusSource::ProcessExit => "process_exit",
        StatusSource::StreamRegex => "stream_regex",
        StatusSource::ScreenText => "screen_text",
        StatusSource::IdleHeuristic => "idle_heuristic",
        StatusSource::UserOverride => "user_override",
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
    fn status_view는_source_confidence_override를_담는다() {
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"WAITING_FOR_INPUT\n");
        assert_eq!(d.evaluate(None), Some(SessionStatus::Waiting));
        let detected = d.status_view(None);
        assert_eq!(detected.status, SessionStatus::Waiting);
        assert_eq!(detected.source, StatusSource::StreamRegex);
        assert_eq!(
            detected.confidence.as_ref().map(|c| c.reason.as_str()),
            Some("stream_regex")
        );

        let overridden = d.status_view(Some(SessionStatus::Done));
        assert_eq!(overridden.status, SessionStatus::Done);
        assert_eq!(overridden.detected_status, SessionStatus::Waiting);
        assert_eq!(overridden.source, StatusSource::UserOverride);
        assert_eq!(overridden.user_override, Some(SessionStatus::Done));
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

    /// error regex 오탐이 그 pane에 타이핑할 때까지 무기한 남던 문제 — hook이 보고한
    /// 턴 시작도 입력과 동등한 해제 신호다(RuntimeCommand::NoteTurnStart).
    #[test]
    fn latch된_결과_상태는_턴_시작으로도_해제된다() {
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"Error: failed\n");
        assert_eq!(d.evaluate(None), Some(SessionStatus::Error));
        d.on_turn_start();
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
    fn 같은_상태의_다른_프롬프트는_소비와_무관하게_재감지() {
        // codex P1: count 기반 소비는 "APPROVE A → 입력 → APPROVE B"를 놓쳤다
        let p = StatusPatterns::compile(None, Some("APPROVE"), None, None);
        let mut d = StatusDetector::new(p);
        d.evaluate(Some("APPROVE write file A?"));
        assert_eq!(d.status(), SessionStatus::NeedsApproval);
        d.on_input(); // A에 응답
        // echo 라인은 억제 → Running 복귀가 보고된다 (아이콘 해제)
        assert_eq!(
            d.evaluate(Some("APPROVE write file A? y")),
            Some(SessionStatus::Running)
        );
        // 다른 텍스트의 새 approval 프롬프트 — 개수는 같아도 재감지돼야 한다
        assert_eq!(
            d.evaluate(Some("APPROVE delete file B?")),
            Some(SessionStatus::NeedsApproval)
        );
    }

    #[test]
    fn 나중_stream_매치가_이전_상태를_대체() {
        // codex P1: 우선순위 latch가 시간축을 무시해 approval 후 done이 씹혔다
        let p = StatusPatterns::compile(None, Some("APPROVE\\?"), None, Some("ALL DONE"));
        let mut d = StatusDetector::new(p);
        d.on_output(b"APPROVE?\n");
        assert_eq!(d.status(), SessionStatus::NeedsApproval);
        d.on_output(b"ALL DONE\n");
        assert_eq!(d.status(), SessionStatus::Done);
    }

    #[test]
    fn 입력은_화면_재스캔을_예약() {
        let p = StatusPatterns::compile(Some("WAIT"), None, None, None);
        let mut d = StatusDetector::new(p);
        assert!(!d.should_scan_screen(false));
        d.on_input();
        assert!(d.should_scan_screen(false));
        assert!(!d.should_scan_screen(false)); // 1회성
    }

    #[test]
    fn regex_없는_detector도_builtin_위해_화면을_스캔한다() {
        // #3b: config regex가 없어도 built-in claude/codex 패턴을 위해 화면을 스캔한다.
        let mut d = StatusDetector::new(StatusPatterns::compile(None, None, None, None));
        d.on_output(b"busy\n");
        assert!(d.should_scan_screen(true)); // 새 출력 → 스캔 (건너뛰지 않음)
        assert!(!d.should_scan_screen(false)); // 새 출력·요청 없으면 스캔 안 함
        d.on_input();
        assert!(d.should_scan_screen(false)); // 입력 후 재확인 요청됨
    }

    #[test]
    fn builtin_패턴이_claude_승인_프롬프트를_감지() {
        // #3b: config regex 없는 셸에서도 claude/codex 승인 프롬프트를 NeedsApproval로.
        let mut d = StatusDetector::new(StatusPatterns::compile(None, None, None, None));
        d.on_output(b"working\n");
        let screen = "Do you want to proceed?\n❯ 1. Yes\n  2. No";
        assert_eq!(d.evaluate(Some(screen)), Some(SessionStatus::NeedsApproval));
    }

    #[test]
    fn builtin_패턴이_codex_승인_푸터를_감지() {
        // codex는 옵션이 위쪽·푸터가 꼬리에 온다 — 꼬리 5줄의 푸터/문구로 감지(#92 사용자).
        let mut d = StatusDetector::new(StatusPatterns::compile(None, None, None, None));
        d.on_output(b"working\n");
        let screen = "  1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands (p)\n  3. No, and tell Codex what to do differently (esc)\n\nPress enter to confirm or esc to cancel";
        assert_eq!(d.evaluate(Some(screen)), Some(SessionStatus::NeedsApproval));
    }

    #[test]
    fn idle_heuristic() {
        // 출력이 멎으면 idle 휴리스틱은 Idle(작업완료·프롬프트 복귀)로 본다.
        // 진짜 입력 대기(Waiting)는 regex/화면 프롬프트로만 감지된다.
        let mut d = StatusDetector::new(patterns());
        d.on_output(b"busy\n");
        d.last_output = Instant::now() - Duration::from_secs(11);
        assert_eq!(d.evaluate(None), Some(SessionStatus::Idle));
    }

    #[test]
    fn 잘못된_regex는_무시() {
        let p = StatusPatterns::compile(Some("(unclosed"), None, None, None);
        assert!(p.is_empty());
    }

    /// 2026-07-14 프로파일링 회귀: 진행률/스피너 출력(`\r`만, 개행 없음)에서
    /// on_output이 매 청크마다 line_buf 전체를 재스캔해 CPU 1위를 차지했다
    /// (sample: on_output 802 vs VTE parse 94). 스캔 오프셋으로 새 구간만 본다.
    #[test]
    fn 개행_없는_스트림에서_line_buf를_재스캔하지_않는다() {
        let mut d = StatusDetector::new(patterns());
        // 개행이 전혀 없는 청크를 상한(8KB)을 넘길 만큼 흘린다
        for i in 0..4000 {
            d.on_output(format!("\rprogress={i}").as_bytes());
        }
        // 스캔 오프셋은 항상 버퍼 끝(= 개행 없음이 확정된 지점)
        assert_eq!(d.scanned, d.line_buf.len());
        // 버퍼는 2×CAP를 넘지 않게 유계 (트림이 동작)
        assert!(
            d.line_buf.len() <= LINE_BUF_CAP * 2,
            "line_buf 무한 증가: {}",
            d.line_buf.len()
        );
        // 개행이 오면 그 라인은 정상 평가된다(기능 회귀 없음)
        d.on_output(b"\nProceed? [y/n]\n");
        assert_eq!(d.status, SessionStatus::NeedsApproval);
    }
}
