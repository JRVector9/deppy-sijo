//! 문서 파일(md·txt)의 유계 로드/저장 lane.
//!
//! 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md` §4·§6·§7.
//!
//! 이 모듈은 UI 타입에 의존하지 않는다 — App이 워커로 돌릴 수 있는 요청/결과 값
//! 타입만 노출한다(`dotenv_sync`, `agent_state_worker`와 같은 관례). 실제 스레드 배선은
//! App(`app.rs`)의 몫이고, 여기서는 순수 동기 함수(`load_document`/`save_document`)만
//! 제공한다.
//!
//! 경로·문서 내용은 로그에 남기지 않는다(§7). 실패 로그는 error code와 바이트 수까지만
//! 남긴다 — `tests::production_source_never_logs_path_or_content`가 회귀를 잡는다.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

/// 이 이상은 절대 열지 않는다(§6 Refuse 경계) — 메타데이터 확인만으로 차단하고 본문은
/// 아예 읽지 않는다. 프로브(+1바이트)는 이 상수를 넘는 레이스(읽는 도중 파일이 커짐)를
/// 잡기 위한 여유일 뿐, 이 상한 자체가 "무제한 할당 경로 없음"의 근거다.
pub const DOCUMENT_REFUSE_BYTES_MAX: u64 = 8 * 1024 * 1024; // 8 MiB

/// Full(편집 가능) 티어 상한(§6). egui `TextEdit`는 가상화가 없어 내용 전체를 갤리
/// 하나로 레이아웃한다 — 그래서 "편집 가능 상한"은 정책이 아니라 실측으로 정한다.
///
/// 2026-08-22 실측(`tests::bench_textedit_layout_*`, Apple Silicon, `--release`,
/// egui_kittest 헤드리스 900×700, 렌더 백엔드 없음, 한글 섞인 유사 마크다운 텍스트,
/// 크기별 별도 프로세스 — `ru_maxrss`는 고점이라 한 프로세스에서 이어 재면 부풀려진다):
///
/// | 크기 | 첫 레이아웃 | 재레이아웃(1글자 편집) | RSS 증가 |
/// |---|---|---|---|
/// | 1 MiB  | 29.4 ms  | 13.7 ms  | ~55 MB  |
/// | 4 MiB  | 129.7 ms | 64.1 ms  | ~196 MB |
/// | 10 MiB | 332.9 ms | 167.7 ms | ~474 MB |
///
/// `TextEdit`는 내용이 바뀌면 갤리를 다시 만든다(캐시가 내용 해시 기준이라 편집 = 캐시
/// 미스) — 그래서 "재레이아웃(키 입력 한 글자)" 비용이 "첫 레이아웃"의 절반 안팎으로
/// 여전히 크다. 10 MiB 문서는 한 글자 칠 때마다 168ms대 프레임 하나를 먹는다(60fps
/// 예산 16.6ms의 10배) — 입력이 뚜렷하게 밀린다. 4 MiB도 64ms로 체감 지연이 있다.
/// 1 MiB는 13.7ms로 60fps 예산 안에 든다.
///
/// 그래서 Full 상한은 1 MiB로 둔다 — 키 입력마다의 재레이아웃이 한 프레임 예산 안에
/// 들어오는 마지막 지점이다. RSS도 1 MiB에서 ~55MB(대부분 폰트 아틀라스·AccessKit
/// 트리 같은 하네스 고정비 — 파일 자체 10배가 아니다)로 8 MiB Refuse 상한까지의 여유가
/// 충분하다.
pub const DOCUMENT_FULL_BYTES_MAX: u64 = 1024 * 1024; // 1 MiB

/// 프로브 상한 — `DOCUMENT_REFUSE_BYTES_MAX`를 초과하는지 확인할 때 그 이상은 절대
/// 읽지 않는다(레이스 세이프티 넷 1바이트만 여유를 둔다).
const DOCUMENT_READ_PROBE_BYTES: u64 = DOCUMENT_REFUSE_BYTES_MAX + 1;

