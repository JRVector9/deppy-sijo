//! egui 기본 폰트에는 한글 글리프가 없다 — 시스템 CJK 폰트를 fallback으로 등록한다.
//! (PR-05 완료 기준: 한글 렌더링 깨짐 없음)

/// 번들 터미널 모노 폰트 두 가족 (둘 다 OFL — assets/fonts/*-OFL.txt).
/// - **D2Coding**(기본, 2026-07-13): 네이버 한글 코딩 폰트 — 한글·영문이 한 폰트에서
///   2:1 폭 정합이라 한글 섞인 출력의 정렬이 정확하다. 굵기는 Regular/Bold 2단.
/// - **JetBrains Mono**: Latin 전용(한글은 CJK 폴백) — 굵기 5단.
/// egui는 리가처·가변폰트를 셰이핑/해석하지 않으므로 정적 weight 파일을 번들한다.
const JB_LIGHT: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Light.ttf");
const JB_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const JB_MEDIUM: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf");
const JB_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf");
const JB_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");
const D2_REGULAR: &[u8] = include_bytes!("../assets/fonts/D2Coding-Regular.ttf");
const D2_BOLD: &[u8] = include_bytes!("../assets/fonts/D2Coding-Bold.ttf");

/// 설정에서 고를 수 있는 터미널 모노 폰트 가족.
pub const MONO_FONTS: &[&str] = &["D2Coding", "JetBrainsMono"];
/// 기본 폰트 가족 (2026-07-13 사용자: D2Coding 기본).
pub const DEFAULT_MONO_FONT: &str = "D2Coding";
/// JetBrains Mono 굵기(가는 것부터).
pub const JB_MONO_WEIGHTS: &[&str] = &["Light", "Regular", "Medium", "SemiBold", "Bold"];
/// D2Coding 굵기 — 원본이 Regular/Bold 2단만 제공한다.
pub const D2_MONO_WEIGHTS: &[&str] = &["Regular", "Bold"];
/// 기본 굵기.
pub const DEFAULT_MONO_WEIGHT: &str = "Regular";

/// 폰트 가족이 지원하는 굵기 목록 — 설정 UI/검증 공용.
pub fn mono_weights_for(font: &str) -> &'static [&'static str] {
    match font {
        "JetBrainsMono" => JB_MONO_WEIGHTS,
        _ => D2_MONO_WEIGHTS,
    }
}

/// (가족, 굵기) → 번들 폰트 바이트. 미지값은 해당 가족 Regular로 폴백.
fn mono_bytes(font: &str, weight: &str) -> &'static [u8] {
    match font {
        "JetBrainsMono" => match weight {
            "Light" => JB_LIGHT,
            "Medium" => JB_MEDIUM,
            "SemiBold" => JB_SEMIBOLD,
            "Bold" => JB_BOLD,
            _ => JB_REGULAR,
        },
        _ => match weight {
            "Bold" => D2_BOLD,
            _ => D2_REGULAR,
        },
    }
}

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

/// 기본 UI 폰트 경로 (macOS — AppleGothic, 사용자 선호 2026-07-08. 이전 기본은
/// Apple SD Gothic Neo였고 목록에서 여전히 선택 가능).
#[cfg(target_os = "macos")]
const DEFAULT_UI_FONT: &str = "/System/Library/Fonts/Supplemental/AppleGothic.ttf";
#[cfg(not(target_os = "macos"))]
const DEFAULT_UI_FONT: &str = "";

#[cfg(target_os = "macos")]
pub const DEFAULT_UI_FONT_NAME: &str = "AppleGothic";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_UI_FONT_NAME: &str = "System";

