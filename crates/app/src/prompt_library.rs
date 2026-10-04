//! 프롬프트 라이브러리 — 저장·재사용 가능한 에이전트 지시(파라미터 포함).
//!
//! Warp Drive의 Prompts에 대응하는 호스트 레벨 기능(기능2). 자주 쓰는 지시("이 PR
//! 리뷰해", "X에 테스트 추가")를 `{{param}}` 파라미터와 함께 저장해 두고, 팔레트에서
//! 골라 파라미터만 채워 활성 세션에 주입한다.
//!
//! 이 모듈은 **순수 데이터 + 로직**이다(egui·PTY를 모른다). 후속 PR에서 leaf 팔레트가
//! 이 데이터로 UI를 그리고, `app.rs`가 치환 결과를 composer/WriteInput 경로로 주입한다
//! — 기존 leaf+intent+host I/O 경계와 동일하다.

use std::collections::BTreeMap;
use std::hash::Hasher;
use std::io::{Read, Write};
use std::path::Path;

pub const PROMPT_LIBRARY_FILE_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const PROMPT_LIBRARY_CONTENT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PROMPT_LIBRARY_MAX_ITEMS: usize = 1024;
pub const PROMPT_BODY_MAX_BYTES: usize = 1024 * 1024;
pub const PROMPT_TITLE_MAX_BYTES: usize = 4096;
const PROMPT_ID_MAX_BYTES: usize = 256;
pub const PROMPT_TAG_MAX_BYTES: usize = 256;
pub const PROMPT_MAX_TAGS: usize = 32;
pub const PROMPT_QUERY_MAX_BYTES: usize = 256;
pub const PROMPT_PARAM_MAX_NAMES: usize = 128;
pub const PROMPT_PARAM_NAME_MAX_BYTES: usize = 256;
pub const PROMPT_PARAM_NAMES_MAX_BYTES: usize = 16 * 1024;
pub const PROMPT_PARAM_VALUE_MAX_BYTES: usize = 8 * 1024;
pub const PROMPT_PARAM_VALUES_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptLibraryError {
    ReadFailed,
    Corrupt,
    LimitExceeded,
    WriteFailed,
    RecoveryRequired,
    WorkerUnavailable,
    StaleRevision,
    Conflict,
}

impl std::fmt::Display for PromptLibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PromptLibraryError {}

#[derive(Debug, PartialEq)]
pub enum PromptLibraryLoad {
    Missing,
    Loaded(PromptLibrary),
    Failed(PromptLibraryError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptLibraryFileVersion {
    Missing,
    Present { bytes: usize, fingerprint: u64 },
}

pub struct PromptLibraryStartup {
    pub library: PromptLibrary,
    pub seed_missing: bool,
    pub file_version: PromptLibraryFileVersion,
    /// Automatic writes remain disabled until the original file is recovered and reloaded.
    pub error: Option<PromptLibraryError>,
}

/// 저장된 프롬프트 하나. 파라미터는 별도 필드가 아니라 `body`의 `{{name}}`에서
/// 파생한다(단일 진실원). `tags`는 검색·분류용.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prompt {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 사용자 프롬프트 모음. `config_dir/prompt_library.json`에 직렬화된다.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptLibrary {
    #[serde(default)]
    pub prompts: Vec<Prompt>,
}

impl PromptLibrary {
    /// Read a bounded regular file through one handle. Missing is the only seeding case;
    /// parse/read/limit errors never authorize writes to the original.
    #[cfg(test)]
    pub fn load(path: &Path) -> PromptLibraryLoad {
        Self::load_with_version(path).0
    }

    fn load_with_version(path: &Path) -> (PromptLibraryLoad, PromptLibraryFileVersion) {
        let bytes = match read_library_file(path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return (
                    PromptLibraryLoad::Missing,
                    PromptLibraryFileVersion::Missing,
                );
            }
            Err(error) => {
                return (
                    PromptLibraryLoad::Failed(error),
                    PromptLibraryFileVersion::Missing,
                );
            }
        };
        let version = file_version(&bytes);
        let library: Self = match serde_json::from_slice(&bytes) {
            Ok(library) => library,
            Err(_) => {
                return (
                    PromptLibraryLoad::Failed(PromptLibraryError::Corrupt),
                    version,
                );
            }
        };
        let loaded = match library.validate() {
            Ok(()) => PromptLibraryLoad::Loaded(library),
            Err(error) => PromptLibraryLoad::Failed(error),
        };
        (loaded, version)
    }

