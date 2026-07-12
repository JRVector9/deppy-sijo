//! 모바일 파일 첨부 업로드 (계획 PR-P6d) — `POST /upload?token=`.
//!
//! multipart 아님: 폰이 파일 원본 바이트를 그대로 본문으로 보내고 Content-Type 헤더로 종류를
//! 알린다(별도 파일명 필드가 프로토콜에 아예 없다). 파일명/확장자는 **서버가 생성**한다
//! (uuid v4 + Content-Type 매핑 확장자) — 클라가 경로/이름을 지정할 방법이 없어 path
//! traversal이 구조적으로 불가능하다. 저장 후 절대 경로를 JSON으로 돌려주면 app.js가 composer
//! textarea에 붙여 넣는다 — 데스크톱 이미지 paste와 같은 종단(에이전트가 경로를 읽는다).
//!
//! GC: `storage::scrollback_archive`의 "총량 예산 + mtime LRU" 관례를 차용한다. 다만 그
//! 모듈은 세션별 하위 디렉터리(`logs_root/<uuid>/scrollback.zlib`) 전제라, 세션에 묶이지 않는
//! 평면 업로드 디렉터리에는 맞지 않아 이 파일에 독립적으로 같은 패턴을 재구현한다.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::http::{RequestHead, Response};

/// 업로드 본문 상한 — 확정 사양 10MB. Content-Length 선검사(lib.rs handle_connection)로
/// 이 상한을 넘는 요청은 본문을 읽기 전에 413으로 거부돼 메모리 점유가 유계다. 아래에서
/// body.len()도 한 번 더 확인한다(선검사 경로가 우회되는 경우에 대한 방어적 이중 검사).
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// 업로드 디렉터리 총량 예산 — 초과 시 mtime 오래된 파일부터 GC(scrollback_archive 관례).
const UPLOAD_DISK_BUDGET_BYTES: u64 = 200 * 1024 * 1024;

/// Content-Type → 저장 확장자 화이트리스트. 여기 없는 타입은 전부 거부(415)한다 — 이미지
/// (png/jpg/jpeg/gif/webp/heic) + 문서(pdf/docx/doc/xlsx/xls/pptx/ppt/txt/csv/md).
const ALLOWED_TYPES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/heic", "heic"),
    ("application/pdf", "pdf"),
    ("application/msword", "doc"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.ms-excel", "xls"),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("application/vnd.ms-powerpoint", "ppt"),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    ("text/plain", "txt"),
    ("text/csv", "csv"),
    ("text/markdown", "md"),
];

/// `POST /upload` 요청을 처리한다. 그 경로/메서드가 아니면 None(정적 라우팅으로 흘려보냄).
/// `uploads_dir` 미설정(테스트/미배선)이면 404 — 업로드 기능 자체가 비활성이다.
pub fn route(
    head: &RequestHead,
    body: &[u8],
    token: &str,
    uploads_dir: Option<&Path>,
) -> Option<Response> {
    if head.method != "POST" || head.path != "/upload" {
        return None;
    }
    Some(upload_response(head, body, token, uploads_dir))
}

fn upload_response(
    head: &RequestHead,
    body: &[u8],
    token: &str,
    uploads_dir: Option<&Path>,
) -> Response {
    if !token_query_matches(&head.query, token) {
        return Response::plain(401, "unauthorized");
    }
    let Some(dir) = uploads_dir else {
        return Response::plain(404, "upload disabled");
    };
    let Some(ext) = content_type_ext(head.header("content-type")) else {
        return Response::plain(415, "unsupported media type");
    };
    if body.is_empty() {
        return Response::plain(400, "empty upload");
    }
    if body.len() > MAX_UPLOAD_BYTES {
        return Response::plain(413, "upload too large");
    }
    match save_upload(dir, ext, body) {
        Ok(path) => {
            let json = serde_json::json!({ "path": path }).to_string();
            Response {
                status: 201,
                content_type: "application/json",
                body: std::borrow::Cow::Owned(json.into_bytes()),
            }
        }
        Err(e) => {
            tracing::warn!("업로드 저장 실패: {e:#}");
            Response::plain(500, "upload failed")
        }
    }
}

/// Content-Type 헤더(파라미터 포함 가능: `image/jpeg; foo=bar`)를 화이트리스트와 대조해
/// 저장 확장자를 돌려준다. 대소문자 무시. 헤더 없음/화이트리스트 밖이면 None.
fn content_type_ext(content_type: Option<&str>) -> Option<&'static str> {
    let raw = content_type?;
    let mime = raw.split(';').next().unwrap_or(raw).trim();
    ALLOWED_TYPES
        .iter()
        .find(|(ty, _)| ty.eq_ignore_ascii_case(mime))
        .map(|(_, ext)| *ext)
}

