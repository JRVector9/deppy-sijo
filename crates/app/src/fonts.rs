//! egui 기본 폰트에는 한글 글리프가 없다 — 시스템 CJK 폰트를 fallback으로 등록한다.
//! (PR-05 완료 기준: 한글 렌더링 깨짐 없음)

/// 플랫폼별 한글 폰트 후보 (앞에서부터 시도).
#[cfg(target_os = "macos")]
const CJK_FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
    "/System/Library/Fonts/Supplemental/NotoSansGothic-Regular.ttf",
];
#[cfg(target_os = "windows")]
const CJK_FONT_CANDIDATES: &[&str] = &[
    "C:\\Windows\\Fonts\\malgun.ttf",
    "C:\\Windows\\Fonts\\gulim.ttc",
];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CJK_FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
];

/// 한글 fallback 폰트를 등록한다. 실패해도 앱은 계속 뜬다 (한글만 깨짐).
pub fn install_cjk_fallback(ctx: &egui::Context) {
    let Some((path, bytes)) = CJK_FONT_CANDIDATES
        .iter()
        .find_map(|p| std::fs::read(p).ok().map(|b| (*p, b)))
    else {
        tracing::warn!("한글 폰트를 찾지 못함 — 터미널 한글이 깨질 수 있음");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
    tracing::info!(font = path, "한글 fallback 폰트 등록");
}
