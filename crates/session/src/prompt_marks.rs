//! OSC 133 프롬프트 마크 (셸 통합 1단계 — 2026-07-17).
//!
//! zdot 래퍼(app crate env_reload.rs)의 zsh precmd 훅이 프롬프트 직전에
//! `ESC]133;A BEL`을 쏘고, 이 스캐너가 PTY 출력 스트림에서 그 마크를 찾아
//! **절대 라인 번호**로 저장한다. 스캐너는 storage::logs의 StripState와 같은
//! "chunk 경계에 안전한 상태머신" 문제를 푼다 — 그 모델을 133 전용으로 축소했다.
//!
//! 좌표계: Ground 상태에서 센 LF 누적 카운트 = 현재(최하단) 라인의 절대 번호.
//! 마크의 `line_from_bottom` = 현재 카운트 − 마크 값 — T3 검색
//! (`ScrollbackMatch::line_from_bottom`, 최하단=0)과 같은 의미라 점프 수식도
//! 검색의 스크롤 수식과 동일하다. wrapped 라인은 LF 없이 시각 라인을 늘리므로
//! 근사 오차가 있다(베스트 에포트 — 마크는 항법이지 정밀 좌표가 아니다).
//! alt screen(vim/less)의 LF는 스크롤백 라인을 만들지 않으므로 세지 않는다.

use std::collections::VecDeque;

/// 마크 보관 상한 — 초과 시 오래된 것부터 버린다.
const MAX_PROMPT_MARKS: usize = 200;
/// OSC payload 판별에 필요한 선두 바이트 수 ("133;A;"까지).
const OSC_HEAD_CAP: usize = 6;
/// CSI 파라미터 수집 상한 (StripState와 동일 — 깨진 스트림 방어).
const CSI_PARAMS_CAP: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum ScanPhase {
    #[default]
    Ground,
    /// ESC 수신 직후 (다음 바이트로 종류 결정)
    Esc,
    /// CSI 본문 (최종 바이트 0x40..=0x7e까지) — alt screen 전환 감지용
    Csi,
    /// OSC 본문 (BEL 또는 ESC\ 까지)
    Osc,
    /// OSC 안에서 ESC 수신 (\이면 종료)
    OscEsc,
}

/// OSC 133 스캐너 + 마크 저장소. [`crate::Session`]이 pump마다 출력 chunk를 넘긴다.
#[derive(Debug, Default)]
pub(crate) struct PromptMarks {
    phase: ScanPhase,
    /// OSC payload 선두 — "133;A" 판별에 필요한 만큼만 수집.
    osc_head: [u8; OSC_HEAD_CAP],
    osc_len: u8,
    /// CSI 파라미터 바이트 — alt screen 전환(?1049 등) 판별용.
    csi_params: [u8; CSI_PARAMS_CAP],
    csi_len: u8,
    /// alt screen(?1049/?1047/?47) 안에서는 LF가 스크롤백 라인을 만들지 않는다.
    alt_screen: bool,
    /// Ground에서 센 LF 누적 = 현재 라인의 절대 번호.
    line: u64,
    /// 프롬프트 시작(133;A) 마크의 절대 라인 번호 — 오래된 순.
    marks: VecDeque<u64>,
}

