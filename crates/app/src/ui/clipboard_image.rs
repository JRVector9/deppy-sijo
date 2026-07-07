use std::path::{Path, PathBuf};

use anyhow::Context;

pub fn paste_clipboard_paths_or_image_to_paths() -> anyhow::Result<Option<Vec<PathBuf>>> {
    let mut clipboard = arboard::Clipboard::new().context("clipboard 열기 실패")?;
    if let Some(paths) = clipboard_file_list(&mut clipboard)? {
        return Ok(Some(paths));
    }
    paste_clipboard_image_to_png_with(&mut clipboard).map(|path| path.map(|path| vec![path]))
}

/// OS 클립보드의 텍스트를 직접 읽는다(빈/부재/에러는 None). ⌘V 시 egui Event::Paste가
/// 터미널 위젯(비 텍스트에딧)에 안 오는 경우의 fallback — claude/codex 상태창 붙여넣기(#4).
pub fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

fn clipboard_file_list(clipboard: &mut arboard::Clipboard) -> anyhow::Result<Option<Vec<PathBuf>>> {
    match clipboard.get().file_list() {
        Ok(paths) => Ok((!paths.is_empty()).then_some(paths)),
        Err(arboard::Error::ContentNotAvailable)
        | Err(arboard::Error::ClipboardNotSupported)
        | Err(arboard::Error::ConversionFailure) => Ok(None),
        Err(e) => Err(e).context("clipboard 파일 목록 읽기 실패"),
    }
}

fn paste_clipboard_image_to_png_with(
    clipboard: &mut arboard::Clipboard,
) -> anyhow::Result<Option<PathBuf>> {
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(e).context("clipboard 이미지 읽기 실패"),
    };
    // 너무 큰 이미지는 UI 스레드 동기 PNG 인코딩이 hitch를 유발 — 상한(codex 리뷰).
    const MAX_PIXELS: usize = 40_000_000; // ~40MP
    anyhow::ensure!(
        image.width.saturating_mul(image.height) <= MAX_PIXELS,
        "clipboard 이미지가 너무 큽니다({}x{}) — 붙여넣기 생략",
        image.width,
        image.height
    );
    let path = next_clipboard_image_path();
    // 오래된 캐시 파일 정리(무한 증가 방지, codex 리뷰).
    if let Some(dir) = path.parent() {
        prune_clipboard_cache(dir);
    }
    write_rgba_png(&path, image.width, image.height, image.bytes.as_ref())?;
    Ok(Some(path))
}

/// clipboard-images 캐시에서 TTL(24h) 지난 파일을 지운다 — paste마다 새 파일을 만들어
/// 쌓이던 것을 유계화(codex 리뷰). best-effort.
fn prune_clipboard_cache(dir: &Path) {
    const TTL_SECS: u64 = 24 * 60 * 60;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        if let Ok(meta) = e.metadata()
            && let Ok(modified) = meta.modified()
            && now
                .duration_since(modified)
                .is_ok_and(|age| age.as_secs() > TTL_SECS)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn next_clipboard_image_path() -> PathBuf {
    let dir = directories::ProjectDirs::from("city", "ahto", "deppy-sijo")
        .map(|dirs| dirs.cache_dir().join("clipboard-images"))
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("deppy-sijo")
                .join("clipboard-images")
        });
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    dir.join(format!(
        "clipboard-image-{}-{millis}.png",
        std::process::id()
    ))
}

fn write_rgba_png(path: &Path, width: usize, height: usize, rgba: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(width > 0 && height > 0, "clipboard 이미지 크기가 비어 있음");
    anyhow::ensure!(
        width <= u32::MAX as usize && height <= u32::MAX as usize,
        "clipboard 이미지 크기가 PNG 저장 한도를 초과함"
    );
    anyhow::ensure!(
        rgba.len() == width.saturating_mul(height).saturating_mul(4),
        "clipboard 이미지 RGBA 버퍼 크기 불일치"
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("clipboard 이미지 디렉터리 생성 실패: {}", parent.display())
        })?;
    }
    image::save_buffer_with_format(
        path,
        rgba,
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .with_context(|| format!("clipboard 이미지 저장 실패: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_png_writer_rejects_wrong_buffer_size() {
        let path = std::env::temp_dir().join(format!(
            "deppy-sijo-bad-clipboard-image-{}.png",
            std::process::id()
        ));
        let err = write_rgba_png(&path, 2, 2, &[0, 0, 0, 255]).unwrap_err();
        assert!(err.to_string().contains("RGBA"));
    }

    #[test]
    fn rgba_png_writer_creates_png_file() {
        let path = std::env::temp_dir().join(format!(
            "deppy-sijo-clipboard-image-test-{}.png",
            std::process::id()
        ));
        let rgba = [
            255, 0, 0, 255, //
            0, 255, 0, 255,
        ];
        write_rgba_png(&path, 2, 1, &rgba).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let _ = std::fs::remove_file(path);
    }
}