/// §6의 한계 티어. Full/ViewOnly/Refuse는 바이트 수, Binary는 UTF-8 여부로 갈린다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentLimitTier {
    /// 열기·편집·저장·preview 전부 가능.
    Full,
    /// 열린다. source 읽기 전용 + preview. 편집·저장은 잠긴다.
    ViewOnly,
    /// 열지 않는다 — 상한 초과.
    Refuse,
    /// 열지 않는다 — UTF-8이 아니다.
    Binary,
}

/// 실패를 UI 문구 없이 App에 전달하는 코드. 문구는 `crates/i18n`이 이 코드로 나중에
/// 붙인다(§7) — 이 모듈은 사람이 읽는 문자열을 만들지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentIoErrorCode {
    /// 대상 경로가 없다.
    NotFound,
    /// 파일이 아니다(디렉터리 등).
    FileTypeInvalid,
    /// 읽기 실패(권한 등).
    ReadFailed,
    /// 메타데이터 조회 실패.
    MetadataFailed,
    /// 읽는 도중 파일이 바뀌었다(길이 불일치) — 내용을 신뢰할 수 없어 거부한다.
    Changed,
    /// 대상 경로에 부모 디렉터리가 없다.
    ParentMissing,
    /// 임시 파일 생성 실패.
    TempCreateFailed,
    /// 임시 파일 쓰기 실패.
    TempWriteFailed,
    /// 임시 파일 flush 실패.
    TempSyncFailed,
    /// atomic replace(rename) 실패.
    ReplaceFailed,
    /// 저장할 내용이 상한을 넘는다.
    ContentTooLarge,
}

impl DocumentIoErrorCode {
    /// 로그·전송에 쓰는 안정적인 스네이크케이스 코드.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "document_not_found",
            Self::FileTypeInvalid => "document_file_type_invalid",
            Self::ReadFailed => "document_read_failed",
            Self::MetadataFailed => "document_metadata_failed",
            Self::Changed => "document_changed_during_read",
            Self::ParentMissing => "document_parent_missing",
            Self::TempCreateFailed => "document_temp_create_failed",
            Self::TempWriteFailed => "document_temp_write_failed",
            Self::TempSyncFailed => "document_temp_sync_failed",
            Self::ReplaceFailed => "document_replace_failed",
            Self::ContentTooLarge => "document_content_too_large",
        }
    }
}

/// 저장 시 외부 변경을 감지하는 데 쓰는 스냅샷 — mtime + len (+ unix면 device/inode).
/// 내용은 담지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentRevision {
    modified: std::time::SystemTime,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl DocumentRevision {
    fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, DocumentIoErrorCode> {
        let modified = metadata
            .modified()
            .map_err(|_| DocumentIoErrorCode::MetadataFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Ok(Self {
                modified,
                len: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                modified,
                len: metadata.len(),
            })
        }
    }
}

/// 문서 열기 요청. 워커 스레드로 넘길 값 타입 — 경로 외 상태를 갖지 않는다.
#[derive(Debug, Clone)]
pub struct DocumentLoadRequest {
    pub path: PathBuf,
}

/// 문서 열기 결과. "조용한 실패도 전면 차단도 아니다" — 상한을 넘겨도 ViewOnly로는
/// 연다(§6).
#[derive(Debug)]
pub enum DocumentLoadOutcome {
    /// Full 티어로 열렸다 — 편집·저장 가능.
    Loaded {
        source: String,
        revision: DocumentRevision,
    },
    /// ViewOnly 티어로 열렸다 — source 읽기 전용 + preview만.
    ViewOnly {
        source: String,
        revision: DocumentRevision,
        byte_len: u64,
    },
    /// Refuse 티어 — 상한 초과로 열지 않았다.
    Refused { byte_len: u64 },
    /// Binary 티어 — UTF-8이 아니어서 열지 않았다. 내용은 이미 버려졌다.
    Binary { byte_len: u64 },
    /// I/O 실패.
    Failed { code: DocumentIoErrorCode },
}