impl PromptMarks {
    /// PTY 출력 chunk 하나를 스캔한다 — escape가 chunk 경계에 걸려도 이어간다.
    pub(crate) fn scan(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            match self.phase {
                ScanPhase::Ground => match byte {
                    0x1b => self.phase = ScanPhase::Esc,
                    b'\n' if !self.alt_screen => self.line += 1,
                    _ => {}
                },
                ScanPhase::Esc => match byte {
                    b'[' => {
                        self.phase = ScanPhase::Csi;
                        self.csi_len = 0;
                    }
                    b']' => {
                        self.phase = ScanPhase::Osc;
                        self.osc_len = 0;
                    }
                    // 2바이트 escape (ESC =, ESC > 등) — 이 바이트로 종료
                    _ => self.phase = ScanPhase::Ground,
                },
                ScanPhase::Csi => {
                    if (0x30..=0x3f).contains(&byte) {
                        if (self.csi_len as usize) < CSI_PARAMS_CAP {
                            self.csi_params[self.csi_len as usize] = byte;
                            self.csi_len += 1;
                        }
                    } else if (0x40..=0x7e).contains(&byte) {
                        // alt screen 전환 — vim/less의 화면 내 LF가 좌표를 밀지 않게 한다.
                        let params = &self.csi_params[..self.csi_len as usize];
                        if matches!(params, b"?1049" | b"?1047" | b"?47") {
                            match byte {
                                b'h' => self.alt_screen = true,
                                b'l' => self.alt_screen = false,
                                _ => {}
                            }
                        }
                        self.phase = ScanPhase::Ground;
                    }
                    // 그 외(intermediate 0x20..=0x2f)는 본문 계속
                }
                ScanPhase::Osc => match byte {
                    0x07 => self.finish_osc(),
                    0x1b => self.phase = ScanPhase::OscEsc,
                    _ => {
                        if (self.osc_len as usize) < OSC_HEAD_CAP {
                            self.osc_head[self.osc_len as usize] = byte;
                            self.osc_len += 1;
                        }
                    }
                },
                ScanPhase::OscEsc => {
                    // ESC\ 종결. 그 외 바이트는 OSC 본문 계속으로 취급 (StripState 관례)
                    if byte == b'\\' {
                        self.finish_osc();
                    } else {
                        self.phase = ScanPhase::Osc;
                    }
                }
            }
        }
    }

    /// OSC 종결 — payload가 `133;A`(옵션 파라미터 `133;A;…` 허용)면 마크로 기록한다.
    fn finish_osc(&mut self) {
        self.phase = ScanPhase::Ground;
        let head = &self.osc_head[..self.osc_len as usize];
        if head != b"133;A" && head != b"133;A;" {
            return;
        }
        // 같은 라인의 중복 마크(redraw 등)는 한 번만.
        if self.marks.back() == Some(&self.line) {
            return;
        }
        if self.marks.len() >= MAX_PROMPT_MARKS {
            self.marks.pop_front();
        }
        self.marks.push_back(self.line);
    }

    /// 이전(−)/다음(+) 프롬프트로 가는 스크롤 델타 (양수 = 과거로 — Scroll 관례).
    /// 수식은 T3 검색의 스크롤 수식과 동치: `desired = (b − rows/2).clamp(0, history)`,
    /// `delta = desired − scroll_offset`. 기준은 현재 화면 중앙 라인
    /// (`scroll_offset + rows/2`). 트림으로 스크롤백 밖으로 사라진 마크는 여기서
    /// 제거한다(조회 시 클램프/제거 — 마크는 오래된 순이라 앞에서부터 지운다).
    pub(crate) fn jump_delta(
        &mut self,
        direction: i8,
        scroll_offset: i32,
        rows: usize,
        history: usize,
    ) -> Option<i32> {
        let rows_half = (rows / 2) as i64;
        let total = (history + rows) as i64;
        while self
            .marks
            .front()
            .is_some_and(|mark| (self.line - mark) as i64 > total - 1)
        {
            self.marks.pop_front();
        }
        let cur = i64::from(scroll_offset) + rows_half;
        let bottoms = self.marks.iter().map(|mark| (self.line - mark) as i64);
        let target = match direction.signum() {
            // 이전(과거) — 기준보다 위(b 큰)인 마크 중 가장 가까운 것
            -1 => bottoms.filter(|b| *b > cur).min(),
            // 다음(최신) — 기준보다 아래(b 작은)인 마크 중 가장 가까운 것
            1 => bottoms.filter(|b| *b < cur).max(),
            _ => None,
        }?;
        let desired = (target - rows_half).clamp(0, history as i64);
        let delta = desired - i64::from(scroll_offset);
        (delta != 0).then_some(delta as i32)
    }

    #[cfg(test)]
    fn mark_count(&self) -> usize {
        self.marks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(marks: &mut PromptMarks, chunks: &[&[u8]]) {
        for chunk in chunks {
            marks.scan(chunk);
        }
    }

    #[test]
    fn 마크는_bel과_st_양_종결을_모두_인식한다() {
        let mut marks = PromptMarks::default();
        marks.scan(b"\x1b]133;A\x07one\n\x1b]133;A\x1b\\two\n");
        assert_eq!(marks.mark_count(), 2);
        assert_eq!(marks.marks, [0, 1]);
    }

    #[test]
    fn chunk_경계에_걸친_마크도_인식한다() {
        let mut marks = PromptMarks::default();
        // ESC / ']' / "133;" / "A" / BEL 전부 다른 chunk
        scan_all(&mut marks, &[b"line\n\x1b", b"]", b"133;", b"A", b"\x07"]);
        assert_eq!(marks.marks, [1]);
    }

    #[test]
    fn 비_133_osc와_다른_133_동작은_무시한다() {
        let mut marks = PromptMarks::default();
        marks.scan(b"\x1b]0;title\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b]1337;x\x07");
        assert_eq!(marks.mark_count(), 0);
        // 파라미터 붙은 A(133;A;cl=m)는 마크다 — 같은 라인 중복은 한 번만.
        marks.scan(b"\x1b]133;A;cl=m\x07\x1b]133;A\x07");
        assert_eq!(marks.marks, [0]);
    }

    #[test]
    fn alt_screen의_lf는_라인을_세지_않는다() {
        let mut marks = PromptMarks::default();
        marks.scan(b"\x1b]133;A\x07a\n\x1b[?1049hin-alt\n\n\n\x1b[?1049l\x1b]133;A\x07");
        // alt 안 LF 3개는 무시 — 두 번째 마크는 라인 1
        assert_eq!(marks.marks, [0, 1]);
    }

    #[test]
    fn 마크_상한을_넘으면_오래된_것부터_버린다() {
        let mut marks = PromptMarks::default();
        for _ in 0..(MAX_PROMPT_MARKS + 10) {
            marks.scan(b"\x1b]133;A\x07cmd\n");
        }
        assert_eq!(marks.mark_count(), MAX_PROMPT_MARKS);
        // 가장 오래된 10개(라인 0..9)가 버려졌다
        assert_eq!(marks.marks.front(), Some(&10));
    }

    #[test]
    fn 트림으로_사라진_마크는_조회_시_제거된다() {
        let mut marks = PromptMarks::default();
        marks.scan(b"\x1b]133;A\x07old\n");
        for _ in 0..100 {
            marks.scan(b"fill\n");
        }
        marks.scan(b"\x1b]133;A\x07new\n");
        // history 20 + rows 10 = 총 30라인만 남음 — 라인 0의 old 마크(b=102)는 제거
        assert_eq!(marks.jump_delta(-1, 0, 10, 20), None);
        assert_eq!(marks.mark_count(), 1);
    }

    /// 점프 수식은 T3 검색(app ui/workspace.rs)의 스크롤 수식과 동치다:
    /// `desired = (line_from_bottom − rows/2).clamp(0, history)`.
    #[test]
    fn 점프_델타는_검색_스크롤_수식과_동치다() {
        let mut marks = PromptMarks::default();
        marks.scan(b"\x1b]133;A\x07p1\n"); // 라인 0
        for _ in 0..40 {
            marks.scan(b"fill\n");
        }
        marks.scan(b"\x1b]133;A\x07p2\n"); // 라인 41, 이후 카운터 42
        // 내용 43라인(0..=42)을 담은 10행 화면 — history 33 (총 43).
        let (rows, history) = (10usize, 33usize);

        // 맨 아래(offset 0)에서 이전 — p2(b=1)는 중앙(5) 아래라 건너뛰고 p1(b=42)로.
        let b = 42i64;
        let desired = (b - 10 / 2).clamp(0, history as i64); // = 33 (클램프)
        assert_eq!(marks.jump_delta(-1, 0, rows, history), Some(desired as i32));

        // 그 위치에서 다음 — p2(b=1): desired = (1−5).clamp(0,33) = 0 → 맨 아래 복귀.
        let offset = desired as i32;
        assert_eq!(marks.jump_delta(1, offset, rows, history), Some(-offset));

        // 맨 아래에서 다음 — p2(b=1)는 이미 목표 위치(desired 0 = 현재)라 델타 없음.
        assert_eq!(marks.jump_delta(1, 0, rows, history), None);
        // 델타 0(이미 그 위치)이면 None.
        assert_eq!(marks.jump_delta(-1, offset, rows, history), None);
    }
}