/// 파일명은 uuid v4 + Content-Type 매핑 확장자로 서버가 전부 생성한다(클라 입력 없음) —
/// path traversal이 구조적으로 불가능하다. tmp+rename으로 원자 기록(scrollback_archive
/// 관례) 후 실행 권한 없이(0o644) 저장하고, 디렉터리 총량 GC를 수행한다. 반환값은 절대 경로
/// 문자열(에이전트가 그대로 읽을 수 있게).
fn save_upload(dir: &Path, ext: &str, body: &[u8]) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("업로드 디렉터리 생성 실패: {}", dir.display()))?;
    let filename = format!("{}.{ext}", uuid::Uuid::new_v4());
    let path = dir.join(&filename);
    let tmp = dir.join(format!("{filename}.tmp"));
    std::fs::write(&tmp, body).with_context(|| format!("tmp 기록 실패: {}", tmp.display()))?;
    // perms 실패도 tmp를 남기지 않는다(리뷰 P3: GC가 .tmp를 안 세므로 예산·정리를 우회).
    if let Err(e) = set_no_exec_perms(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("업로드 rename 실패: {}", path.display())
    })?;
    // 업로드 자체는 이미 성공했다 — GC 실패로 요청을 실패시키지 않는다(경고만).
    gc(dir, UPLOAD_DISK_BUDGET_BYTES);
    let absolute = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(absolute.to_string_lossy().into_owned())
}

/// 실행 권한 없이 저장(0o644) — 서버는 업로드된 바이트를 파싱/실행하지 않는다(저장만).
#[cfg(unix)]
fn set_no_exec_perms(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("업로드 권한 설정 실패: {}", path.display()))
}

#[cfg(not(unix))]
fn set_no_exec_perms(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 쓰기 도중 크래시/실패로 남은 `.tmp` 잔재를 mtime 기준으로 정리한다. GC 스캔은 `.tmp`를
/// 세지도 지우지도 않으므로(collect_uploads), 이걸 안 하면 잔재가 디스크·예산을 영구 우회한다
/// (리뷰 P3). in-flight 업로드는 UPLOAD_READ_TIMEOUT(120s) 안에 rename되므로 그보다 넉넉한
/// 5분을 넘긴 것만 지운다.
fn sweep_stale_tmp(dir: &Path) {
    const TMP_STALE_AGE: std::time::Duration = std::time::Duration::from_secs(300);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tmp")
            && let Ok(meta) = entry.metadata()
            && let Ok(mtime) = meta.modified()
            && now
                .duration_since(mtime)
                .is_ok_and(|age| age >= TMP_STALE_AGE)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `dir` 아래 파일들의 총 바이트가 예산을 넘으면 mtime 오래된 것부터 삭제한다
/// (`storage::scrollback_archive::gc`와 동일 패턴). 스캔/삭제 실패는 경고만 남기고 계속한다.
fn gc(dir: &Path, budget_bytes: u64) {
    sweep_stale_tmp(dir); // 예산과 무관하게 잔재 tmp를 먼저 정리한다.
    let mut files = match collect_uploads(dir) {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!("업로드 GC 스캔 실패: {e}");
            return;
        }
    };
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= budget_bytes {
        return;
    }
    files.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, len, path) in files {
        if total <= budget_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(len);
                tracing::info!(path = %path.display(), "업로드 GC — 예산 초과 제거");
            }
            // 동시 GC 패스가 이미 지웠다 — 크기를 차감해 이 패스가 다음(멀쩡한) 파일까지
            // 과다삭제하지 않게 한다(리뷰 P3: NotFound 미차감 시 초과 제거).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                total = total.saturating_sub(len);
            }
            Err(e) => tracing::warn!(path = %path.display(), "업로드 GC 삭제 실패: {e}"),
        }
    }
}

/// 업로드 디렉터리의 파일들을 (mtime, len, path)로 모은다. `.tmp` 잔재(쓰기 도중 크래시)는
/// 아직 유효한 업로드가 아니므로 GC 스캔에서 제외한다.
fn collect_uploads(dir: &Path) -> std::io::Result<Vec<(std::time::SystemTime, u64, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tmp") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((mtime, meta.len(), path));
        }
    }
    Ok(files)
}

