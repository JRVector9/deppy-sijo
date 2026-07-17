//! 세션당 append-only redacted 로그 3종 (설계문서 7장):
//! redacted.ansi.log / redacted.plain.txt / events.redacted.jsonl
//! 호출측(runtime worker)이 redaction을 끝낸 바이트만 넘긴다 —
//! 이 모듈은 평문 secret을 받지 않는 것이 계약이다.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use deppy_core::SessionId;

pub struct SessionLogWriter {
    ansi: File,
    plain: File,
    events: File,
    /// chunk 경계에 걸친 escape 시퀀스 대응 — plain 변환기 상태 유지
    strip_state: StripState,
}

impl SessionLogWriter {
    /// `logs_root/<session id>/` 아래에 세 파일을 append 모드로 연다.
    pub fn open(logs_root: &Path, session: SessionId) -> anyhow::Result<Self> {
        Self::open_key(logs_root, &session.0.to_string())
    }

    /// 영속 세션 id를 디렉터리 키로 사용해 로그를 연다. 런타임의 숫자 SessionId는
    /// 프로세스 재시작마다 다시 1부터 시작하므로, 재시작 복원 대상은 이 API를 써야
    /// 이전 ANSI 로그와 같은 파일에 계속 append할 수 있다.
    pub fn open_key(logs_root: &Path, session_key: &str) -> anyhow::Result<Self> {
        let dir = Self::session_dir_key(logs_root, session_key)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("로그 디렉터리 생성 실패: {}", dir.display()))?;
        Ok(Self {
            ansi: append_only(&dir.join("redacted.ansi.log"))?,
            plain: append_only(&dir.join("redacted.plain.txt"))?,
            events: append_only(&dir.join("events.redacted.jsonl"))?,
            strip_state: StripState::default(),
        })
    }

    pub fn session_dir(logs_root: &Path, session: SessionId) -> PathBuf {
        logs_root.join(session.0.to_string())
    }

    /// 저장된 ANSI 로그를 복원할 때 사용하는 경로. key는 DB가 발급한 UUID지만,
    /// 방어적으로 단일 파일명 성분만 허용해 경로 탈출을 막는다.
    pub fn session_dir_key(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        let mut components = Path::new(session_key).components();
        let valid = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        anyhow::ensure!(
            valid && !session_key.is_empty(),
            "유효하지 않은 session log key"
        );
        Ok(logs_root.join(session_key))
    }

    pub fn ansi_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::session_dir_key(logs_root, session_key)?.join("redacted.ansi.log"))
    }

    /// 마지막으로 UI가 확정한 터미널 grid 크기. ANSI 로그의 zsh/ZLE redraw는 당시
    /// 열 수에 의존하므로 재시작 복원도 같은 크기에서 먼저 파싱해야 한다.
    pub fn terminal_size_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::session_dir_key(logs_root, session_key)?.join("terminal.size"))
    }

    pub fn load_terminal_size(
        logs_root: &Path,
        session_key: &str,
    ) -> anyhow::Result<Option<(u16, u16)>> {
        let path = Self::terminal_size_path(logs_root, session_key)?;
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("터미널 크기 읽기 실패: {}", path.display()));
            }
        };
        let mut fields = raw.split_whitespace();
        let cols: u16 = fields
            .next()
            .context("터미널 열 수 없음")?
            .parse()
            .context("터미널 열 수 파싱 실패")?;
        let rows: u16 = fields
            .next()
            .context("터미널 행 수 없음")?
            .parse()
            .context("터미널 행 수 파싱 실패")?;
        anyhow::ensure!(fields.next().is_none(), "터미널 크기 필드가 너무 많음");
        // 손상된 sidecar가 재시작 시 과도한 terminal grid 할당으로 이어지지 않게
        // UI/runtime이 허용하는 범위와 같은 상한을 둔다.
        anyhow::ensure!((1..=500).contains(&cols), "터미널 열 수 범위 초과");
        anyhow::ensure!((1..=500).contains(&rows), "터미널 행 수 범위 초과");
        Ok(Some((cols, rows)))
    }

    pub fn save_terminal_size(
        logs_root: &Path,
        session_key: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<()> {
        anyhow::ensure!((1..=500).contains(&cols), "터미널 열 수 범위 초과");
        anyhow::ensure!((1..=500).contains(&rows), "터미널 행 수 범위 초과");
        let dir = Self::session_dir_key(logs_root, session_key)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("로그 디렉터리 생성 실패: {}", dir.display()))?;
        let path = dir.join("terminal.size");
        std::fs::write(&path, format!("{cols} {rows}\n"))
            .with_context(|| format!("터미널 크기 기록 실패: {}", path.display()))
    }

    /// append 재개 시 offset을 기존 파일 길이부터 이어가기 위한 길이 조회.
    pub fn ansi_len(&self) -> std::io::Result<u64> {
        self.ansi.metadata().map(|metadata| metadata.len())
    }

    /// redaction이 끝난 출력 chunk를 기록한다.
    /// ansi.log에는 그대로, plain.txt에는 ANSI escape 제거본을 쓴다.
    pub fn append_output(&mut self, redacted: &[u8]) -> anyhow::Result<()> {
        if redacted.is_empty() {
            return Ok(());
        }
        self.ansi
            .write_all(redacted)
            .context("ansi.log 기록 실패")?;
        let plain = strip_ansi_stateful(redacted, &mut self.strip_state);
        self.plain
            .write_all(&plain)
            .context("plain.txt 기록 실패")?;
        Ok(())
    }

    /// 세션 이벤트 한 줄 (jsonl). detail은 이미 redaction된 값만 받는다.
    pub fn append_event(&mut self, kind: &str, detail: Option<&str>) -> anyhow::Result<()> {
        let ts = deppy_core::time::unix_ms();
        let line = match detail {
            Some(detail) => format!(
                "{{\"ts_ms\":{ts},\"event\":\"{}\",\"detail\":\"{}\"}}\n",
                json_escape(kind),
                json_escape(detail)
            ),
            None => format!("{{\"ts_ms\":{ts},\"event\":\"{}\"}}\n", json_escape(kind)),
        };
        self.events
            .write_all(line.as_bytes())
            .context("events.jsonl 기록 실패")?;
        self.events.flush().ok();
        Ok(())
    }

    /// 종료/주기 flush.
    pub fn flush(&mut self) {
        self.ansi.flush().ok();
        self.plain.flush().ok();
        self.events.flush().ok();
    }
}

