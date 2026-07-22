//! 홈 「AI 공지」 제목 자동 번역 (2026-07-18 사용자) — LLM이 연결돼 있으면
//! 앱 로케일 언어로 번역해 보여준다. 이 사용자 환경의 "LLM 연결"은 API 키가
//! 아니라 구독 인증된 `claude` CLI다 — `claude -p --model haiku` 일회성 호출로
//! 제목 배치를 번역한다. GUI 앱 PATH에는 CLI가 없으므로 git_cli 관례대로
//! 절대경로 후보를 직접 찾는다. CLI가 없거나 실패하면 원문(영어) 그대로 표시.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// 번역 subprocess 상한 — haiku 배치 번역은 수 초, 행이면 죽인다.
const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(60);
const CACHE_VERSION: u32 = 1;
const CACHE_MAX_ITEMS: usize = 256;
const CACHE_MAX_RETAINED_BYTES: usize = 512 * 1024;
const CACHE_FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
const CACHE_ENTRY_OVERHEAD_BYTES: usize = 128;
const CACHE_PROVIDER_MAX_BYTES: usize = 64;
const CACHE_LOCALE_MAX_BYTES: usize = 32;
const CACHE_TITLE_MAX_BYTES: usize = 4 * 1024;
const CACHE_TRANSLATION_MAX_BYTES: usize = 4 * 1024;
const TRANSLATE_BATCH_MAX_ITEMS: usize = 32;
const TRANSLATE_INPUT_MAX_BYTES: usize = 128 * 1024;
const TRANSLATE_PROMPT_MAX_BYTES: usize = TRANSLATE_INPUT_MAX_BYTES * 6 + 4 * 1024;
const TRANSLATE_LANGUAGE_MAX_BYTES: usize = 128;
const TRANSLATE_OUTPUT_MAX_BYTES: usize = 256 * 1024;

/// 제공자·언어·원문을 함께 묶는다. 같은 제목이어도 제공자나 표시 언어가 다르면
/// 별도 번역으로 취급해 앱 실행 중 로케일 변경과 provider 충돌을 안전하게 처리한다.
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TranslationCacheKey {
    provider: String,
    locale: String,
    title: String,
}