impl DocumentLoadOutcome {
    /// 테스트가 §6 티어 경계를 확인할 때 쓰는 분류. App은 `Loaded`/`ViewOnly` 등
    /// 각 변형이 담은 값(source·revision·byte_len)이 그대로 필요해 이 요약 대신
    /// 전체 매치를 쓴다 — 그래서 프로덕션 호출부가 없다(test-only).
    #[cfg(test)]
    pub fn tier(&self) -> Option<DocumentLimitTier> {
        match self {
            Self::Loaded { .. } => Some(DocumentLimitTier::Full),
            Self::ViewOnly { .. } => Some(DocumentLimitTier::ViewOnly),
            Self::Refused { .. } => Some(DocumentLimitTier::Refuse),
            Self::Binary { .. } => Some(DocumentLimitTier::Binary),
            Self::Failed { .. } => None,
        }
    }
}

enum RawRead {
    Content {
        bytes: Vec<u8>,
        revision: DocumentRevision,
    },
    TooLarge {
        byte_len: u64,
    },
}

/// 상한까지만 읽는다. 상한을 넘는 파일은 본문을 아예 읽지 않고 크기만 돌려준다.
fn read_document_bounded(path: &Path) -> Result<RawRead, DocumentIoErrorCode> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DocumentIoErrorCode::NotFound
        } else {
            DocumentIoErrorCode::ReadFailed
        }
    })?;
    let opened = file
        .metadata()
        .map_err(|_| DocumentIoErrorCode::MetadataFailed)?;
    if !opened.is_file() {
        return Err(DocumentIoErrorCode::FileTypeInvalid);
    }
    if opened.len() > DOCUMENT_REFUSE_BYTES_MAX {
        return Ok(RawRead::TooLarge {
            byte_len: opened.len(),
        });
    }

    let mut bytes = Vec::with_capacity(opened.len().min(DOCUMENT_READ_PROBE_BYTES) as usize);
    std::io::Read::by_ref(&mut file)
        .take(DOCUMENT_READ_PROBE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| DocumentIoErrorCode::ReadFailed)?;
    if bytes.len() as u64 > DOCUMENT_REFUSE_BYTES_MAX {
        return Ok(RawRead::TooLarge {
            byte_len: bytes.len() as u64,
        });
    }

    // 읽는 도중 파일이 바뀌었으면(길이 불일치) 잘린/섞인 내용을 보여주지 않는다.
    let after = file
        .metadata()
        .map_err(|_| DocumentIoErrorCode::MetadataFailed)?;
    if after.len() != bytes.len() as u64 {
        return Err(DocumentIoErrorCode::Changed);
    }

    let revision = DocumentRevision::from_metadata(&after)?;
    Ok(RawRead::Content { bytes, revision })
}

/// 로컬 UTF-8 파일을 유계로 읽는다. 경로·내용은 반환값에만 담기고 로그로는 나가지
/// 않는다(§7).
pub fn load_document(request: &DocumentLoadRequest) -> DocumentLoadOutcome {
    let outcome = match read_document_bounded(&request.path) {
        Ok(RawRead::TooLarge { byte_len }) => DocumentLoadOutcome::Refused { byte_len },
        Ok(RawRead::Content { bytes, revision }) => {
            let byte_len = bytes.len() as u64;
            match String::from_utf8(bytes) {
                Ok(source) if byte_len <= DOCUMENT_FULL_BYTES_MAX => {
                    DocumentLoadOutcome::Loaded { source, revision }
                }
                Ok(source) => DocumentLoadOutcome::ViewOnly {
                    source,
                    revision,
                    byte_len,
                },
                // 비 UTF-8이면 내용을 버리고 그 사실만 돌려준다.
                Err(_) => DocumentLoadOutcome::Binary { byte_len },
            }
        }
        Err(code) => DocumentLoadOutcome::Failed { code },
    };
    if let DocumentLoadOutcome::Failed { code } = &outcome {
        tracing::warn!(
            kind = "document_load",
            error_code = code.as_str(),
            "document load failed"
        );
    }
    outcome
}

/// 저장 직전 다시 읽는 현재 revision(내용은 읽지 않는다).
fn current_revision(path: &Path) -> Result<DocumentRevision, DocumentIoErrorCode> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DocumentIoErrorCode::NotFound
        } else {
            DocumentIoErrorCode::MetadataFailed
        }
    })?;
    if !metadata.is_file() {
        return Err(DocumentIoErrorCode::FileTypeInvalid);
    }
    DocumentRevision::from_metadata(&metadata)
}

