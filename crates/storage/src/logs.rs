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
            strip_state: StripState::Ground,
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
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
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

/// 표시용 plain 텍스트 변환기 상태 — escape가 chunk 경계에 걸려도 이어간다.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StripState {
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

/// CSI/OSC/2바이트 escape와 CR을 제거한다. 상태는 호출 간 유지된다.
fn strip_ansi_stateful(buffer: &[u8], state: &mut StripState) -> Vec<u8> {
    let mut out = Vec::with_capacity(buffer.len());
    for &byte in buffer {
        match *state {
            StripState::Ground => match byte {
                0x1b => *state = StripState::Esc,
                b'\r' => {}
                byte => out.push(byte),
            },
            StripState::Esc => match byte {
                b'[' => *state = StripState::Csi,
                b']' => *state = StripState::Osc,
                // 2바이트 escape (ESC =, ESC > 등) — 이 바이트로 종료
                _ => *state = StripState::Ground,
            },
            StripState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    *state = StripState::Ground;
                }
            }
            StripState::Osc => match byte {
                0x07 => *state = StripState::Ground,
                0x1b => *state = StripState::OscEsc,
                _ => {}
            },
            StripState::OscEsc => {
                // ESC\ 종결. 그 외 바이트는 OSC 본문 계속으로 취급
                *state = if byte == b'\\' {
                    StripState::Ground
                } else {
                    StripState::Osc
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
}