/// `/upload` 토큰 인가 — handle_connection(lib.rs)이 **본문을 읽기 전에** 호출한다(리뷰
/// P2-1: 미인증 요청이 10MB를 선할당하지 않게). route 내부 검사와 같은 상수시간 규약.
pub fn authorized(query: &str, token: &str) -> bool {
    token_query_matches(query, token)
}

/// query의 `token=` 파라미터를 상수시간 비교한다(static_srv/push 게이트와 동일 규약 —
/// 두 모듈도 각자 이 짧은 헬퍼를 private로 갖는다).
fn token_query_matches(query: &str, expected: &str) -> bool {
    let Some(provided) = query.split('&').find_map(|kv| kv.strip_prefix("token=")) else {
        return false;
    };
    crate::static_srv::token_matches(expected, provided.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("deppy-upload-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn content_type_화이트리스트_매칭() {
        assert_eq!(content_type_ext(Some("image/png")), Some("png"));
        assert_eq!(content_type_ext(Some("IMAGE/JPEG")), Some("jpg")); // 대소문자 무시
        assert_eq!(
            content_type_ext(Some("text/plain; charset=utf-8")),
            Some("txt")
        ); // 파라미터 제거
        assert_eq!(content_type_ext(Some("application/octet-stream")), None);
        assert_eq!(content_type_ext(None), None);
    }

    #[test]
    fn token_불일치는_거부() {
        assert!(token_query_matches("token=abc", "abc"));
        assert!(!token_query_matches("token=abc", "xyz"));
        assert!(!token_query_matches("", "abc"));
    }

    #[test]
    fn save_upload은_uuid_파일명으로_저장하고_절대경로를_돌려준다() {
        let dir = temp_dir();
        let path_str = save_upload(&dir, "png", b"fake-png-bytes").unwrap();
        let path = PathBuf::from(&path_str);
        assert!(path.is_absolute(), "{path_str}");
        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "png");
        // 파일명이 uuid 형식(하이픈 포함 36자) + .png
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert_eq!(stem.len(), 36, "{stem}");
        assert_eq!(std::fs::read(&path).unwrap(), b"fake-png-bytes");
        // tmp 잔재 없음(원자 기록)
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .all(|e| e.path().extension().is_none_or(|ext| ext != "tmp"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_upload은_실행권한_없이_저장한다() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path_str = save_upload(&dir, "txt", b"hello").unwrap();
        let mode = std::fs::metadata(&path_str).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "{mode:o}");
    }

    #[test]
    fn gc는_예산_초과시_오래된_파일부터_제거한다() {
        let dir = temp_dir();
        let payload = vec![b'x'; 1024];
        for (i, name) in ["old.png", "mid.png", "new.png"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, &payload).unwrap();
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000_000 + i as u64 * 1000);
            let file = std::fs::File::options().append(true).open(&path).unwrap();
            file.set_modified(time).unwrap();
        }
        // 방금 만든 .tmp(in-flight 업로드)는 예산 계산·삭제 대상이 아니다 — 갓 생성돼
        // sweep 연령(5분)보다 어리므로 tmp sweep도 건드리지 않는다.
        std::fs::write(dir.join("fresh.png.tmp"), &payload).unwrap();

        gc(&dir, 1024 * 2); // 3개(각 1024) 중 2개 예산 → 가장 오래된 1개 제거
        assert!(!dir.join("old.png").exists());
        assert!(dir.join("mid.png").exists());
        assert!(dir.join("new.png").exists());
        assert!(dir.join("fresh.png.tmp").exists(), "갓 만든 tmp는 보존");
    }

    #[test]
    fn gc는_오래된_tmp_잔재를_정리한다() {
        // 크래시/perms 실패로 남은 .tmp는 collect_uploads가 안 세므로 디스크·예산을
        // 영구 우회한다(리뷰 P3). sweep이 연령 지난 것만 지운다.
        let dir = temp_dir();
        let stale = dir.join("orphan.png.tmp");
        std::fs::write(&stale, vec![b'x'; 1024]).unwrap();
        // mtime을 충분히 과거로(1970 기준) — 5분 연령 임계 초과.
        std::fs::File::options()
            .append(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        gc(&dir, u64::MAX); // 예산은 넉넉해도 tmp sweep은 돈다.
        assert!(!stale.exists(), "연령 지난 tmp 잔재는 정리돼야 한다");
    }

    #[test]
    fn 예산_이내면_gc가_아무것도_지우지_않는다() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.png"), vec![b'x'; 1024]).unwrap();
        gc(&dir, 1024 * 1024);
        assert!(dir.join("a.png").exists());
    }
}