struct TempFileGuard(Option<PathBuf>);

impl TempFileGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 같은 디렉터리에 임시 파일을 flush한 뒤 atomic replace한다. 실패 시 대상 파일은
/// 절대 건드리지 않는다 — rename은 한 번만, 임시 파일이 완전히 쓰이고 sync된 뒤에만
/// 호출한다.
fn atomic_write_document(path: &Path, contents: &[u8]) -> Result<(), DocumentIoErrorCode> {
    let parent = path.parent().ok_or(DocumentIoErrorCode::ParentMissing)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    let temp = parent.join(format!(
        ".{file_name}.deppy-doc-tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&temp)
        .map_err(|_| DocumentIoErrorCode::TempCreateFailed)?;
    let mut guard = TempFileGuard(Some(temp.clone()));

    // 기존 파일의 권한을 유지한다(저장이 파일 모드를 바꾸는 부작용을 만들지 않는다).
    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, metadata.permissions());
    }

    file.write_all(contents)
        .map_err(|_| DocumentIoErrorCode::TempWriteFailed)?;
    file.sync_all()
        .map_err(|_| DocumentIoErrorCode::TempSyncFailed)?;
    drop(file);

    atomic_replace(&temp, path).map_err(|_| DocumentIoErrorCode::ReplaceFailed)?;
    guard.disarm();

    // 디렉터리 엔트리 durability는 best-effort — rename 자체는 이미 끝났으므로 이
    // 실패로 저장을 실패 처리하지 않는다(정전 대비 여유일 뿐).
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(windows)]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 두 경로는 호출 동안 살아 있는 NUL 종료 UTF-16 버퍼다.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

/// 문서 저장 요청. `expected_revision`은 로드(또는 직전 저장) 시점의 스냅샷이다.
#[derive(Debug, Clone)]
pub struct DocumentSaveRequest {
    pub path: PathBuf,
    pub contents: String,
    pub expected_revision: DocumentRevision,
}

/// 문서 저장 결과.
#[derive(Debug)]
pub enum DocumentSaveOutcome {
    /// 저장 완료 — 새 revision을 돌려준다(다음 저장의 `expected_revision`).
    Saved { revision: DocumentRevision },
    /// 저장 직전 다시 읽은 revision이 로드 시점과 달라 덮어쓰지 않았다.
    Conflict,
    /// I/O 실패.
    Failed { code: DocumentIoErrorCode },
}

