use std::path::{Path, PathBuf};

use anyhow::Context;

fn paste_clipboard_paths_or_image_to_paths() -> anyhow::Result<Option<Vec<PathBuf>>> {
    let mut clipboard = arboard::Clipboard::new().context("clipboard 열기 실패")?;
    if let Some(paths) = clipboard_file_list(&mut clipboard)? {
        return Ok(Some(paths));
    }
    paste_clipboard_image_to_png_with(&mut clipboard).map(|path| path.map(|path| vec![path]))
}

/// 클립보드 파일/이미지 paste를 백그라운드 스레드에서 처리한다 — get_image()의 전체 RGBA
/// 복사 + PNG 인코딩(스크린샷 기준 50~300ms)이 UI 스레드를 멈추던 딜레이 제거(2026-07-07).
/// 결과는 채널로 오고, 완료 시 repaint를 깨워 다음 프레임에 즉시 소비된다.
pub fn paste_clipboard_paths_or_image_background(
    ctx: egui::Context,
    has_text_fallback: bool,
) -> std::sync::mpsc::Receiver<anyhow::Result<Option<Vec<PathBuf>>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut result = paste_clipboard_paths_or_image_to_paths();
        // 스크린샷 직후 ⌘V 레이스: 캡처 유틸이 클립보드에 이미지를 쓰기까지 수백 ms 걸릴
        // 수 있어 첫 ⌘V가 "빈 클립보드"로 무시됐다(2026-07-08 사용자). 파일/이미지/텍스트가
        // 전부 없을 때만 잠깐 기다렸다 재시도한다 — 이미 Event::Paste 텍스트를 받았거나
        // (has_text_fallback — 기다리면 그 텍스트 붙여넣기만 늦어짐, codex Low) 클립보드에
        // 텍스트가 있으면 재시도 없이 즉시 반환.
        let mut tries = 0;
        while !has_text_fallback
            && tries < 4
            && matches!(&result, Ok(None))
            && read_clipboard_text().is_none()
        {
            std::thread::sleep(std::time::Duration::from_millis(150));
            result = paste_clipboard_paths_or_image_to_paths();
            tries += 1;
        }
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
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

/// macOS: pasteboard의 PNG 바이트를 **디코드 없이 그대로** 가져온다 — cmux와 동일 접근.
/// 스크린샷은 pasteboard에 이미 PNG로 있으므로 파일로 쓰기만 하면 된다(수 ms).
/// arboard get_image()는 RGBA 디코드 + PNG 재인코딩 왕복이라 수백 ms 걸렸다(2026-07-08).
#[cfg(target_os = "macos")]
fn clipboard_png_bytes() -> Option<Vec<u8>> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
    // SAFETY: generalPasteboard는 공유 싱글턴 반환, dataForType은 불변 조회 —
    // 백그라운드 스레드에서 읽기 전용 접근은 NSPasteboard 문서상 허용.
    unsafe {
        let pasteboard = NSPasteboard::generalPasteboard();
        let data = pasteboard.dataForType(NSPasteboardTypePNG)?;
        Some(data.to_vec())
    }
}

fn paste_clipboard_image_to_png_with(
    clipboard: &mut arboard::Clipboard,
) -> anyhow::Result<Option<PathBuf>> {
    // 1) macOS 빠른 경로: pasteboard PNG 바이트를 그대로 파일로 (변환 0).
    #[cfg(target_os = "macos")]
    {
        const MAX_PNG_BYTES: usize = 32 * 1024 * 1024; // 32MB — 비정상 payload 방지
        if let Some(png) = clipboard_png_bytes()
            && !png.is_empty()
            && png.len() <= MAX_PNG_BYTES
        {
            let path = next_clipboard_image_path();
            if let Some(dir) = path.parent() {
                prune_clipboard_cache(dir);
                std::fs::create_dir_all(dir).with_context(|| {
                    format!("clipboard 이미지 디렉터리 생성 실패: {}", dir.display())
                })?;
            }
            std::fs::write(&path, &png)
                .with_context(|| format!("clipboard PNG 저장 실패: {}", path.display()))?;
            return Ok(Some(path));
        }
    }
    // 2) fallback: RGBA 디코드 + Fast PNG 인코딩 (TIFF-only pasteboard, 비 macOS 등).
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
    // UUID로 유일성 보장 — 대체된 옛 paste task가 아직 살아있어 같은 millisecond에 경로를
    // 잡으면 파일이 서로 덮여 최신 반환 경로 내용이 옛 바이트가 되던 레이스 방지(codex).
    dir.join(format!(
        "clipboard-image-{}-{millis}-{}.png",
        std::process::id(),
        uuid::Uuid::new_v4()
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
    // Fast 압축 — 기본 압축은 스크린샷(수 MP)에서 수백 ms 걸려 붙여넣기 지연의 주범이었다
    // (2026-07-08). 파일이 조금 커지지만 임시 캐시(24h TTL)라 트레이드오프가 맞다.
    use image::ImageEncoder;
    let file = std::fs::File::create(path)
        .with_context(|| format!("clipboard 이미지 파일 생성 실패: {}", path.display()))?;
    image::codecs::png::PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        rgba,
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgba8,
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