fn append_only(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("로그 파일 열기 실패: {}", path.display()))
}

/// 표시용 plain 텍스트 변환기 단계 — escape가 chunk 경계에 걸려도 이어간다.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum StripPhase {
    #[default]
    Ground,
    /// ESC 수신 직후 (다음 바이트로 종류 결정)
    Esc,
    /// CSI 본문 (최종 바이트 0x40..=0x7e까지)
    Csi,
    /// OSC 본문 (BEL 또는 ESC\ 까지)
    Osc,
    /// OSC 안에서 ESC 수신 (\이면 종료)
    OscEsc,
}

/// plain 변환기 상태 — 단계 + CSI 파라미터 + 현재 열.
#[derive(Debug, Clone, Default)]
pub(crate) struct StripState {
    phase: StripPhase,
    /// CSI 파라미터 바이트(0x30..=0x3f) 수집 — 커서 이동 폭 계산에 쓴다.
    params: Vec<u8>,
    /// 현재 출력 열(0-based). 커서 이동을 공백으로 되돌릴 때 기준.
    col: usize,
}

/// CSI 파라미터의 첫 숫자(기본값 1).
fn first_param(params: &[u8], default: usize) -> usize {
    let text = std::str::from_utf8(params).unwrap_or("");
    let head = text.split(';').next().unwrap_or("");
    head.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// CSI/OSC/2바이트 escape를 제거하고 CR을 열 리셋으로 처리한다. 상태는 호출 간 유지된다.
///
/// **커서 가로 이동은 공백으로 되돌린다** (2026-07-17): claude/codex는 정렬에 공백이
/// 아니라 커서 이동을 쓴다 — `auto ESC[11G mode ESC[16G on`. escape를 통째로 버리면
/// `automodeon`이 되어 plain.txt를 사람이 읽을 수 없다(실측). CHA(`G`)는 목표 열까지,
/// CUF(`C`)는 n칸을 공백으로 채운다.
///
/// 열 계산은 바이트가 아니라 문자 단위다(UTF-8 연속 바이트는 세지 않는다). 전각(한글 등)을
/// 1칸으로 세므로 정렬이 완벽하진 않지만, 단어가 붙어버리는 것보다 훨씬 읽을 만하다 —
/// 이 파일은 사람이 읽는 로그이지 화면 재현이 아니다(화면 재현은 ansi.log 몫).
fn strip_ansi_stateful(buffer: &[u8], state: &mut StripState) -> Vec<u8> {
    let mut out = Vec::with_capacity(buffer.len());
    for &byte in buffer {
        match state.phase {
            StripPhase::Ground => match byte {
                0x1b => state.phase = StripPhase::Esc,
                b'\r' => state.col = 0,
                b'\n' => {
                    state.col = 0;
                    out.push(byte);
                }
                byte => {
                    // UTF-8 연속 바이트(10xxxxxx)는 같은 문자의 일부 — 열을 늘리지 않는다.
                    if byte & 0xc0 != 0x80 {
                        state.col += 1;
                    }
                    out.push(byte);
                }
            },
            StripPhase::Esc => match byte {
                b'[' => {
                    state.phase = StripPhase::Csi;
                    state.params.clear();
                }
                b']' => state.phase = StripPhase::Osc,
                // 2바이트 escape (ESC =, ESC > 등) — 이 바이트로 종료
                _ => state.phase = StripPhase::Ground,
            },
            StripPhase::Csi => {
                if (0x30..=0x3f).contains(&byte) {
                    // 파라미터는 몇 바이트 안 되지만, 깨진 스트림이 무한히 쌓이지 않게 상한.
                    if state.params.len() < 32 {
                        state.params.push(byte);
                    }
                } else if (0x40..=0x7e).contains(&byte) {
                    let pad = match byte {
                        // CHA — 절대 열로 이동(1-based). 뒤로 가는 이동은 공백으로 표현할 수
                        // 없으니 무시한다(덮어쓰기 재그리기라 어차피 내용이 이어진다).
                        b'G' => first_param(&state.params, 1).saturating_sub(1 + state.col),
                        // CUF — 상대 전진.
                        b'C' => first_param(&state.params, 1),
                        _ => 0,
                    };
                    out.resize(out.len() + pad, b' ');
                    state.col += pad;
                    state.phase = StripPhase::Ground;
                }
            }
            StripPhase::Osc => match byte {
                0x07 => state.phase = StripPhase::Ground,
                0x1b => state.phase = StripPhase::OscEsc,
                _ => {}
            },
            StripPhase::OscEsc => {
                // ESC\ 종결. 그 외 바이트는 OSC 본문 계속으로 취급
                state.phase = if byte == b'\\' {
                    StripPhase::Ground
                } else {
                    StripPhase::Osc
                };
            }
        }
    }
    out
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-logs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn escape가_chunk_경계에_걸려도_plain은_깨끗() {
        let root = temp_root("split-esc");
        let session = SessionId(9);
        let mut writer = SessionLogWriter::open(&root, session).unwrap();
        // CSI가 두 chunk에 걸침: "\x1b[3" + "1mRED\x1b[0m ok"
        writer.append_output(b"pre \x1b[3").unwrap();
        writer.append_output(b"1mRED\x1b[0m ok").unwrap();
        // OSC가 걸침: "\x1b]0;ti" + "tle\x07after"
        writer.append_output(b" \x1b]0;ti").unwrap();
        writer.append_output(b"tle\x07after").unwrap();
        writer.flush();
        let plain = std::fs::read_to_string(
            SessionLogWriter::session_dir(&root, session).join("redacted.plain.txt"),
        )
        .unwrap();
        assert_eq!(plain, "pre RED ok after");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 세_파일_생성과_append_only() {
        let root = temp_root("basic");
        let session = SessionId(7);
        {
            let mut writer = SessionLogWriter::open(&root, session).unwrap();
            writer
                .append_output(b"line1 \x1b[31mred\x1b[0m\r\n")
                .unwrap();
            writer.append_event("spawned", None).unwrap();
            writer.flush();
        }
        // 재오픈 후 추가 기록 — 기존 내용 보존 (append-only)
        {
            let mut writer = SessionLogWriter::open(&root, session).unwrap();
            writer.append_output(b"line2\n").unwrap();
            writer.append_event("exited", Some("exit code 0")).unwrap();
            writer.flush();
        }
        let dir = SessionLogWriter::session_dir(&root, session);
        let ansi = std::fs::read(dir.join("redacted.ansi.log")).unwrap();
        assert!(ansi.windows(5).any(|w| w == b"line1"));
        assert!(ansi.windows(5).any(|w| w == b"\x1b[31m")); // ansi.log는 escape 보존
        assert!(ansi.windows(5).any(|w| w == b"line2"));

        let plain = std::fs::read_to_string(dir.join("redacted.plain.txt")).unwrap();
        assert_eq!(plain, "line1 red\nline2\n"); // escape/CR 제거

        let events = std::fs::read_to_string(dir.join("events.redacted.jsonl")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"spawned\""));
        assert!(lines[1].contains("exit code 0"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 영속_session_key는_ansi를_이어쓰고_경로탈출을_거부한다() {
        let root = temp_root("persistent-key");
        let key = "019f3804-586d-7ca3-9386-1cbc8710ca08";
        {
            let mut writer = SessionLogWriter::open_key(&root, key).unwrap();
            writer.append_output(b"\x1b[36mkept\x1b[0m").unwrap();
            writer.flush();
        }
        assert_eq!(
            std::fs::read(SessionLogWriter::ansi_path(&root, key).unwrap()).unwrap(),
            b"\x1b[36mkept\x1b[0m"
        );
        for invalid in ["", ".", "..", "../escape", "nested/session", "/tmp/escape"] {
            assert!(SessionLogWriter::session_dir_key(&root, invalid).is_err());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 터미널_크기_sidecar를_저장하고_검증한다() {
        let root = temp_root("terminal-size");
        let key = "019f3804-586d-7ca3-9386-1cbc8710ca08";

        assert_eq!(
            SessionLogWriter::load_terminal_size(&root, key).unwrap(),
            None
        );
        SessionLogWriter::save_terminal_size(&root, key, 121, 47).unwrap();
        assert_eq!(
            SessionLogWriter::load_terminal_size(&root, key).unwrap(),
            Some((121, 47))
        );

        std::fs::write(
            SessionLogWriter::terminal_size_path(&root, key).unwrap(),
            "65535 47\n",
        )
        .unwrap();
        assert!(SessionLogWriter::load_terminal_size(&root, key).is_err());
        assert!(SessionLogWriter::save_terminal_size(&root, key, 0, 47).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
    /// 2026-07-17 실측 회귀: claude는 정렬에 공백이 아니라 커서 이동을 쓴다
    /// (`auto ESC[11G mode ESC[16G on`). escape를 통째로 버리던 때는 plain.txt에
    /// `automodeon`으로 남아 사람이 읽을 수 없었다 — 벨 인박스 미리보기가 이 파일을
    /// 읽으면서 드러났다.
    #[test]
    fn strip_ansi_커서이동을_공백으로_되돌린다() {
        let mut state = StripState::default();
        // 실제 로그에서 뜬 바이트열 그대로.
        let input = b"auto\x1b[11Gmode\x1b[16Gon";
        let out = strip_ansi_stateful(input, &mut state);
        // auto(0..4) → 11열(0-based 10)까지 공백 6 → mode(10..14) → 16열(15)까지 공백 1 → on
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "auto      mode on",
            "커서가 가리킨 열까지 공백으로 채워 단어가 붙지 않아야 한다"
        );
    }

    #[test]
    fn strip_ansi_cuf는_상대_전진만큼_띄운다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"a\x1b[3Cb", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "a   b");
    }

    /// 뒤로 가는 이동(이미 지난 열)은 공백으로 표현할 수 없다 — 무시하고 내용만 잇는다.
    #[test]
    fn strip_ansi_뒤로가는_cha는_공백을_넣지_않는다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"abcdef\x1b[2Gx", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "abcdefx");
    }

    /// 열 추적은 문자 단위 — UTF-8 연속 바이트를 세면 한글 뒤 정렬이 어긋난다.
    #[test]
    fn strip_ansi_한글_뒤_열계산은_문자수_기준() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful("가나\x1b[5Gx".as_bytes(), &mut state);
        // '가나' = 2문자(col 2) → 5열(0-based 4)까지 공백 2개
        assert_eq!(String::from_utf8(out).unwrap(), "가나  x");
    }

    /// 줄바꿈은 열을 리셋한다 — 안 하면 다음 줄의 CHA가 통째로 무시된다.
    #[test]
    fn strip_ansi_개행_후_열이_리셋된다() {
        let mut state = StripState::default();
        let out = strip_ansi_stateful(b"abcdef\n\x1b[4Gx", &mut state);
        assert_eq!(String::from_utf8(out).unwrap(), "abcdef\n   x");
    }

    /// escape가 chunk 경계에 걸려도 열/파라미터 상태가 이어진다(스트리밍 계약).
    #[test]
    fn strip_ansi_chunk_경계에_걸친_커서이동도_복원된다() {
        let mut state = StripState::default();
        let mut out = strip_ansi_stateful(b"auto\x1b[1", &mut state);
        out.extend(strip_ansi_stateful(b"1Gmode", &mut state));
        assert_eq!(String::from_utf8(out).unwrap(), "auto      mode");
    }
}