impl TranslationCacheKey {
    pub fn new(provider: &str, locale: &str, title: &str) -> Self {
        let locale = cache_locale(locale);
        if !valid_text(provider, CACHE_PROVIDER_MAX_BYTES, false)
            || !valid_text(locale, CACHE_LOCALE_MAX_BYTES, false)
            || !valid_text(title, CACHE_TITLE_MAX_BYTES, false)
        {
            return Self {
                provider: String::new(),
                locale: String::new(),
                title: String::new(),
            };
        }
        Self {
            provider: provider.to_owned(),
            locale: locale.to_owned(),
            title: title.to_owned(),
        }
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn is_valid(&self) -> bool {
        valid_text(&self.provider, CACHE_PROVIDER_MAX_BYTES, false)
            && valid_text(&self.locale, CACHE_LOCALE_MAX_BYTES, false)
            && valid_text(&self.title, CACHE_TITLE_MAX_BYTES, false)
    }

    fn retained_bytes(&self) -> usize {
        CACHE_ENTRY_OVERHEAD_BYTES
            .saturating_add(self.provider.len())
            .saturating_add(self.locale.len())
            .saturating_add(self.title.len())
    }
}

impl fmt::Debug for TranslationCacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationCacheKey")
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TranslationCacheFile {
    version: u32,
    entries: Vec<TranslationCacheEntry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TranslationCacheEntry {
    provider: String,
    locale: String,
    title: String,
    translation: String,
}

/// 홈 공지 번역의 영속 캐시. 런타임 조회는 HashMap, 디스크 포맷은 사람이 확인할 수
/// 있는 JSON 배열로 분리한다. 최근 256건/512 KiB만 유지해 RSS와 캐시 파일을 함께
/// 제한한다.
#[derive(Default)]
pub struct TranslationCache {
    entries: HashMap<TranslationCacheKey, String>,
    order: VecDeque<TranslationCacheKey>,
    retained_bytes: usize,
}

impl fmt::Debug for TranslationCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationCache")
            .field("items", &self.entries.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

impl TranslationCache {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take((CACHE_FILE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= CACHE_FILE_MAX_BYTES,
            "공지 번역 캐시 파일이 크기 상한을 초과했습니다"
        );
        let file: TranslationCacheFile = serde_json::from_slice(&bytes)?;
        if file.version != CACHE_VERSION {
            anyhow::bail!(
                "지원하지 않는 공지 번역 캐시 버전: {} (현재 {CACHE_VERSION})",
                file.version
            );
        }
        let mut cache = Self::default();
        for entry in file.entries {
            cache.insert(
                TranslationCacheKey {
                    provider: entry.provider,
                    locale: entry.locale,
                    title: entry.title,
                },
                entry.translation,
            );
        }
        Ok(cache)
    }

    pub fn get(&self, provider: &str, locale: &str, title: &str) -> Option<&str> {
        if !valid_text(provider, CACHE_PROVIDER_MAX_BYTES, false)
            || !valid_text(locale, CACHE_LOCALE_MAX_BYTES, false)
            || !valid_text(title, CACHE_TITLE_MAX_BYTES, false)
        {
            return None;
        }
        self.entries
            .get(&TranslationCacheKey::new(provider, locale, title))
            .map(String::as_str)
    }

    pub fn contains_key(&self, key: &TranslationCacheKey) -> bool {
        key.is_valid() && self.entries.contains_key(key)
    }

    pub fn extend(&mut self, translations: Vec<(TranslationCacheKey, String)>) {
        for (key, translation) in translations {
            self.insert(key, translation);
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entries: Vec<_> = self
            .order
            .iter()
            .filter_map(|key| {
                self.entries
                    .get(key)
                    .map(|translation| TranslationCacheEntry {
                        provider: key.provider.clone(),
                        locale: key.locale.clone(),
                        title: key.title.clone(),
                        translation: translation.clone(),
                    })
            })
            .collect();
        let file = TranslationCacheFile {
            version: CACHE_VERSION,
            entries,
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        anyhow::ensure!(
            bytes.len() <= CACHE_FILE_MAX_BYTES,
            "공지 번역 캐시 직렬화가 크기 상한을 초과했습니다"
        );
        deppy_core::fs::atomic_write(path, &bytes)?;
        Ok(())
    }

    fn insert(&mut self, key: TranslationCacheKey, translation: String) {
        if !key.is_valid() || !valid_text(&translation, CACHE_TRANSLATION_MAX_BYTES, false) {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(key.retained_bytes().saturating_add(previous.len()));
            if let Some(position) = self.order.iter().position(|candidate| candidate == &key) {
                self.order.remove(position);
            }
        }
        self.retained_bytes = self
            .retained_bytes
            .saturating_add(key.retained_bytes().saturating_add(translation.len()));
        self.entries.insert(key.clone(), translation);
        self.order.push_back(key);
        while self.entries.len() > CACHE_MAX_ITEMS || self.retained_bytes > CACHE_MAX_RETAINED_BYTES
        {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.entries.remove(&evicted) {
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_sub(evicted.retained_bytes().saturating_add(value.len()));
            }
        }
    }
}

fn valid_text(value: &str, max_bytes: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.trim().is_empty())
        && value.len() <= max_bytes
        && !value.as_bytes().contains(&0)
}

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

/// 동일 표시 언어의 지역 변형은 같은 번역을 공유한다. 중국어는 현재 번역 프롬프트의
/// 간체/번체 구분과 같은 기준을 써서 서로 섞이지 않게 한다.
fn cache_locale(locale: &str) -> &str {
    match locale.split('-').next().unwrap_or(locale) {
        "ko" => "ko",
        "ja" => "ja",
        "zh" if locale == "zh-Hant" => "zh-Hant",
        "zh" => "zh-Hans",
        _ => locale,
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
    (translated.len() == expected_len
        && translated
            .iter()
            .all(|value| valid_text(value, CACHE_TRANSLATION_MAX_BYTES, false)))
    .then_some(translated)
}

/// 제목 배치를 백그라운드에서 번역한다. 결과는 (영속 캐시 키, 번역) 쌍 목록 —
/// 실패 시 빈 목록(채널 닫힘)으로 끝난다. 호출측은 한 번에 하나만 띄운다.
pub fn spawn_translate(
    bin: PathBuf,
    keys: Vec<TranslationCacheKey>,
    language: String,
    egui_ctx: egui::Context,
) -> Receiver<Vec<(TranslationCacheKey, String)>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let request_bytes = keys.iter().try_fold(language.len(), |total, key| {
        total.checked_add(key.retained_bytes())
    });
    if keys.is_empty()
        || keys.len() > TRANSLATE_BATCH_MAX_ITEMS
        || request_bytes.is_none_or(|bytes| bytes > TRANSLATE_INPUT_MAX_BYTES)
        || !valid_text(&language, TRANSLATE_LANGUAGE_MAX_BYTES, false)
        || keys.iter().any(|key| !key.is_valid())
    {
        return rx;
    }
    let spawned = std::thread::Builder::new()
        .name("notice-translate".into())
        .spawn(move || {
            let titles: Vec<String> = keys.iter().map(|key| key.title().to_owned()).collect();
            let prompt = build_prompt(&titles, &language);
            if prompt.len() > TRANSLATE_PROMPT_MAX_BYTES {
                return;
            }
            match run_claude(&bin, &prompt) {
                Ok(raw) => {
                    if let Some(translated) = parse_response(&raw, titles.len()) {
                        let pairs = keys.into_iter().zip(translated).collect();
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
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("claude stdout pipe missing");
    };
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel(1);
    let reader = match std::thread::Builder::new()
        .name("notice-translate-stdout".to_owned())
        .spawn(move || {
            let captured = read_bytes_limited(stdout, TRANSLATE_OUTPUT_MAX_BYTES);
            let _ = out_tx.send(captured);
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let deadline = Instant::now() + TRANSLATE_TIMEOUT;
    let completion = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(anyhow::anyhow!("claude -p 비정상 종료: {status}")),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(anyhow::anyhow!(
                    "claude -p가 {TRANSLATE_TIMEOUT:?} 안에 끝나지 않아 중단"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.into());
            }
        }
    };
    let captured = out_rx
        .recv()
        .unwrap_or_else(|_| Err(std::io::Error::other("stdout reader disconnected")));
    let _ = reader.join();
    completion?;
    let captured = captured?;
    anyhow::ensure!(!captured.truncated, "claude stdout 크기 상한 초과");
    String::from_utf8(captured.bytes).map_err(Into::into)
}

struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bytes_limited(mut reader: impl Read, max_bytes: usize) -> std::io::Result<BoundedBytes> {
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    reader
        .by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok(BoundedBytes { bytes, truncated })
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
        let oversized =
            serde_json::to_string(&vec!["x".repeat(CACHE_TRANSLATION_MAX_BYTES + 1)]).unwrap();
        assert_eq!(parse_response(&oversized, 1), None);
        assert_eq!(parse_response(r#"["contains\u0000nul"]"#, 1), None);
    }

    #[test]
    fn build_prompt는_제목을_json으로_싣는다() {
        let prompt = build_prompt(&["A \"quoted\" title".to_owned()], "한국어");
        assert!(prompt.contains("한국어"));
        assert!(prompt.contains(r#"["A \"quoted\" title"]"#));
    }

    #[test]
    fn cache는_제공자와_언어별로_분리한다() {
        let mut cache = TranslationCache::default();
        cache.extend(vec![
            (
                TranslationCacheKey::new("Claude", "ko-KR", "Incident"),
                "장애".to_owned(),
            ),
            (
                TranslationCacheKey::new("OpenAI", "ja-JP", "Incident"),
                "障害".to_owned(),
            ),
        ]);

        assert_eq!(cache.get("Claude", "ko", "Incident"), Some("장애"));
        assert_eq!(cache.get("OpenAI", "ja", "Incident"), Some("障害"));
        assert_eq!(cache.get("OpenAI", "ko-KR", "Incident"), None);
    }

    #[test]
    fn cache는_원자저장후_재시작처럼_다시_읽힌다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-notice-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notice_translations.json");
        let mut cache = TranslationCache::default();
        cache.extend(vec![(
            TranslationCacheKey::new("Claude", "zh-Hant", "Incident"),
            "事件".to_owned(),
        )]);
        cache.save(&path).unwrap();

        let restored = TranslationCache::load(&path).unwrap();
        assert_eq!(restored.get("Claude", "zh-Hant", "Incident"), Some("事件"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache는_rotation뒤에도_item과_byte_cap을_유지한다() {
        let mut cache = TranslationCache::default();
        for index in 0..2_000 {
            cache.extend(vec![(
                TranslationCacheKey::new("OpenAI", "ko-KR", &format!("Incident {index}")),
                format!("번역 {index}"),
            )]);
        }

        assert!(cache.entries.len() <= CACHE_MAX_ITEMS);
        assert_eq!(cache.order.len(), cache.entries.len());
        assert!(cache.retained_bytes <= CACHE_MAX_RETAINED_BYTES);
        assert_eq!(cache.get("OpenAI", "ko-KR", "Incident 0"), None);
        assert_eq!(
            cache.get("OpenAI", "ko-KR", "Incident 1999"),
            Some("번역 1999")
        );
    }

    #[test]
    fn invalid_cache_payload는_fail_closed이고_debug는_redacted다() {
        let mut cache = TranslationCache::default();
        let secret_title = "token-like-title";
        let key = TranslationCacheKey::new("OpenAI", "ko-KR", secret_title);
        let debug = format!("{key:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(secret_title));

        cache.extend(vec![(key, "정상 번역".to_owned())]);
        cache.extend(vec![(
            TranslationCacheKey::new("OpenAI", "ko-KR", &"x".repeat(CACHE_TITLE_MAX_BYTES + 1)),
            "oversized key".to_owned(),
        )]);
        cache.extend(vec![(
            TranslationCacheKey::new("OpenAI", "ko-KR", "oversized translation"),
            "x".repeat(CACHE_TRANSLATION_MAX_BYTES + 1),
        )]);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.get("OpenAI", "ko-KR", secret_title),
            Some("정상 번역")
        );
    }

    #[test]
    fn bounded_stdout_reader는_exact_cap과_plus_one을_구분한다() {
        let exact = read_bytes_limited(
            std::io::Cursor::new(vec![b'a'; TRANSLATE_OUTPUT_MAX_BYTES]),
            TRANSLATE_OUTPUT_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(exact.bytes.len(), TRANSLATE_OUTPUT_MAX_BYTES);
        assert!(!exact.truncated);

        let over = read_bytes_limited(
            std::io::Cursor::new(vec![b'b'; TRANSLATE_OUTPUT_MAX_BYTES + 1]),
            TRANSLATE_OUTPUT_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(over.bytes.len(), TRANSLATE_OUTPUT_MAX_BYTES);
        assert!(over.truncated);
    }

    #[test]
    fn invalid_translation_batch는_worker를_만들지_않고_disconnect한다() {
        let receiver = spawn_translate(
            PathBuf::from("must-not-run"),
            Vec::new(),
            "한국어".to_owned(),
            egui::Context::default(),
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn oversized_cache_file은_deserialize전에_거부한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-notice-cache-oversized-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notice_translations.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((CACHE_FILE_MAX_BYTES + 1) as u64).unwrap();
        assert!(TranslationCache::load(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_source에는_unbounded_cache_or_stdout_read가_없다() {
        let source = include_str!("notice_translate.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(!production.contains("std::sync::mpsc::channel()"));
        assert!(!production.contains("read_to_end(&mut buf)"));
        assert!(production.contains("CACHE_MAX_ITEMS"));
        assert!(production.contains("CACHE_MAX_RETAINED_BYTES"));
        assert!(production.contains("TRANSLATE_OUTPUT_MAX_BYTES"));
    }
}