/// 같은 디렉터리에 flush한 뒤 atomic replace한다. 저장 직전 `DocumentRevision`을 다시
/// 읽어 비교하고, 다르면(또는 파일이 사라졌으면) **덮어쓰지 않고** 충돌을 돌려준다(§7).
/// 외부 변경을 자동으로 다시 읽지는 않는다 — 호출자가 재로드 여부를 선택한다.
pub fn save_document(request: DocumentSaveRequest) -> DocumentSaveOutcome {
    let byte_len = request.contents.len() as u64;
    if byte_len > DOCUMENT_REFUSE_BYTES_MAX {
        let code = DocumentIoErrorCode::ContentTooLarge;
        tracing::warn!(
            kind = "document_save",
            error_code = code.as_str(),
            byte_len,
            "document save failed"
        );
        return DocumentSaveOutcome::Failed { code };
    }

    match current_revision(&request.path) {
        Ok(revision) if revision == request.expected_revision => {}
        Ok(_) => return DocumentSaveOutcome::Conflict,
        // 파일이 사라진 것도 외부 변경이다 — 덮어쓰지 않는다.
        Err(DocumentIoErrorCode::NotFound) => return DocumentSaveOutcome::Conflict,
        Err(code) => {
            tracing::warn!(
                kind = "document_save",
                error_code = code.as_str(),
                byte_len,
                "document save failed"
            );
            return DocumentSaveOutcome::Failed { code };
        }
    }

    let outcome = match atomic_write_document(&request.path, request.contents.as_bytes()) {
        Ok(()) => match current_revision(&request.path) {
            Ok(revision) => DocumentSaveOutcome::Saved { revision },
            Err(code) => DocumentSaveOutcome::Failed { code },
        },
        Err(code) => DocumentSaveOutcome::Failed { code },
    };
    if let DocumentSaveOutcome::Failed { code } = &outcome {
        tracing::warn!(
            kind = "document_save",
            error_code = code.as_str(),
            byte_len,
            "document save failed"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deppy-document-io-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    // ── 티어 판정 경계값 ────────────────────────────────────────────────

    #[test]
    fn full_tier_at_exact_boundary() {
        let dir = unique_dir("full-exact");
        let bytes = vec![b'a'; DOCUMENT_FULL_BYTES_MAX as usize];
        let path = write_file(&dir, "doc.md", &bytes);
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::Full));
        assert!(matches!(outcome, DocumentLoadOutcome::Loaded { .. }));
    }

    #[test]
    fn view_only_tier_just_above_full_boundary() {
        let dir = unique_dir("full-plus-one");
        let bytes = vec![b'a'; DOCUMENT_FULL_BYTES_MAX as usize + 1];
        let path = write_file(&dir, "doc.md", &bytes);
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::ViewOnly));
    }

    #[test]
    fn view_only_tier_at_refuse_boundary() {
        let dir = unique_dir("refuse-exact");
        let bytes = vec![b'a'; DOCUMENT_REFUSE_BYTES_MAX as usize];
        let path = write_file(&dir, "doc.md", &bytes);
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::ViewOnly));
    }

    #[test]
    fn refuse_tier_just_above_refuse_boundary() {
        let dir = unique_dir("refuse-plus-one");
        let bytes = vec![b'a'; DOCUMENT_REFUSE_BYTES_MAX as usize + 1];
        let path = write_file(&dir, "doc.md", &bytes);
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::Refuse));
        // 상한 초과 시 내용을 아예 읽지 않는다 — 결과에 담기는 건 크기뿐.
        assert!(matches!(
            outcome,
            DocumentLoadOutcome::Refused { byte_len } if byte_len == DOCUMENT_REFUSE_BYTES_MAX + 1
        ));
    }

    #[test]
    fn zero_byte_file_is_full_tier() {
        let dir = unique_dir("zero-byte");
        let path = write_file(&dir, "doc.md", b"");
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::Full));
        assert!(matches!(outcome, DocumentLoadOutcome::Loaded { source, .. } if source.is_empty()));
    }

    #[test]
    fn non_utf8_file_is_binary_tier_and_drops_content() {
        let dir = unique_dir("non-utf8");
        // 0x80은 UTF-8 연속 바이트로만 유효하고 단독으로는 무효하다.
        let path = write_file(&dir, "doc.md", &[0xFF, 0xFE, 0x00, 0x80]);
        let outcome = load_document(&DocumentLoadRequest { path });
        assert_eq!(outcome.tier(), Some(DocumentLimitTier::Binary));
        assert!(matches!(outcome, DocumentLoadOutcome::Binary { byte_len } if byte_len == 4));
    }

    #[test]
    fn missing_file_is_failed_not_found() {
        let dir = unique_dir("missing");
        let outcome = load_document(&DocumentLoadRequest {
            path: dir.join("does-not-exist.md"),
        });
        assert!(matches!(
            outcome,
            DocumentLoadOutcome::Failed {
                code: DocumentIoErrorCode::NotFound
            }
        ));
    }

    // ── 저장: revision 충돌 시 덮어쓰지 않는다 ─────────────────────────

    #[test]
    fn save_conflict_when_file_changed_externally_does_not_overwrite() {
        let dir = unique_dir("save-conflict");
        let path = write_file(&dir, "doc.md", b"ORIGINAL");
        let load = load_document(&DocumentLoadRequest { path: path.clone() });
        let DocumentLoadOutcome::Loaded { revision, .. } = load else {
            panic!("expected Loaded");
        };

        // 로드 이후 외부에서 파일이 바뀐다(길이가 달라 revision도 반드시 달라진다).
        std::fs::write(&path, b"EXTERNALLY CHANGED CONTENT").unwrap();

        let outcome = save_document(DocumentSaveRequest {
            path: path.clone(),
            contents: "MY EDIT".to_owned(),
            expected_revision: revision,
        });
        assert!(matches!(outcome, DocumentSaveOutcome::Conflict));
        // 디스크의 외부 변경 내용이 그대로 남아 있어야 한다 — 덮어쓰지 않았다.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EXTERNALLY CHANGED CONTENT"
        );
    }

    #[test]
    fn save_conflict_when_file_deleted_externally() {
        let dir = unique_dir("save-conflict-deleted");
        let path = write_file(&dir, "doc.md", b"ORIGINAL");
        let load = load_document(&DocumentLoadRequest { path: path.clone() });
        let DocumentLoadOutcome::Loaded { revision, .. } = load else {
            panic!("expected Loaded");
        };
        std::fs::remove_file(&path).unwrap();

        let outcome = save_document(DocumentSaveRequest {
            path,
            contents: "MY EDIT".to_owned(),
            expected_revision: revision,
        });
        assert!(matches!(outcome, DocumentSaveOutcome::Conflict));
    }

    #[test]
    fn save_succeeds_when_revision_matches_and_advances_revision() {
        let dir = unique_dir("save-success");
        let path = write_file(&dir, "doc.md", b"ORIGINAL");
        let load = load_document(&DocumentLoadRequest { path: path.clone() });
        let DocumentLoadOutcome::Loaded { revision, .. } = load else {
            panic!("expected Loaded");
        };

        let outcome = save_document(DocumentSaveRequest {
            path: path.clone(),
            contents: "UPDATED".to_owned(),
            expected_revision: revision,
        });
        let DocumentSaveOutcome::Saved {
            revision: new_revision,
        } = outcome
        else {
            panic!("expected Saved");
        };
        assert_ne!(new_revision, revision);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "UPDATED");

        // 저장 후 임시 파일이 남아있지 않아야 한다.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("deppy-doc-tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    // ── atomic replace: 중간 실패에서 원본을 깨뜨리지 않는다 ───────────

    #[test]
    fn atomic_replace_never_touches_target_when_source_is_missing() {
        // rename 자체가 원자적이라, 임시 파일이 완성되지 않은 채로는 절대 원본을
        // 바꾸지 않는다는 걸 직접 확인한다 — 존재하지 않는 temp로 replace를 시도해도
        // 원본은 그대로여야 한다.
        let dir = unique_dir("atomic-replace-missing-source");
        let target = write_file(&dir, "doc.md", b"ORIGINAL");
        let phantom_temp = dir.join(".phantom-temp-never-written");

        let result = atomic_replace(&phantom_temp, &target);
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
    }

    #[cfg(unix)]
    #[test]
    fn save_mid_failure_leaves_original_untouched() {
        // 임시 파일 생성 자체가 실패하는 상황(디렉터리 쓰기 권한 없음)을 강제해,
        // atomic_write_document가 원본에 손도 대지 않고 실패로 돌아오는지 확인한다.
        use std::os::unix::fs::PermissionsExt as _;

        let dir = unique_dir("save-mid-failure");
        let path = write_file(&dir, "doc.md", b"ORIGINAL");
        let load = load_document(&DocumentLoadRequest { path: path.clone() });
        let DocumentLoadOutcome::Loaded { revision, .. } = load else {
            panic!("expected Loaded");
        };

        let original_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = save_document(DocumentSaveRequest {
            path: path.clone(),
            contents: "SHOULD NOT LAND".to_owned(),
            expected_revision: revision,
        });

        // 정리 전에 권한을 복구해야 tempdir 삭제 등 후속 정리가 막히지 않는다.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(original_mode)).unwrap();

        assert!(matches!(
            outcome,
            DocumentSaveOutcome::Failed {
                code: DocumentIoErrorCode::TempCreateFailed
            }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ORIGINAL");
    }

    // ── 경로·내용이 로그에 안 나가는지 ─────────────────────────────────

    #[test]
    fn production_source_never_logs_path_or_content() {
        // 구현이 되돌아가 path.display()/to_string_lossy나 tracing 필드에 path·
        // source·contents를 직접 꽂으면 이 테스트가 실패한다.
        let production = include_str!("document_io.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            ".display()",
            "to_string_lossy",
            "%path",
            "?path",
            "{path}",
            "%source",
            "?source",
            "{source}",
            "%contents",
            "?contents",
            "{contents}",
        ] {
            assert!(
                !production.contains(forbidden),
                "found forbidden token in production source: {forbidden}"
            );
        }
        assert!(production.contains("error_code"));
    }

    // ── 측정: TextEdit 레이아웃 비용(#[ignore], CI에서 돌지 않음) ──────
    //
    // 실행: cargo test -p deppy-sijo --release document_io::tests::bench_textedit_layout \
    //   -- --ignored --nocapture --test-threads=1
    //
    // 크기별로 별도 프로세스(또는 최소 --test-threads=1)로 돌려야 RSS 측정이 깨끗하다
    // (`getrusage`의 ru_maxrss는 프로세스 생애 전체의 고점이라 감소하지 않는다).

    fn bench_text(target_bytes: usize) -> String {
        // 실제 마크다운 문서와 비슷한 줄 길이를 흉내내는 반복 문단.
        const BLOCK: &str = "이것은 문서 편집기 성능 측정을 위한 더미 문단입니다. 한 줄에 다양한 길이의 한글과 영문이 섞여 실제 마크다운 텍스트의 줄바꿈 빈도를 대략 흉내냅니다.\n";
        let mut text = String::with_capacity(target_bytes + BLOCK.len());
        while text.len() < target_bytes {
            text.push_str(BLOCK);
        }
        // 한글은 멀티바이트라 임의 바이트 위치에서 자르면 글자 경계를 깰 수 있다 —
        // target_bytes 이하의 가장 가까운 글자 경계로 자른다.
        let mut cut = target_bytes.min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text
    }

    /// 근사 RSS(바이트). 플랫폼별 `ru_maxrss` 단위 차이(macOS=bytes, 그 외=KB)를 보정한다.
    fn approx_peak_rss_bytes() -> i64 {
        // SAFETY: `rusage`는 POD 구조체이고 `getrusage`는 그 버퍼만 채우는 표준 libc
        // 호출이다. 반환값은 항상 0(성공) 또는 -1이며 실패해도 zeroed 상태로 읽는다.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            #[cfg(target_os = "macos")]
            {
                usage.ru_maxrss as i64
            }
            #[cfg(not(target_os = "macos"))]
            {
                usage.ru_maxrss as i64 * 1024
            }
        }
    }

    fn bench_textedit_layout(label: &str, target_bytes: usize) {
        let text = bench_text(target_bytes);
        let rss_before = approx_peak_rss_bytes();

        // `build_ui_state`가 첫 프레임(첫 레이아웃)을 이미 실행하므로, 이 호출 자체를
        // 잰다.
        let first_layout_start = std::time::Instant::now();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::Vec2::new(900.0, 700.0))
            .build_ui_state(
                |ui, text: &mut String| {
                    ui.add(egui::TextEdit::multiline(text).desired_width(f32::INFINITY));
                },
                text,
            );
        let first_layout = first_layout_start.elapsed();

        // 한 글자 편집을 흉내내 다음 프레임의 재레이아웃 비용을 잰다.
        harness.state_mut().push('x');
        let relayout_start = std::time::Instant::now();
        harness.step();
        let relayout = relayout_start.elapsed();

        let rss_after = approx_peak_rss_bytes();
        eprintln!(
            "[document_io bench] {label}: first_layout={first_layout:?} relayout={relayout:?} rss_delta~={}KB",
            (rss_after - rss_before) / 1024
        );
    }

    #[test]
    #[ignore = "측정용 벤치 — 수동 실행, 위 실행법 주석 참조"]
    fn bench_textedit_layout_1mb() {
        bench_textedit_layout("1MiB", 1024 * 1024);
    }

    #[test]
    #[ignore = "측정용 벤치 — 수동 실행, 위 실행법 주석 참조"]
    fn bench_textedit_layout_4mb() {
        bench_textedit_layout("4MiB", 4 * 1024 * 1024);
    }

    #[test]
    #[ignore = "측정용 벤치 — 수동 실행, 위 실행법 주석 참조"]
    fn bench_textedit_layout_10mb() {
        bench_textedit_layout("10MiB", 10 * 1024 * 1024);
    }
}