/// 한글 fallback 폰트를 등록한다. 실패해도 앱은 계속 뜬다 (한글만 깨짐).
/// `ui_font`: 설정에서 고른 UI(Proportional) 폰트 파일 경로 — None/로드 실패면 기본
/// (macOS는 AppleGothic). 설정 변경 시 재호출해 hot reload된다(2026-07-07).
pub fn install_cjk_fallback(
    ctx: &egui::Context,
    ui_font: Option<&str>,
    mono_font: &str,
    mono_weight: &str,
) {
    let mut fonts = egui::FontDefinitions::default();

    // 터미널 모노 = 번들 폰트(설정 가족+굵기). Monospace 패밀리 **맨 앞**에 넣어 egui
    // 기본 Hack 대신 쓴다. D2Coding은 한글까지 자체 커버(2:1 폭 정합)하고, JetBrains
    // Mono는 Latin만 — 한글은 아래 CJK 폴백이 처리한다(D2Coding도 미보유 한자 등은 폴백).
    fonts.font_data.insert(
        "term_mono".to_owned(),
        egui::FontData::from_static(mono_bytes(mono_font, mono_weight)).into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "term_mono".to_owned());

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

    // UI(Proportional) 기본 폰트 — 설정 폰트 > 기본(macOS: Apple SD Gothic Neo) 순으로
    // 시도. SFNS.ttf(SF Pro)는 fvar 가변폰트라 egui/skrifa가 무시했다(2026-07-06) —
    // .ttc는 index로 로드된다. 모노(터미널)는 위에서 번들 JetBrains Mono + CJK fallback.
    let ui_candidates = [ui_font.unwrap_or_default(), DEFAULT_UI_FONT];
    for path in ui_candidates.iter().filter(|p| !p.is_empty()) {
        let Ok(bytes) = std::fs::read(path) else {
            tracing::warn!(font = path, "UI 폰트 로드 실패 — 다음 후보로");
            continue;
        };
        let mut fd = egui::FontData::from_owned(bytes);
        fd.index = 0; // .ttc의 첫 face (Regular)
        fonts.font_data.insert("ui".to_owned(), fd.into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".to_owned());
        tracing::info!(font = path, "UI 폰트 등록");
        break;
    }

    ctx.set_fonts(fonts);
}

/// 설정에서 고를 수 있는 UI 폰트 목록 — (표시명, 파일 경로). 시스템에 실제 설치되어
/// 있고 한글을 지원할 법한 폰트만: 고정 후보 + 폰트 디렉터리의 한글 폰트 이름 스캔.
/// 파일 IO라 호출부에서 캐시할 것(설정 창 열 때 1회).
pub fn ui_font_options() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // 고정 후보 (macOS 기본 한글 폰트)
    for (name, path) in [
        (
            "AppleGothic",
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
        ),
        (
            "Apple SD Gothic Neo",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        ),
    ] {
        if !path.is_empty() && std::path::Path::new(path).is_file() {
            out.push((name.to_owned(), path.to_owned()));
        }
    }
    // 사용자/시스템 폰트 디렉터리에서 한글 폰트로 보이는 파일 스캔 (이름 휴리스틱).
    const KOREAN_MARKERS: &[&str] = &[
        "nanum",
        "pretendard",
        "gothic",
        "spoqa",
        "d2coding",
        "kopub",
        "batang",
        "dotum",
        "malgun",
        "gulim",
        "kr",
    ];
    let mut dirs = vec![std::path::PathBuf::from("/Library/Fonts")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join("Library/Fonts"));
    }
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let ext_ok = path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| {
                    matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc")
                });
            let lower = stem.to_ascii_lowercase();
            if ext_ok && KOREAN_MARKERS.iter().any(|m| lower.contains(m)) {
                let p = path.display().to_string();
                if !out.iter().any(|(_, existing)| existing == &p) {
                    out.push((stem.to_owned(), p));
                }
            }
        }
    }
    out
}

/// 설정 화면에 표시할 실제 UI 폰트명. `None`은 단순히 "자동"으로 숨기지 않고 현재
/// 플랫폼 기본값을 표시해, 사용자가 지금 적용된 폰트를 즉시 확인할 수 있게 한다.
pub fn effective_ui_font_name(selected_path: Option<&str>, options: &[(String, String)]) -> String {
    let Some(selected_path) = selected_path else {
        return DEFAULT_UI_FONT_NAME.to_owned();
    };
    options
        .iter()
        .find(|(_, path)| path == selected_path)
        .map(|(name, _)| name.clone())
        .or_else(|| {
            std::path::Path::new(selected_path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| DEFAULT_UI_FONT_NAME.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_UI_FONT_NAME, effective_ui_font_name};

    #[test]
    fn 기본_ui_font는_실제_플랫폼_폰트명을_표시한다() {
        assert_eq!(effective_ui_font_name(None, &[]), DEFAULT_UI_FONT_NAME);
    }

    #[test]
    fn 선택한_ui_font는_options의_표시명을_사용한다() {
        let options = vec![("Pretendard".to_owned(), "/fonts/p.ttf".to_owned())];
        assert_eq!(
            effective_ui_font_name(Some("/fonts/p.ttf"), &options),
            "Pretendard"
        );
    }
}