    pub fn load_startup(path: &Path) -> PromptLibraryStartup {
        let (loaded, file_version) = Self::load_with_version(path);
        match loaded {
            PromptLibraryLoad::Missing => PromptLibraryStartup {
                library: Self::default_seed(),
                seed_missing: true,
                file_version,
                error: None,
            },
            PromptLibraryLoad::Loaded(library) => PromptLibraryStartup {
                library,
                seed_missing: false,
                file_version,
                error: None,
            },
            PromptLibraryLoad::Failed(error) => PromptLibraryStartup {
                library: Self::default(),
                seed_missing: false,
                file_version,
                error: Some(error),
            },
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.prompts.iter().map(prompt_bytes).sum()
    }

    pub fn validate(&self) -> Result<(), PromptLibraryError> {
        if self.prompts.len() > PROMPT_LIBRARY_MAX_ITEMS {
            return Err(PromptLibraryError::LimitExceeded);
        }
        let mut bytes = 0usize;
        for prompt in &self.prompts {
            validate_prompt(prompt)?;
            bytes = bytes.saturating_add(prompt_bytes(prompt));
            if bytes > PROMPT_LIBRARY_CONTENT_MAX_BYTES {
                return Err(PromptLibraryError::LimitExceeded);
            }
        }
        Ok(())
    }

    /// Validate before mutating, so a refused edit preserves both memory and disk state.
    pub fn validate_upsert(&self, prompt: &Prompt) -> Result<(), PromptLibraryError> {
        validate_prompt(prompt)?;
        let existing = self.get(&prompt.id);
        if existing.is_none() && self.prompts.len() >= PROMPT_LIBRARY_MAX_ITEMS {
            return Err(PromptLibraryError::LimitExceeded);
        }
        let bytes = self
            .retained_bytes()
            .saturating_sub(existing.map_or(0, prompt_bytes))
            .saturating_add(prompt_bytes(prompt));
        if bytes > PROMPT_LIBRARY_CONTENT_MAX_BYTES {
            return Err(PromptLibraryError::LimitExceeded);
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn try_upsert(&mut self, prompt: Prompt) -> Result<(), PromptLibraryError> {
        self.validate_upsert(&prompt)?;
        self.upsert(prompt);
        Ok(())
    }

    /// Bounded serialization goes directly to an exclusively-created sibling file. Failure
    /// removes only that temporary file; rename is the sole replacement of the original.
    #[cfg(test)]
    pub fn save(&self, path: &Path) -> Result<(), PromptLibraryError> {
        self.save_inner(path, None).map(|_| ())
    }

    pub(crate) fn save_checked(
        &self,
        path: &Path,
        expected: PromptLibraryFileVersion,
    ) -> Result<PromptLibraryFileVersion, PromptLibraryError> {
        self.save_inner(path, Some(expected))
    }

    fn save_inner(
        &self,
        path: &Path,
        expected: Option<PromptLibraryFileVersion>,
    ) -> Result<PromptLibraryFileVersion, PromptLibraryError> {
        self.validate()?;
        if let Some(expected) = expected {
            check_file_version(path, expected)?;
        }
        let tmp = path.with_file_name(format!(".prompt-library-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&tmp)
            .map_err(|_| PromptLibraryError::WriteFailed)?;
        let result = (|| {
            let mut writer = LimitedWriter {
                file: std::io::BufWriter::with_capacity(64 * 1024, file),
                written: 0,
                exceeded: false,
                hash: std::collections::hash_map::DefaultHasher::new(),
            };
            if serde_json::to_writer_pretty(&mut writer, self).is_err() {
                return Err(if writer.exceeded {
                    PromptLibraryError::LimitExceeded
                } else {
                    PromptLibraryError::WriteFailed
                });
            }
            writer
                .file
                .flush()
                .map_err(|_| PromptLibraryError::WriteFailed)?;
            writer
                .file
                .get_ref()
                .sync_all()
                .map_err(|_| PromptLibraryError::WriteFailed)?;
            let version = PromptLibraryFileVersion::Present {
                bytes: writer.written,
                fingerprint: writer.hash.finish(),
            };
            drop(writer);
            if let Some(expected) = expected {
                check_file_version(path, expected)?;
            }
            commit_temp(&tmp, path, expected)?;
            Ok(version)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// 제목·본문·태그에 대해 대소문자 무시 부분일치로 검색한다. 빈 쿼리는 전체를
    /// 저장 순서대로 돌려준다.
    #[cfg(test)]
    pub fn search(&self, query: &str) -> Vec<&Prompt> {
        let q = query.trim().to_lowercase();
        self.prompts
            .iter()
            .filter(|p| {
                q.is_empty()
                    || p.title.to_lowercase().contains(&q)
                    || p.body.to_lowercase().contains(&q)
                    || p.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Prompt> {
        self.prompts.iter().find(|p| p.id == id)
    }

    /// 새 프롬프트에 부여할 라이브러리 내 유일한 id를 title 슬러그로 만든다. 슬러그가
    /// 비면(예: 한글 전용 제목) `prompt`를 쓰고, 충돌 시 `-2`, `-3`…을 붙인다. id는
    /// 내부 식별자일 뿐 화면에는 title이 나온다.
    pub fn fresh_id(&self, title: &str) -> String {
        let base = slugify(title);
        let mut base = if base.is_empty() {
            "prompt".to_owned()
        } else {
            base
        };
        // Slugs are ASCII. Leave room for every collision suffix inside the ID byte budget.
        if base.len() > PROMPT_ID_MAX_BYTES {
            base = base[..PROMPT_ID_MAX_BYTES].to_owned();
        }
        if self.get(&base).is_none() {
            return base;
        }
        let mut n = 2;
        loop {
            let suffix = format!("-{n}");
            let prefix = &base[..base.len().min(PROMPT_ID_MAX_BYTES - suffix.len())];
            let candidate = format!("{prefix}{suffix}");
            if self.get(&candidate).is_none() {
                return candidate;
            }
            n += 1;
        }
    }

    /// id가 같은 프롬프트가 있으면 교체(편집), 없으면 추가(신규)한다.
    pub fn upsert(&mut self, prompt: Prompt) {
        match self.prompts.iter_mut().find(|p| p.id == prompt.id) {
            Some(existing) => *existing = prompt,
            None => self.prompts.push(prompt),
        }
    }

    /// id로 프롬프트를 삭제한다(없으면 무시).
    pub fn delete(&mut self, id: &str) {
        self.prompts.retain(|p| p.id != id);
    }

    /// 첫 실행(저장 파일 없음)에서 팔레트가 비지 않도록 채우는 예시 프롬프트들.
    pub fn default_seed() -> Self {
        let p = |id: &str, title: &str, body: &str, tags: &[&str]| Prompt {
            id: id.into(),
            title: title.into(),
            body: body.into(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
        };
        Self {
            prompts: vec![
                p(
                    "review-branch",
                    "브랜치 리뷰",
                    "이 브랜치의 변경을 리뷰해줘. 특히 {{focus}}를 중점으로 보고 버그·엣지케이스·테스트 누락을 짚어줘.",
                    &["review", "git"],
                ),
                p(
                    "add-tests",
                    "테스트 추가",
                    "{{module}}에 대한 단위 테스트를 추가해줘. 경계값과 실패 경로를 포함하고 기존 테스트 스타일을 따라줘.",
                    &["test"],
                ),
                p(
                    "explain-error",
                    "에러 설명",
                    "방금 출력된 에러의 원인을 설명하고 최소 수정안을 제안해줘.",
                    &["debug"],
                ),
                p(
                    "commit-msg",
                    "커밋 메시지",
                    "지금 staged 변경에 대한 한국어 커밋 메시지를 conventional commits 형식으로 써줘.",
                    &["git"],
                ),
            ],
        }
    }
}

/// Bounded per-palette result indices; no borrowed prompt references or duplicate bodies.
#[derive(Default)]
pub struct PromptSearchCache {
    revision: Option<u64>,
    query: String,
    indices: Vec<usize>,
    #[cfg(test)]
    scanned_prompts: usize,
}
impl PromptSearchCache {
    pub fn update(
        &mut self,
        library: &PromptLibrary,
        revision: u64,
        query: &str,
    ) -> Result<&[usize], PromptLibraryError> {
        if query.len() > PROMPT_QUERY_MAX_BYTES || library.prompts.len() > PROMPT_LIBRARY_MAX_ITEMS
        {
            return Err(PromptLibraryError::LimitExceeded);
        }
        if self.revision == Some(revision) && self.query == query {
            return Ok(&self.indices);
        }
        self.indices.clear();
        let normalized = query.trim().to_lowercase();
        for (index, prompt) in library.prompts.iter().enumerate() {
            #[cfg(test)]
            {
                self.scanned_prompts = self.scanned_prompts.saturating_add(1);
            }
            if normalized.is_empty()
                || prompt.title.to_lowercase().contains(&normalized)
                || prompt.body.to_lowercase().contains(&normalized)
                || prompt
                    .tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&normalized))
            {
                self.indices.push(index);
            }
        }
        self.query.clear();
        self.query.push_str(query);
        self.revision = Some(revision);
        Ok(&self.indices)
    }
    #[cfg(test)]
    pub fn scanned_prompts(&self) -> usize {
        self.scanned_prompts
    }
}

fn commit_temp(
    tmp: &Path,
    path: &Path,
    expected: Option<PromptLibraryFileVersion>,
) -> Result<(), PromptLibraryError> {
    if expected == Some(PromptLibraryFileVersion::Missing) {
        // Atomic no-clobber seed: a real library may arrive between the final check and commit.
        std::fs::hard_link(tmp, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                PromptLibraryError::Conflict
            } else {
                PromptLibraryError::WriteFailed
            }
        })?;
        let _ = std::fs::remove_file(tmp);
    } else {
        std::fs::rename(tmp, path).map_err(|_| PromptLibraryError::WriteFailed)?;
    }
    Ok(())
}

fn open_library_file(path: &Path) -> Result<Option<std::fs::File>, PromptLibraryError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(PromptLibraryError::ReadFailed),
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(PromptLibraryError::ReadFailed);
    }
    Ok(Some(file))
}

fn read_library_file(path: &Path) -> Result<Option<Vec<u8>>, PromptLibraryError> {
    let Some(file) = open_library_file(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take((PROMPT_LIBRARY_FILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| PromptLibraryError::ReadFailed)?;
    if bytes.len() > PROMPT_LIBRARY_FILE_MAX_BYTES {
        return Err(PromptLibraryError::LimitExceeded);
    }
    Ok(Some(bytes))
}

fn file_version(bytes: &[u8]) -> PromptLibraryFileVersion {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    hash.write(bytes);
    PromptLibraryFileVersion::Present {
        bytes: bytes.len(),
        fingerprint: hash.finish(),
    }
}

/// Verification shares the bounded regular-file opener but never allocates the entire file.
/// The hasher is incremental, identical to startup and the streaming writer.
fn read_file_version(path: &Path) -> Result<PromptLibraryFileVersion, PromptLibraryError> {
    let Some(file) = open_library_file(path)? else {
        return Ok(PromptLibraryFileVersion::Missing);
    };
    let mut reader = file.take((PROMPT_LIBRARY_FILE_MAX_BYTES + 1) as u64);
    let mut scratch = [0u8; 64 * 1024];
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut bytes = 0usize;
    loop {
        let count = reader
            .read(&mut scratch)
            .map_err(|_| PromptLibraryError::ReadFailed)?;
        if count == 0 {
            break;
        }
        bytes += count;
        if bytes > PROMPT_LIBRARY_FILE_MAX_BYTES {
            return Err(PromptLibraryError::LimitExceeded);
        }
        hash.write(&scratch[..count]);
    }
    Ok(PromptLibraryFileVersion::Present {
        bytes,
        fingerprint: hash.finish(),
    })
}

fn check_file_version(
    path: &Path,
    expected: PromptLibraryFileVersion,
) -> Result<(), PromptLibraryError> {
    let current = read_file_version(path)?;
    if current == expected {
        Ok(())
    } else {
        Err(PromptLibraryError::Conflict)
    }
}

fn prompt_bytes(prompt: &Prompt) -> usize {
    prompt
        .id
        .len()
        .saturating_add(prompt.title.len())
        .saturating_add(prompt.body.len())
        .saturating_add(prompt.tags.iter().map(String::len).sum::<usize>())
}

fn validate_prompt(prompt: &Prompt) -> Result<(), PromptLibraryError> {
    if prompt.body.len() > PROMPT_BODY_MAX_BYTES
        || prompt.title.len() > PROMPT_TITLE_MAX_BYTES
        || prompt.id.len() > PROMPT_ID_MAX_BYTES
        || prompt.tags.len() > PROMPT_MAX_TAGS
        || prompt
            .tags
            .iter()
            .any(|tag| tag.len() > PROMPT_TAG_MAX_BYTES)
    {
        return Err(PromptLibraryError::LimitExceeded);
    }
    Ok(())
}

struct LimitedWriter {
    file: std::io::BufWriter<std::fs::File>,
    written: usize,
    exceeded: bool,
    hash: std::collections::hash_map::DefaultHasher,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > PROMPT_LIBRARY_FILE_MAX_BYTES.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(std::io::Error::other("prompt library size limit"));
        }
        let written = self.file.write(bytes)?;
        self.written += written;
        self.hash.write(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `{{...}}` 스캔이 내놓는 조각 하나. param_names와 render가 이 이터레이터 하나를
/// 공유해서 find("{{") → find("}}") → trim → is_param_name 브레이스 파싱 로직이 두
/// 함수에서 따로 갈라지는 걸 막는다(PR-1 리뷰 M2: 한쪽만 고치면 파라미터 판정이
/// 조용히 어긋날 수 있었음).
enum Segment<'a> {
    /// 일반 텍스트, 또는 파라미터가 아닌 것으로 판정된 `{{...}}` 원문(중괄호 포함
    /// 그대로) — render는 이걸 그대로 출력에 이어붙이면 된다.
    Literal(&'a str),
    /// 유효한 파라미터로 판정된 `{{ name }}` 블록의 트림된 이름.
    Param(&'a str),
}

/// `body`를 앞에서부터 스캔해 Literal/Param 조각을 등장 순서대로 내놓는다. `{{`를 찾고
/// 짝이 되는 `}}`를 찾아 안쪽을 트림한 뒤 `is_param_name`으로 파라미터 여부를 판정한다
/// — 판정 로직이 이 한 곳에만 있어서 param_names와 render가 절대 어긋나지 않는다.
fn scan_braces(body: &str) -> impl Iterator<Item = Segment<'_>> {
    struct Scanner<'a> {
        rest: &'a str,
    }

    impl<'a> Iterator for Scanner<'a> {
        type Item = Segment<'a>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.rest.is_empty() {
                return None;
            }
            let Some(open) = self.rest.find("{{") else {
                // 더 이상 "{{" 없음 — 남은 전부 리터럴
                let lit = self.rest;
                self.rest = "";
                return Some(Segment::Literal(lit));
            };
            if open > 0 {
                // "{{" 앞의 일반 텍스트를 먼저 리터럴로 내놓는다
                let lit = &self.rest[..open];
                self.rest = &self.rest[open..];
                return Some(Segment::Literal(lit));
            }
            let after = &self.rest[2..];
            let Some(close) = after.find("}}") else {
                // 닫힘 없음 — 남은 전부 리터럴
                let lit = self.rest;
                self.rest = "";
                return Some(Segment::Literal(lit));
            };
            let raw = &self.rest[..2 + close + 2];
            let name = after[..close].trim();
            self.rest = &after[close + 2..];
            if is_param_name(name) {
                Some(Segment::Param(name))
            } else {
                // 파라미터 아님 — `{{...}}` 원문을 리터럴로 보존
                Some(Segment::Literal(raw))
            }
        }
    }

    Scanner { rest: body }
}

/// `body`의 `{{name}}` 파라미터 이름을 등장 순서로, 중복 없이 뽑는다. name은
/// `[A-Za-z0-9_]+`(양옆 공백 허용: `{{ name }}`)만 파라미터로 본다 — 그 밖의 `{{...}}`는
/// 프롬프트 본문의 리터럴로 두고 무시한다(예: JSON 예시 안의 중괄호).
#[cfg(test)]
pub fn param_names(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in scan_braces(body) {
        if let Segment::Param(name) = seg
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// `body`의 `{{name}}`을 `values`로 치환한다. 값이 없는 파라미터는 빈 문자열로 치환한다
/// (UI가 제출 전 전부 채우게 강제하므로 정상 경로에선 발생하지 않는다). 파라미터가
/// 아닌 `{{...}}`(리터럴 중괄호)는 원문 그대로 남긴다.
#[cfg(test)]
pub fn render(body: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    for seg in scan_braces(body) {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Param(name) => {
                out.push_str(values.get(name).map(String::as_str).unwrap_or(""));
            }
        }
    }
    out
}

/// UI preparation is bounded independently of the saved body size. Existing templates are
/// preserved on disk even when their expanded input cannot fit the submission budget.
pub fn param_names_bounded(body: &str) -> Result<Vec<String>, PromptLibraryError> {
    let mut names = Vec::new();
    let mut bytes = 0usize;
    for segment in scan_braces(body) {
        if let Segment::Param(name) = segment {
            if name.len() > PROMPT_PARAM_NAME_MAX_BYTES {
                return Err(PromptLibraryError::LimitExceeded);
            }
            if names.iter().any(|existing| existing == name) {
                continue;
            }
            bytes += name.len();
            if names.len() >= PROMPT_PARAM_MAX_NAMES || bytes > PROMPT_PARAM_NAMES_MAX_BYTES {
                return Err(PromptLibraryError::LimitExceeded);
            }
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

pub fn render_bounded(
    body: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, PromptLibraryError> {
    if body.len() > PROMPT_BODY_MAX_BYTES
        || values.len() > PROMPT_PARAM_MAX_NAMES
        || values.iter().any(|(name, value)| {
            name.len() > PROMPT_PARAM_NAME_MAX_BYTES || value.len() > PROMPT_PARAM_VALUE_MAX_BYTES
        })
        || values.values().map(String::len).sum::<usize>() > PROMPT_PARAM_VALUES_MAX_BYTES
    {
        return Err(PromptLibraryError::LimitExceeded);
    }
    let piece = |segment| match segment {
        Segment::Literal(text) => text,
        Segment::Param(name) => values.get(name).map(String::as_str).unwrap_or(""),
    };
    // Compute the exact bounded size before allocating; repeated parameters cannot multiply
    // a small values map into an unbounded allocation.
    let mut bytes = 0usize;
    for segment in scan_braces(body) {
        bytes = bytes.saturating_add(piece(segment).len());
        if bytes > PROMPT_BODY_MAX_BYTES {
            return Err(PromptLibraryError::LimitExceeded);
        }
    }
    let mut rendered = String::with_capacity(bytes);
    for segment in scan_braces(body) {
        rendered.push_str(piece(segment));
    }
    Ok(rendered)
}

fn is_param_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// title → id 슬러그. ASCII 영숫자는 소문자로 남기고 나머지는 `-`로 접은 뒤 앞뒤·중복
/// `-`를 정리한다. 비-ASCII만 있는 제목은 빈 슬러그가 되며 호출부가 fallback을 쓴다.
fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

/// 편집 폼의 태그 입력(공백·쉼표 구분)을 정규화한다. 트림·빈값 제거·중복 제거하고
/// 입력 순서를 보존한다.
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in input.split([',', ' ', '\t', '\n']) {
        let tag = tag.trim();
        // 대소문자 무시 중복제거(검색이 대소문자 무시라 "Git"/"git"이 같은 태그로
        // 매칭됨 — 저장도 같게 취급해 근접 중복을 막는다). 첫 등장 표기를 보존한다.
        if !tag.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            out.push(tag.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr4_search_cache_does_not_rescan_unchanged_query_or_revision() {
        let library = PromptLibrary::default_seed();
        let mut cache = PromptSearchCache::default();
        let first = cache.update(&library, 1, "리뷰").unwrap().to_vec();
        let work = cache.scanned_prompts();
        for _ in 0..10 {
            assert_eq!(cache.update(&library, 1, "리뷰").unwrap(), first);
        }
        assert_eq!(
            cache.scanned_prompts(),
            work,
            "unchanged frames must not rescan body text"
        );
    }

    #[test]
    fn pr4_search_cache_matches_unicode_and_invalidates_edits_deletes() {
        let mut library = PromptLibrary {
            prompts: vec![Prompt {
                id: "a".into(),
                title: "İΣ 한글".into(),
                body: "Review {{repo}} ΟΣ".into(),
                tags: vec!["Git".into()],
            }],
        };
        let mut cache = PromptSearchCache::default();
        for query in ["", "σ", "ος", "i̇", "한글", "GIT", "missing"] {
            let expected: Vec<usize> = library
                .search(query)
                .into_iter()
                .map(|p| library.prompts.iter().position(|x| x.id == p.id).unwrap())
                .collect();
            assert_eq!(cache.update(&library, 1, query).unwrap(), expected);
        }
        assert_eq!(cache.update(&library, 1, "Review").unwrap(), &[0]);
        library.prompts[0].body = "changed".into();
        assert!(cache.update(&library, 2, "Review").unwrap().is_empty());
        library.prompts.clear();
        assert!(cache.update(&library, 3, "").unwrap().is_empty());
        assert_eq!(
            cache.update(&library, 3, &"한".repeat(86)),
            Err(PromptLibraryError::LimitExceeded)
        );
    }

    #[test]
    fn pr4_bounded_parameters_reject_names_and_expansion_before_growth() {
        assert_eq!(
            param_names_bounded(&format!("{{{{{}}}}}", "x".repeat(257))),
            Err(PromptLibraryError::LimitExceeded)
        );
        let excessive = (0..129)
            .map(|i| format!("{{{{p{i}}}}}"))
            .collect::<String>();
        assert_eq!(
            param_names_bounded(&excessive),
            Err(PromptLibraryError::LimitExceeded)
        );
        let wide = (0..65)
            .map(|i| format!("{{{{p{i:03}_{}}}}}", "x".repeat(251)))
            .collect::<String>();
        assert_eq!(
            param_names_bounded(&wide),
            Err(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(
            param_names_bounded("{{x}} {{x}} {{y}} {{ 아닌 }}").unwrap(),
            ["x", "y"]
        );
        let mut params = BTreeMap::from([("x".into(), "가".repeat(2730))]);
        let body = "{{x}}".repeat(129);
        assert_eq!(
            render_bounded(&body, &params),
            Err(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(render_bounded("{{x}}", &params).unwrap(), params["x"]);
        params.insert("x".into(), "가".repeat(2731));
        assert_eq!(
            render_bounded("{{x}}", &params),
            Err(PromptLibraryError::LimitExceeded)
        );
        let many = (0..9)
            .map(|i| (format!("p{i}"), "x".repeat(8192)))
            .collect();
        assert_eq!(
            render_bounded("{{p0}}", &many),
            Err(PromptLibraryError::LimitExceeded)
        );
    }

    #[test]
    fn pr4_streaming_version_matches_startup_and_checked_save_across_chunks() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        let mut library = PromptLibrary::default_seed();
        library.prompts[0].body = "한글 body".repeat(25_000);
        library.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let version = read_file_version(&path).unwrap();
        assert_eq!(version, file_version(&bytes));
        library.prompts[0].body.push_str(" changed");
        let saved = library.save_checked(&path, version).unwrap();
        assert_eq!(read_file_version(&path).unwrap(), saved);
        assert_eq!(saved, PromptLibrary::load_startup(&path).file_version);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr4_streaming_version_rejects_over_limit_nonregular_and_conflicts() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        assert_eq!(
            read_file_version(&path).unwrap(),
            PromptLibraryFileVersion::Missing
        );
        assert_eq!(read_file_version(&dir), Err(PromptLibraryError::ReadFailed));
        std::fs::File::create(&path)
            .unwrap()
            .set_len((PROMPT_LIBRARY_FILE_MAX_BYTES + 1) as u64)
            .unwrap();
        assert_eq!(
            read_file_version(&path),
            Err(PromptLibraryError::LimitExceeded)
        );
        std::fs::write(&path, "external").unwrap();
        assert_eq!(
            check_file_version(&path, PromptLibraryFileVersion::Missing),
            Err(PromptLibraryError::Conflict)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("deppy-library-pr3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn pr3_startup_preserves_valid_empty_library() {
        let dir = unique_temp_dir();
        let path = dir.join("prompt_library.json");
        let original = br#"{"prompts":[]}"#;
        std::fs::write(&path, original).unwrap();
        let startup = PromptLibrary::load_startup(&path);
        assert!(
            startup.library.prompts.is_empty(),
            "deleting all prompts must survive startup"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_startup_preserves_corrupt_original_bytes() {
        let dir = unique_temp_dir();
        let path = dir.join("prompt_library.json");
        let original = b"{ broken but recoverable prompt content";
        std::fs::write(&path, original).unwrap();
        let _ = PromptLibrary::load_startup(&path);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "startup must not overwrite recovery data"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_startup_only_seeds_missing_and_does_not_write_on_load() {
        let dir = unique_temp_dir();
        let path = dir.join("missing.json");
        let startup = PromptLibrary::load_startup(&path);
        assert!(startup.seed_missing);
        assert!(startup.error.is_none());
        assert_eq!(startup.library, PromptLibrary::default_seed());
        assert!(!path.exists(), "seed persistence belongs to the worker");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_bounded_load_rejects_large_files_bodies_and_item_counts() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((PROMPT_LIBRARY_FILE_MAX_BYTES + 1) as u64)
            .unwrap();
        let startup = PromptLibrary::load_startup(&path);
        assert_eq!(startup.error, Some(PromptLibraryError::LimitExceeded));
        assert!(!startup.seed_missing);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (PROMPT_LIBRARY_FILE_MAX_BYTES + 1) as u64
        );
        let oversized = PromptLibrary {
            prompts: vec![Prompt {
                id: "large".into(),
                title: "large".into(),
                body: "가".repeat(PROMPT_BODY_MAX_BYTES / 3 + 1),
                tags: vec![],
            }],
        };
        let bytes = serde_json::to_vec(&oversized).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Failed(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let too_many = PromptLibrary {
            prompts: (0..=PROMPT_LIBRARY_MAX_ITEMS)
                .map(|i| Prompt {
                    id: i.to_string(),
                    title: "T".into(),
                    body: "B".into(),
                    tags: vec![],
                })
                .collect(),
        };
        std::fs::write(&path, serde_json::to_vec(&too_many).unwrap()).unwrap();
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Failed(PromptLibraryError::LimitExceeded)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_read_failure_does_not_seed_and_symlinks_are_not_followed() {
        let dir = unique_temp_dir();
        let startup = PromptLibrary::load_startup(&dir);
        assert_eq!(startup.error, Some(PromptLibraryError::ReadFailed));
        assert!(!startup.seed_missing);
        assert!(startup.library.prompts.is_empty());
        #[cfg(unix)]
        {
            let original = dir.join("original.json");
            let link = dir.join("link.json");
            std::fs::write(&original, b"{}").unwrap();
            std::os::unix::fs::symlink(&original, &link).unwrap();
            assert_eq!(
                PromptLibrary::load(&link),
                PromptLibraryLoad::Failed(PromptLibraryError::ReadFailed)
            );
            assert_eq!(std::fs::read(&original).unwrap(), b"{}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_refused_edit_preserves_previous_library() {
        let mut library = PromptLibrary::default_seed();
        let before = library.clone();
        let prompt = Prompt {
            id: "review-branch".into(),
            title: "T".into(),
            body: "x".repeat(PROMPT_BODY_MAX_BYTES + 1),
            tags: vec![],
        };
        assert_eq!(
            library.try_upsert(prompt),
            Err(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(library, before);
    }

    #[test]
    fn pr3_aggregate_content_budget_rejects_edit_without_changing_previous_data() {
        let mut library = PromptLibrary {
            prompts: (0..7)
                .map(|id| Prompt {
                    id: id.to_string(),
                    title: "T".into(),
                    body: "x".repeat(PROMPT_BODY_MAX_BYTES),
                    tags: vec![],
                })
                .collect(),
        };
        let remaining =
            PROMPT_LIBRARY_CONTENT_MAX_BYTES - library.retained_bytes() - "last".len() - "T".len();
        library
            .try_upsert(Prompt {
                id: "last".into(),
                title: "T".into(),
                body: "x".repeat(remaining),
                tags: vec![],
            })
            .unwrap();
        assert_eq!(library.retained_bytes(), PROMPT_LIBRARY_CONTENT_MAX_BYTES);
        let before = library.clone();
        assert_eq!(
            library.try_upsert(Prompt {
                id: "one more".into(),
                title: "T".into(),
                body: "x".into(),
                tags: vec![]
            }),
            Err(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(library, before);
    }

    #[test]
    fn pr3_seed_and_existing_file_conflicts_preserve_external_data() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        let missing = PromptLibrary::load_startup(&path);
        let external = b"{\"prompts\":[]}";
        std::fs::write(&path, external).unwrap();
        assert_eq!(
            missing.library.save_checked(&path, missing.file_version),
            Err(PromptLibraryError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), external);
        let loaded = PromptLibrary::load_startup(&path);
        let external_new = b"{\"prompts\":[]}\n";
        std::fs::write(&path, external_new).unwrap();
        assert_eq!(
            PromptLibrary::default_seed().save_checked(&path, loaded.file_version),
            Err(PromptLibraryError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), external_new);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_seed_commit_is_no_clobber_even_after_final_missing_check() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        let tmp = dir.join("seed.tmp");
        std::fs::write(&tmp, b"seed").unwrap();
        check_file_version(&path, PromptLibraryFileVersion::Missing).unwrap();
        // Deterministic competing editor creation after the final read/check.
        std::fs::write(&path, b"external original").unwrap();
        assert_eq!(
            commit_temp(&tmp, &path, Some(PromptLibraryFileVersion::Missing)),
            Err(PromptLibraryError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"external original");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_serialized_limit_failure_preserves_original_and_removes_temp() {
        let dir = unique_temp_dir();
        let path = dir.join("library.json");
        let original = b"{\"prompts\":[]}";
        std::fs::write(&path, original).unwrap();
        let oversized_json = PromptLibrary {
            prompts: (0..3)
                .map(|id| Prompt {
                    id: id.to_string(),
                    title: "T".into(),
                    body: "\u{1}".repeat(PROMPT_BODY_MAX_BYTES),
                    tags: vec![],
                })
                .collect(),
        };
        assert!(oversized_json.validate().is_ok());
        assert_eq!(
            oversized_json.save(&path),
            Err(PromptLibraryError::LimitExceeded)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn param_names_등장순서_중복제거() {
        assert_eq!(
            param_names("Review PR {{pr}} in {{repo}}, then {{ pr }} again"),
            vec!["pr", "repo"]
        );
        assert_eq!(param_names("no params here"), Vec::<String>::new());
    }

    #[test]
    fn param_names_비파라미터_중괄호_무시() {
        // JSON 예시 안의 중괄호나 공백 포함은 파라미터로 보지 않는다.
        assert_eq!(param_names(r#"emit {{ "a": 1 }} and {{x}}"#), vec!["x"]);
        assert_eq!(param_names("unclosed {{oops"), Vec::<String>::new());
    }

    #[test]
    fn render_치환과_누락_빈문자열() {
        assert_eq!(
            render("add tests for {{module}}", &vals(&[("module", "pty")])),
            "add tests for pty"
        );
        // 값 없는 파라미터 → 빈 문자열
        assert_eq!(render("hi {{name}}", &vals(&[])), "hi ");
    }

    #[test]
    fn render_반복_파라미터와_리터럴_보존() {
        assert_eq!(
            render(
                "{{x}} then {{x}}; keep {{ not param }}",
                &vals(&[("x", "A")])
            ),
            "A then A; keep {{ not param }}"
        );
    }

    #[test]
    fn search_대소문자무시_제목본문태그() {
        let lib = PromptLibrary {
            prompts: vec![
                Prompt {
                    id: "1".into(),
                    title: "Review PR".into(),
                    body: "review {{pr}}".into(),
                    tags: vec!["git".into()],
                },
                Prompt {
                    id: "2".into(),
                    title: "Add tests".into(),
                    body: "add tests for {{module}}".into(),
                    tags: vec![],
                },
            ],
        };
        assert_eq!(lib.search("review").len(), 1);
        assert_eq!(lib.search("TEST").len(), 1); // 제목 "Add tests"
        assert_eq!(lib.search("git").len(), 1); // 태그
        assert_eq!(lib.search("").len(), 2); // 빈 쿼리 = 전체
    }

    #[test]
    fn pr15_generated_id_fits_byte_budget_with_collision_suffixes() {
        let mut library = PromptLibrary { prompts: vec![] };
        let title = "A".repeat(PROMPT_TITLE_MAX_BYTES);
        for _ in 0..12 {
            let id = library.fresh_id(&title);
            assert!(
                id.len() <= PROMPT_ID_MAX_BYTES,
                "generated ID exceeds its contract"
            );
            let prompt = Prompt {
                id,
                title: title.clone(),
                body: "retained 한글".into(),
                tags: vec![],
            };
            assert!(library.validate_upsert(&prompt).is_ok());
            library.upsert(prompt);
        }
        assert_eq!(library.prompts.len(), 12);
    }

    #[test]
    fn fresh_id_슬러그화와_충돌회피() {
        let mut lib = PromptLibrary::default();
        assert_eq!(lib.fresh_id("Review PR"), "review-pr");
        // 한글 전용 제목 → 빈 슬러그 → fallback "prompt"
        assert_eq!(lib.fresh_id("브랜치 리뷰"), "prompt");
        lib.prompts.push(Prompt {
            id: "review-pr".into(),
            title: "x".into(),
            body: "b".into(),
            tags: vec![],
        });
        assert_eq!(lib.fresh_id("Review  PR!!"), "review-pr-2");
    }

    #[test]
    fn upsert_는_교체_또는_추가하고_delete는_제거() {
        let mut lib = PromptLibrary::default();
        let mk = |id: &str, title: &str| Prompt {
            id: id.into(),
            title: title.into(),
            body: "b".into(),
            tags: vec![],
        };
        lib.upsert(mk("a", "first"));
        lib.upsert(mk("b", "second"));
        assert_eq!(lib.prompts.len(), 2);
        // 같은 id → 교체(추가 아님)
        lib.upsert(mk("a", "first-edited"));
        assert_eq!(lib.prompts.len(), 2);
        assert_eq!(lib.get("a").unwrap().title, "first-edited");
        lib.delete("a");
        assert_eq!(lib.prompts.len(), 1);
        assert!(lib.get("a").is_none());
        lib.delete("nope"); // 없는 id는 무시
        assert_eq!(lib.prompts.len(), 1);
    }

    #[test]
    fn parse_tags_트림_빈값제거_중복제거_순서보존() {
        assert_eq!(
            parse_tags(" git, review  review,,test "),
            vec!["git", "review", "test"]
        );
        assert_eq!(parse_tags("   "), Vec::<String>::new());
    }

    #[test]
    fn parse_tags_대소문자_무시_중복제거_첫표기_보존() {
        // "Git"과 "git"은 같은 태그 — 첫 등장 표기("Git")를 남긴다(검색이 대소문자 무시).
        assert_eq!(parse_tags("Git git GIT review"), vec!["Git", "review"]);
    }

    #[test]
    fn render_닫히지_않은_중괄호는_원문_유지() {
        // param_names뿐 아니라 사용자 출력을 만드는 render도 unclosed를 안전 처리한다.
        assert_eq!(render("prefix {{oops", &vals(&[])), "prefix {{oops");
    }

    #[test]
    fn 중첩_빈_중괄호는_파라미터_아님_render_불변() {
        // {{{{x}}}}의 첫 쌍이 잡는 이름은 "{{x" → is_param_name 실패 → 통째로 리터럴.
        assert_eq!(param_names("{{{{x}}}}"), Vec::<String>::new());
        assert_eq!(render("{{{{x}}}}", &vals(&[("x", "V")])), "{{{{x}}}}");
        // 빈 이름({{}})도 파라미터 아님.
        assert_eq!(param_names("{{}}"), Vec::<String>::new());
        assert_eq!(render("{{}}", &vals(&[])), "{{}}");
    }

    #[test]
    fn load_corrupt_json_reports_failure_without_replacing_original() {
        let dir = unique_temp_dir();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, "{ not valid json ").unwrap();
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Failed(PromptLibraryError::Corrupt)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_save_왕복_그리고_없는_파일은_빈것() {
        let dir = unique_temp_dir();
        let path = dir.join("lib.json");
        let _ = std::fs::remove_file(&path);
        // 없는 파일 → 빈 라이브러리
        assert_eq!(PromptLibrary::load(&path), PromptLibraryLoad::Missing);

        let lib = PromptLibrary {
            prompts: vec![Prompt {
                id: "1".into(),
                title: "T".into(),
                body: "b {{p}}".into(),
                tags: vec!["x".into()],
            }],
        };
        lib.save(&path).unwrap();
        assert_eq!(PromptLibrary::load(&path), PromptLibraryLoad::Loaded(lib));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
