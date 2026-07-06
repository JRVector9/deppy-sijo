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
/// macOS에선 목업(system-ui)과 동일하게 SF Pro(UI)·SF Mono(터미널)를 기본 폰트로
/// 얹는다 (2026-07-06). 한글은 CJK fallback이 커버한다.
pub fn install_cjk_fallback(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 한글 fallback (families 끝에 붙여 Latin은 기본/SF, 한글만 이 폰트가 처리)
    if let Some((path, bytes)) = CJK_FONT_CANDIDATES
        .iter()
        .find_map(|p| std::fs::read(p).ok().map(|b| (*p, b)))
    {
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
        tracing::info!(font = path, "한글 fallback 폰트 등록");
    } else {
        tracing::warn!("한글 폰트를 찾지 못함 — 한글이 깨질 수 있음");
    }

    // macOS: Apple SD Gothic Neo(.ttc)를 UI(Proportional) 기본 폰트로 얹는다 —
    // macOS 네이티브 UI 폰트(Latin+한글 통합)라 목업 system-ui 룩과 일치. SFNS.ttf(SF Pro)는
    // fvar 가변폰트라 egui/skrifa가 무시했다(2026-07-06) — .ttc는 index로 로드된다.
    // 모노(터미널)는 SFNSMono도 가변이라 안 바꾸고 egui 기본(Hack)을 유지한다.
    #[cfg(target_os = "macos")]
    {
        if let Ok(bytes) = std::fs::read("/System/Library/Fonts/AppleSDGothicNeo.ttc") {
            let mut fd = egui::FontData::from_owned(bytes);
            fd.index = 0; // Regular face
            fonts.font_data.insert("ui".to_owned(), fd.into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "ui".to_owned());
            tracing::info!("UI 폰트 Apple SD Gothic Neo 등록");
        }
    }

    ctx.set_fonts(fonts);
}
