//! 홈 「AI 공지」 제목 자동 번역 (2026-07-18 사용자) — LLM이 연결돼 있으면
//! 앱 로케일 언어로 번역해 보여준다. 이 사용자 환경의 "LLM 연결"은 API 키가
//! 아니라 구독 인증된 `claude` CLI다 — `claude -p --model haiku` 일회성 호출로
//! 제목 배치를 번역한다. GUI 앱 PATH에는 CLI가 없으므로 git_cli 관례대로
//! 절대경로 후보를 직접 찾는다. CLI가 없거나 실패하면 원문(영어) 그대로 표시.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// 번역 subprocess 상한 — haiku 배치 번역은 수 초, 행이면 죽인다.
const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(60);

/// `claude` CLI 절대경로 후보 (GUI 앱은 로그인 셸 PATH를 못 믿는다 — git_cli 관례).
pub fn claude_bin() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = crate::paths::home_dir() {
        candidates.push(home.join(".local/bin/claude"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));
    candidates.into_iter().find(|path| path.is_file())
}

/// 로케일 → 번역 대상 언어. None = 번역 불필요(원문이 이미 영어).
pub fn language_for_locale(locale: &str) -> Option<&'static str> {
    match locale.split('-').next().unwrap_or(locale) {
        "ko" => Some("한국어"),
        "ja" => Some("日本語"),
        "zh" => Some(if locale == "zh-Hant" {
            "繁體中文"
        } else {
            "简体中文"
        }),
        _ => None,
    }
}

/// 번역 프롬프트 — JSON 배열만 응답하도록 강하게 고정한다(파싱 안정성).
fn build_prompt(titles: &[String], language: &str) -> String {
    format!(
        "다음 AI 서비스 장애 공지 제목들을 {language}로 번역하세요. \
         설명 없이 번역된 문자열들의 JSON 배열만 출력하세요(코드펜스 금지, 순서 유지).\n{}",
        serde_json::to_string(titles).unwrap_or_default()
    )
}

/// CLI 응답 → 번역 목록. 코드펜스로 감싸는 습성을 방어하고, 길이가 어긋나면
/// 통째로 버린다(어긋난 매핑으로 잘못 표기하느니 원문 유지).
fn parse_response(raw: &str, expected_len: usize) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_end_matches("```"))
        .unwrap_or(trimmed)
        .trim();
    let translated: Vec<String> = serde_json::from_str(body).ok()?;
    (translated.len() == expected_len).then_some(translated)
}

/// 제목 배치를 백그라운드에서 번역한다. 결과는 (원문, 번역) 쌍 목록 — 실패 시
/// 빈 목록(채널 닫힘)으로 끝난다. 호출측은 한 번에 하나만 띄운다(App이 rx 보유로 게이트).
pub fn spawn_translate(
    titles: Vec<String>,
    language: String,
    egui_ctx: egui::Context,
) -> Receiver<Vec<(String, String)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("notice-translate".into())
        .spawn(move || {
            let Some(bin) = claude_bin() else {
                return; // CLI 없음 — 채널 닫힘으로 종결(원문 유지)
            };
            let prompt = build_prompt(&titles, &language);
            match run_claude(&bin, &prompt) {
                Ok(raw) => {
                    if let Some(translated) = parse_response(&raw, titles.len()) {
                        let pairs = titles.into_iter().zip(translated).collect();
                        let _ = tx.send(pairs);
                        egui_ctx.request_repaint();
                    } else {
                        tracing::debug!("공지 번역 응답 파싱 실패 — 원문 유지");
                    }
                }
                Err(e) => tracing::debug!("공지 번역 실패: {e:#}"),
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("notice-translate 워커 spawn 실패: {e}");
    }
    rx
}

/// `claude -p` 실행 — git_cli::run_git과 같은 데드라인·파이프 드레인 방어.
fn run_claude(bin: &std::path::Path, prompt: &str) -> anyhow::Result<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg("haiku")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            let _ = out_tx.send(buf);
        });
    }
    let deadline = Instant::now() + TRANSLATE_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => break,
            Some(status) => anyhow::bail!("claude -p 비정상 종료: {status}"),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("claude -p가 {TRANSLATE_TIMEOUT:?} 안에 끝나지 않아 중단");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let stdout = out_rx.recv().unwrap_or_default();
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_for_locale은_영어권이면_none이다() {
        assert_eq!(language_for_locale("ko-KR"), Some("한국어"));
        assert_eq!(language_for_locale("ja-JP"), Some("日本語"));
        assert_eq!(language_for_locale("zh-Hans"), Some("简体中文"));
        assert_eq!(language_for_locale("zh-Hant"), Some("繁體中文"));
        assert_eq!(language_for_locale("en-US"), None);
    }

    #[test]
    fn parse_response는_코드펜스와_길이_불일치를_방어한다() {
        assert_eq!(
            parse_response(r#"["가", "나"]"#, 2),
            Some(vec!["가".to_owned(), "나".to_owned()])
        );
        assert_eq!(
            parse_response("```json\n[\"가\"]\n```", 1),
            Some(vec!["가".to_owned()])
        );
        // 길이가 어긋나면 잘못된 매핑 대신 통째로 버린다.
        assert_eq!(parse_response(r#"["가"]"#, 2), None);
        assert_eq!(parse_response("번역 결과입니다: 가, 나", 2), None);
    }

    #[test]
    fn build_prompt는_제목을_json으로_싣는다() {
        let prompt = build_prompt(&["A \"quoted\" title".to_owned()], "한국어");
        assert!(prompt.contains("한국어"));
        assert!(prompt.contains(r#"["A \"quoted\" title"]"#));
    }
}
