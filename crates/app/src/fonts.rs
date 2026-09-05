//! egui 기본 폰트에는 한글 글리프가 없다 — 시스템 CJK 폰트를 fallback으로 등록한다.
//! (PR-05 완료 기준: 한글 렌더링 깨짐 없음)

use std::io::Read as _;
use std::path::Path;

/// AppleSDGothicNeo.ttc is currently 55,373,848 bytes; 64 MiB admits it without making arbitrary
/// custom font paths a bulk-file allocation surface.
const FONT_FILE_BYTES_MAX: usize = 64 * 1024 * 1024;
const FONT_PATH_BYTES_MAX: usize = 4 * 1024;
const FONT_NAME_BYTES_MAX: usize = 256;
const FONT_DIRECTORY_ENTRIES_MAX: usize = 4_096;
const UI_FONT_OPTIONS_MAX: usize = 256;

fn open_font_read_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        #[cfg(target_os = "macos")]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0x20_000 | 0x800);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn read_font_file_bounded(path: &Path, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        path.as_os_str().as_encoded_bytes().len() <= FONT_PATH_BYTES_MAX,
        "font_path_bytes_exceeded"
    );
    let before =
        std::fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!("font_metadata_failed"))?;
    anyhow::ensure!(
        before.file_type().is_file() && !before.file_type().is_symlink(),
        "font_file_type_invalid"
    );
    let mut file = open_font_read_only(path).map_err(|_| anyhow::anyhow!("font_open_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("font_metadata_failed"))?;
    anyhow::ensure!(opened.is_file(), "font_file_type_invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "font_file_changed"
        );
    }
    anyhow::ensure!(opened.len() <= max_bytes as u64, "font_bytes_exceeded");
    let probe = max_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("font_bytes_exceeded"))?;
    let initial_capacity = usize::try_from(opened.len()).unwrap_or(probe).min(probe);
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.by_ref()
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("font_read_failed"))?;
    anyhow::ensure!(bytes.len() <= max_bytes, "font_bytes_exceeded");
    let after = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("font_metadata_failed"))?;
    anyhow::ensure!(after.len() == bytes.len() as u64, "font_file_changed");
    Ok(bytes)
}

fn font_path_is_admissible(path: &Path) -> bool {
    if path.as_os_str().as_encoded_bytes().len() > FONT_PATH_BYTES_MAX {
        return false;
    }
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= FONT_FILE_BYTES_MAX as u64
    })
}

/// 번들 터미널 모노 폰트 두 가족 (둘 다 OFL — assets/fonts/*-OFL.txt).
/// - **D2Coding**(기본, 2026-07-13): 네이버 한글 코딩 폰트 — 한글·영문이 한 폰트에서
///   2:1 폭 정합이라 한글 섞인 출력의 정렬이 정확하다. 번들은 Regular만 유지한다.
/// - **JetBrains Mono**: Latin 전용(한글은 CJK 폴백) — 굵기 5단.
///
/// egui는 리가처·가변폰트를 셰이핑/해석하지 않으므로 정적 weight 파일을 번들한다.
const JB_LIGHT: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Light.ttf");
const JB_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const JB_MEDIUM: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf");
const JB_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf");
const JB_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");
const D2_REGULAR: &[u8] = include_bytes!("../assets/fonts/D2Coding-Regular.ttf");

/// 설정에서 고를 수 있는 터미널 모노 폰트 가족.
pub const MONO_FONTS: &[&str] = &["D2Coding", "JetBrainsMono"];
/// 기본 폰트 가족 (2026-07-13 사용자: D2Coding 기본).
pub const DEFAULT_MONO_FONT: &str = "D2Coding";
/// JetBrains Mono 굵기(가는 것부터).
pub const JB_MONO_WEIGHTS: &[&str] = &["Light", "Regular", "Medium", "SemiBold", "Bold"];
/// D2Coding은 실행 파일 중복을 줄이기 위해 Regular만 번들한다.
pub const D2_MONO_WEIGHTS: &[&str] = &["Regular"];
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
        _ => D2_REGULAR,
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

type CachedFontData = (&'static str, std::sync::Arc<egui::FontData>);
static CJK_FONT_DATA: std::sync::OnceLock<Option<CachedFontData>> = std::sync::OnceLock::new();

/// 워크스페이스·세션·파일 트리·하단 내비게이션이 공유하는 좌측 사이드바 전용 가족.
/// 전역 UI 폰트 설정은 유지하고 이 named family를 사이드바 scope에만 적용한다.
pub const SIDEBAR_FONT_FAMILY: &str = "sidebar_apple_sd_gothic";

#[cfg(target_os = "macos")]
const SIDEBAR_FONT_PATH: &str = "/System/Library/Fonts/AppleSDGothicNeo.ttc";

#[cfg(target_os = "macos")]
static SIDEBAR_FONT_DATA: std::sync::OnceLock<Option<CachedFontData>> = std::sync::OnceLock::new();

fn cjk_font_data() -> Option<CachedFontData> {
    CJK_FONT_DATA
        .get_or_init(|| {
            CJK_FONT_CANDIDATES.iter().find_map(|path| {
                read_font_file_bounded(Path::new(path), FONT_FILE_BYTES_MAX)
                    .ok()
                    .map(|bytes| {
                        // 바이트를 leak해 `Cow::Borrowed`로 만든다 — 어차피 이 OnceLock이
                        // 프로세스 수명 동안 붙들고 있던 15MB라 수명은 그대로다. 대신
                        // epaint가 face를 만들 때마다 하던 FontData 통째 복사가 사라지고
                        // (`blob_from_font_data`는 Borrowed를 그대로 Arc에 싣는다), 아래
                        // `cell_matched_cjk`의 격자 정합 사본도 같은 버퍼를 공짜로 쓴다.
                        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                        (
                            *path,
                            std::sync::Arc::new(egui::FontData::from_static(leaked)),
                        )
                    })
            })
        })
        .clone()
}

/// 셀 폭의 기준 문자 — `renderer_egui::cell_size`가 재는 문자와 **같아야** 한다.
const CELL_WIDTH_REFERENCE: char = 'M';
/// 두 칸을 차지하는 기준 문자(한글 음절).
const WIDE_REFERENCE: char = '가';
/// 격자 정합 CJK 폴백의 등록 이름.
const CJK_MONO_FONT: &str = "cjk_mono";

/// 폰트 하나에서 글리프 advance를 em 비율로 잰다(폰트 크기 1.0 = 1em). 글리프가 없으면
/// advance가 0이라 `None`.
///
/// epaint가 레이아웃에서 쓰는 것과 **같은 경로**로 재려고 `FontsImpl`을 직접 만든다 —
/// 별도 폰트 파서로 재면 미묘하게 어긋날 수 있다. 폰트 설치 때만(시작 시 1회 + 설정
/// 변경) 도는 경로이고, 위에서 바이트를 leak해 뒀으므로 복사는 일어나지 않는다.
fn advance_em(font_data: std::sync::Arc<egui::FontData>, ch: char) -> Option<f32> {
    let family = egui::FontFamily::Monospace;
    let mut probe = egui::FontDefinitions::empty();
    probe.font_data.insert("probe".to_owned(), font_data);
    probe
        .families
        .insert(family.clone(), vec!["probe".to_owned()]);
    let mut fonts = egui::epaint::text::FontsImpl::new(Default::default(), probe);
    let width = fonts.font(&family).glyph_width(ch, 1.0);
    (width > 0.0).then_some(width)
}

/// 셀 격자에 맞춘 CJK 폴백 사본. 이미 폭이 맞으면 `None`(사본을 만들지 않는다).
///
/// 모노 폰트가 한글을 갖고 있지 않으면(JetBrains Mono) 한글은 이 폴백 폰트가 그리는데,
/// 두 폰트의 em 폭 비가 다르면 wide 글자가 셀 두 칸을 채우지 못한다 — JetBrains Mono는
/// `M`이 0.6em이라 두 칸이 1.2em인데 AppleGothic `가`는 1.0em이다. 13.5pt에서 2.7px가
/// 모자라고, 그만큼 한글 뒤 글자가 자기 셀에서 밀린다(자간 0으로도 남는 잔여 어긋남).
///
/// 그래서 폴백 페이스를 셀에 맞춰 늘린다 — 실제 터미널(Kitty·Ghostty)이 폴백 페이스를
/// 셀 크기에 맞춰 스케일하는 것과 같은 방식이다. 배율은 **실측**한다: 폴백 후보가
/// 플랫폼마다 다르고(`CJK_FONT_CANDIDATES`) 모노 가족·굵기도 설정에서 바뀌므로 상수로
/// 둘 수 없다.
///
/// 기준은 항상 `FontFamily::Monospace`의 선택 굵기다 — `cell_size`가 그 가족의 `M`으로
/// 셀 폭을 정하기 때문. bold 가족도 같은 사본을 쓰며, bold의 `M`이 같은 폭이라는
/// 전제는 `셀_격자는_한글을_정확히_두_칸으로_담는다`가 가족별로 검증한다.
///
/// D2Coding + AppleGothic은 0.5em × 2 ÷ 1.0em = 1.0이라 사본 없이 원본을 그대로 쓴다.
///
/// 세로 보정(`FontTweak::y_offset*`)은 걸지 않는다 — epaint가 폴백 페이스의 높이 차를
/// 이미 반씩 나눠 중앙에 맞춘다(text_layout.rs:978). em=100 실측으로 한글 베이스라인이
/// 라틴(102) 대비 94 → 100으로 **가까워졌다**(JetBrains Mono + AppleGothic).
fn cell_matched_cjk(
    mono_font: &str,
    mono_weight: &str,
    cjk: &std::sync::Arc<egui::FontData>,
) -> Option<egui::FontData> {
    let mono = std::sync::Arc::new(egui::FontData::from_static(mono_bytes(
        mono_font,
        mono_weight,
    )));
    let cell_em = advance_em(mono, CELL_WIDTH_REFERENCE)?;
    let wide_em = advance_em(cjk.clone(), WIDE_REFERENCE)?;
    let scale = 2.0 * cell_em / wide_em;
    // 배율이 1이면 사본을 만들지 않는다 — 같은 폰트를 두 번 등록하면 epaint가 face를
    // 두 벌 파싱하고 아틀라스도 따로 잡는다.
    (scale.is_finite() && (scale - 1.0).abs() > 1e-4).then(|| egui::FontData {
        font: cjk.font.clone(),
        index: cjk.index,
        tweak: egui::FontTweak {
            scale,
            ..cjk.tweak.clone()
        },
    })
}

#[cfg(target_os = "macos")]
fn sidebar_font_data() -> Option<CachedFontData> {
    SIDEBAR_FONT_DATA
        .get_or_init(|| {
            read_font_file_bounded(Path::new(SIDEBAR_FONT_PATH), FONT_FILE_BYTES_MAX)
                .ok()
                .map(|bytes| {
                    let mut data = egui::FontData::from_owned(bytes);
                    data.index = 0;
                    (SIDEBAR_FONT_PATH, std::sync::Arc::new(data))
                })
        })
        .clone()
}

/// 사이드바 named family가 아직 `ctx`에 등록되지 않았으면(예: `install_cjk_fallback`
/// 실행 전 프레임, 혹은 폰트 설치 없이 위젯만 그리는 kittest 하네스) Proportional로
/// 내려간다. `set_fonts`/`add_font`는 다음 pass 시작 시점에만 반영되므로, 같은 pass
/// 안에서 참조하는 곳은 "미등록이면 폴백"만이 즉시 안전하다.
///
/// # 주의: Context 락 재진입 금지
/// 내부적으로 `ctx.fonts(..)`를 호출하며, egui 0.35의 `ContextImpl`은 비재진입
/// `RwLock`이다(egui-0.35.0/src/context.rs:1031). 이미 `ctx.memory_mut(..)` /
/// `ctx.data_mut(..)` / `ctx.style_mut(..)` 클로저 안에서 이 함수(또는 `sidebar_font`)를
/// 부르면 release 빌드에서 데드락, debug 빌드에서 ~10초 뒤 패닉한다. 그런 클로저
/// 내부에서는 호출하지 말 것.
pub fn sidebar_font_family(ctx: &egui::Context) -> egui::FontFamily {
    let family = egui::FontFamily::Name(SIDEBAR_FONT_FAMILY.into());
    let bound = ctx.fonts(|fonts| fonts.definitions().families.contains_key(&family));
    if bound {
        family
    } else {
        egui::FontFamily::Proportional
    }
}

/// 사이드바 painter/TextEdit에서 같은 Apple SD Gothic named family를 지정한다. 이
/// family가 `ctx`에 등록되지 않은 경우(위 `sidebar_font_family` 참고) Proportional로
/// 내려가 `epaint::Fonts::font()`의 패닉을 피한다.
pub fn sidebar_font(ctx: &egui::Context, size: f32) -> egui::FontId {
    egui::FontId::new(size, sidebar_font_family(ctx))
}

/// Panel 내부 기본 label/button/menu/TextEdit 스타일도 사이드바 가족을 쓰게 한다.
pub fn apply_sidebar_text_styles(ui: &mut egui::Ui) {
    let family = sidebar_font_family(ui.ctx());
    for font_id in ui.style_mut().text_styles.values_mut() {
        font_id.family = family.clone();
    }
}

/// 기본 UI 폰트 경로. 사이드바 전용 가족과 **같은 파일**을 쓴다 — 예전에는 기본 UI가
/// AppleGothic이고 사이드바만 Apple SD Gothic Neo라, 워크스페이스/세션 트리와 그 옆
/// 작업 영역에 한글 서체가 두 벌 나란히 놓였다(2026-08-06 사용자: "설명할 수 없이
/// 불편하다"). AppleGothic은 설정 목록에서 여전히 선택 가능하다.
#[cfg(target_os = "macos")]
const DEFAULT_UI_FONT: &str = SIDEBAR_FONT_PATH;
#[cfg(not(target_os = "macos"))]
const DEFAULT_UI_FONT: &str = "";

#[cfg(target_os = "macos")]
pub const DEFAULT_UI_FONT_NAME: &str = "Apple SD Gothic Neo";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_UI_FONT_NAME: &str = "System";

/// 한글 fallback 폰트를 등록한다. 실패해도 앱은 계속 뜬다 (한글만 깨짐).
/// `ui_font`: 설정에서 고른 UI(Proportional) 폰트 파일 경로 — None/로드 실패면 기본
/// (macOS는 사이드바와 같은 Apple SD Gothic Neo). 설정 변경 시 재호출해 hot
/// reload된다(2026-07-07).
pub fn install_cjk_fallback(
    ctx: &egui::Context,
    ui_font: Option<&str>,
    mono_font: &str,
    mono_weight: &str,
) {
    ctx.set_fonts(build_font_definitions(ui_font, mono_font, mono_weight));
}

fn build_font_definitions(
    ui_font: Option<&str>,
    mono_font: &str,
    mono_weight: &str,
) -> egui::FontDefinitions {
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

    // SGR bold 셀용 굵은 모노 패밀리 (B-1, 2026-07-14) — 렌더러가
    // `terminal::MONO_BOLD_FAMILY` 이름으로 찾는다. D2Coding은 Bold 번들을 제거했으므로
    // Regular를 재사용하고, JetBrains Mono를 고르면 실제 Bold 파일을 사용한다.
    fonts.font_data.insert(
        "term_mono_bold".to_owned(),
        egui::FontData::from_static(mono_bytes(mono_font, "Bold")).into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Name(terminal::MONO_BOLD_FAMILY.into()))
        .or_default()
        .insert(0, "term_mono_bold".to_owned());

    // 한글 fallback (families 끝에 붙여 Latin은 기본/SF, 한글만 이 폰트가 처리)
    let mut cjk_font_path = None;
    if let Some((path, font_data)) = cjk_font_data() {
        cjk_font_path = Some(path);
        // 터미널 두 가족(모노·bold)은 셀 격자에 맞춘 사본을 쓴다. UI(Proportional)는
        // 격자가 없으므로 원본 그대로 — 배율을 공유하면 사이드바 한글만 커진다.
        let mono_cjk = match cell_matched_cjk(mono_font, mono_weight, &font_data) {
            Some(matched) => {
                fonts
                    .font_data
                    .insert(CJK_MONO_FONT.to_owned(), matched.into());
                CJK_MONO_FONT
            }
            None => "cjk",
        };
        fonts.font_data.insert("cjk".to_owned(), font_data);
        for (family, name) in [
            (egui::FontFamily::Monospace, mono_cjk),
            (egui::FontFamily::Proportional, "cjk"),
            // bold 셀도 한글이 깨지지 않게 같은 폴백을 붙인다 (B-1).
            (
                egui::FontFamily::Name(terminal::MONO_BOLD_FAMILY.into()),
                mono_cjk,
            ),
        ] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(name.to_owned());
        }
        tracing::info!(kind = "cjk_fallback", "font registered");
    } else {
        tracing::warn!("한글 폰트를 찾지 못함 — 한글이 깨질 수 있음");
    }

    // 좌측 사이드바 전용 Apple SD Gothic Neo. 시스템 폰트를 실행 파일에 포함하지 않고
    // 시작 시 한 번만 읽어 Arc로 재사용한다. **UI 후보 루프보다 먼저** 등록해야 한다 —
    // macOS 기본 UI 폰트가 같은 55MB .ttc라, 루프가 이 항목을 그대로 재사용하지 않으면
    // 원본 바이트와 skrifa Font가 영구 중복된다(아래 `cjk` 재사용과 같은 이유).
    #[cfg(target_os = "macos")]
    let sidebar_font_path = sidebar_font_data().map(|(path, font_data)| {
        fonts
            .font_data
            .insert(SIDEBAR_FONT_FAMILY.to_owned(), font_data);
        tracing::info!(kind = "sidebar_font", "font registered");
        path
    });
    #[cfg(not(target_os = "macos"))]
    let sidebar_font_path: Option<&'static str> = None;

    // UI(Proportional) 기본 폰트 — 설정 폰트 > 기본(macOS: Apple SD Gothic Neo) 순으로
    // 시도. SFNS.ttf(SF Pro)는 fvar 가변폰트라 egui/skrifa가 무시했다(2026-07-06) —
    // .ttc는 index로 로드된다. 모노(터미널)는 위에서 번들 JetBrains Mono + CJK fallback.
    let ui_candidates = [ui_font.unwrap_or_default(), DEFAULT_UI_FONT];
    for path in ui_candidates.iter().filter(|p| !p.is_empty()) {
        if sidebar_font_path == Some(*path) {
            // 기본 macOS 구성 — UI와 사이드바가 같은 Apple SD Gothic Neo다.
            let family = fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default();
            family.retain(|name| name != SIDEBAR_FONT_FAMILY);
            family.insert(0, SIDEBAR_FONT_FAMILY.to_owned());
            tracing::info!(kind = "ui_shared_sidebar", "font registered");
            break;
        }
        if cjk_font_path == Some(*path) {
            // 설정에서 AppleGothic을 고르면 UI와 CJK fallback이 같은 15MB 파일이 된다.
            // 같은 파일을 `ui`라는 별도 FontData로 다시 읽고 파싱하면 원본 바이트와
            // skrifa Font가 영구 중복된다. 기존 `cjk` 항목을 UI의 첫 후보로 재사용한다.
            let family = fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default();
            family.retain(|name| name != "cjk");
            family.insert(0, "cjk".to_owned());
            tracing::info!(kind = "ui_shared_cjk", "font registered");
            break;
        }
        let Ok(bytes) = read_font_file_bounded(Path::new(path), FONT_FILE_BYTES_MAX) else {
            tracing::warn!(
                kind = "ui_font",
                phase = "admission",
                error_code = "font_unavailable",
                "font candidate unavailable"
            );
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
        tracing::info!(kind = "ui_font", "font registered");
        break;
    }

    // 사이드바 named family. 폰트 데이터는 위에서 이미 등록했고, 여기서는 가족 순서만
    // 정한다. 비-macOS/로드 실패에서는 현재 UI Proportional 가족을 그대로 복제해
    // named family가 항상 해석되게 한다.
    let sidebar_family = egui::FontFamily::Name(SIDEBAR_FONT_FAMILY.into());
    let mut sidebar_fallback = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if sidebar_font_path.is_some() {
        sidebar_fallback.retain(|name| name != SIDEBAR_FONT_FAMILY);
        sidebar_fallback.insert(0, SIDEBAR_FONT_FAMILY.to_owned());
    }
    fonts.families.insert(sidebar_family, sidebar_fallback);

    fonts
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
        if !path.is_empty() && font_path_is_admissible(Path::new(path)) {
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
    if let Some(home) = crate::paths::home_dir() {
        dirs.push(home.join("Library/Fonts"));
    }
    let mut scanned_entries = 0usize;
    'directories: for dir in dirs {
        if !std::fs::symlink_metadata(&dir).is_ok_and(|metadata| {
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
        }) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd {
            if scanned_entries >= FONT_DIRECTORY_ENTRIES_MAX || out.len() >= UI_FONT_OPTIONS_MAX {
                break 'directories;
            }
            scanned_entries += 1;
            let Ok(e) = entry else {
                continue;
            };
            if !e.file_type().is_ok_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = e.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.len() > FONT_NAME_BYTES_MAX || !font_path_is_admissible(&path) {
                continue;
            }
            let ext_ok = path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| {
                    matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc")
                });
            let lower = stem.to_ascii_lowercase();
            if ext_ok && KOREAN_MARKERS.iter().any(|m| lower.contains(m)) {
                let Some(p) = path.to_str().map(str::to_owned) else {
                    continue;
                };
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

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-font-bound-{tag}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn font_reader_accepts_exact_and_rejects_plus_one() {
        let path = temp_path("bytes");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        assert_eq!(super::read_font_file_bounded(&path, 64).unwrap().len(), 64);
        std::fs::write(&path, vec![b'x'; 65]).unwrap();
        assert_eq!(
            super::read_font_file_bounded(&path, 64)
                .unwrap_err()
                .to_string(),
            "font_bytes_exceeded"
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn font_reader_rejects_symlink_and_special_file() {
        use std::os::unix::fs::symlink;

        let dir = temp_path("types");
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        let link = dir.join("link");
        std::fs::write(&regular, b"font").unwrap();
        symlink(&regular, &link).unwrap();
        assert_eq!(
            super::read_font_file_bounded(&link, 64)
                .unwrap_err()
                .to_string(),
            "font_file_type_invalid"
        );
        assert_eq!(
            super::read_font_file_bounded(std::path::Path::new("/dev/null"), 64)
                .unwrap_err()
                .to_string(),
            "font_file_type_invalid"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_font_admission_has_no_unbounded_file_materialization() {
        let production = include_str!("fonts.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("std::fs::read("));
        assert!(!production.contains("read_to_string"));
        assert!(production.contains(".take(probe as u64)"));
    }

    #[test]
    fn d2coding은_regular만_번들하고_bold요청도_regular로_폴백한다() {
        assert_eq!(super::mono_weights_for("D2Coding"), &["Regular"]);
        assert!(std::ptr::eq(
            super::mono_bytes("D2Coding", "Regular"),
            super::mono_bytes("D2Coding", "Bold")
        ));
    }

    /// 갤리 한 줄의 실제 진행 폭(advance 합). `Fonts::glyph_width`가 아니라 이걸 재야
    /// 하는 이유는 아래 격자 테스트 주석 참고.
    fn galley_advance(ctx: &egui::Context, font_id: &egui::FontId, text: &str) -> f32 {
        ctx.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(text.to_owned(), font_id.clone(), egui::Color32::WHITE)
                .rect
                .width()
        })
    }

    /// 터미널 격자의 불변식: **좁은 글자는 정확히 1칸, 한글·CJK는 정확히 2칸을 채운다.**
    ///
    /// 깨지는 경로는 둘이다.
    /// 1. **자간**: 자간 `s`는 글자 뒤에 붙는 여백이라 wide 글자는 `2M + s`밖에 못
    ///    채우는데 상자는 `2M + 2s`여서, 한글끼리만 간격이 라틴의 두 배가 된다
    ///    (2026-09-03 사용자 신고: grok 한글이 글자마다 벌어짐).
    /// 2. **폴백 폰트 폭**: 한글이 없는 모노(JetBrains Mono)를 고르면 한글은 시스템 CJK
    ///    폰트가 그리는데, em 폭 비가 다르면 두 칸을 못 채운다. `cell_matched_cjk`가
    ///    폴백을 셀에 맞춰 스케일해 막는다.
    ///
    /// 재는 값이 `Fonts::glyph_width`가 아니라 **갤리 폭**인 이유: epaint 0.35의
    /// `glyph_width`는 `FontTweak::scale`을 곱하지 않는 반면(font.rs:707) 레이아웃은
    /// face별 `styled_metrics`로 반영한다(text_layout.rs:324). 즉 glyph_width로 재면
    /// 격자 정합 사본의 진짜 advance를 못 본다. 렌더러가 그리는 것도 갤리다.
    ///
    /// 번들 가족 × 굵기 전부를, 모노 가족과 SGR bold 가족 양쪽에서, 쓰는 크기 전
    /// 구간에서 확인한다.
    #[test]
    fn 셀_격자는_한글을_정확히_두_칸으로_담는다() {
        use terminal::renderer_egui::{CellMetrics, cell_size};

        /// 한 글자만 재면 폰트가 아니라 반올림을 재게 된다 — 여러 글자를 이어 붙여
        /// 글자당 어긋남이 누적되게 한다(한 run 안에서 실제로 밀리는 방식 그대로).
        const RUN_LEN: usize = 8;
        /// epaint는 갤리 폭을 GUI 격자(1/32pt)로 한 번 반올림한다 — 길이와 무관한
        /// 상수 오차다. 신고된 결함은 이 값의 400배(11pt에서 2.2px×8)라 놓치지 않는다.
        const ROUNDING_SLACK: f32 = 0.05;

        for family in super::MONO_FONTS {
            for weight in super::mono_weights_for(family) {
                let ctx = egui::Context::default();
                ctx.set_fonts(super::build_font_definitions(None, family, weight));
                let _ = ctx.run_ui(egui::RawInput::default(), |_| {});

                // 한글을 그릴 폰트가 아예 없는 머신(시스템 CJK 폰트 미설치 CI)에서는
                // 검증할 대상 자체가 없다 — 없는 것을 실패로 만들지 않고 건너뛴다.
                // D2Coding은 한글을 자체 커버하므로 폴백 없이도 검증된다.
                let mono = std::sync::Arc::new(egui::FontData::from_static(super::mono_bytes(
                    family, weight,
                )));
                if super::advance_em(mono, super::WIDE_REFERENCE).is_none()
                    && super::cjk_font_data().is_none()
                {
                    eprintln!("{family}/{weight}: 한글 폰트가 없어 격자 검증을 건너뜀");
                    continue;
                }

                for font_size in [11.0_f32, 13.5, 16.0, 20.0] {
                    let cell = cell_size(
                        &ctx,
                        CellMetrics {
                            font_size,
                            line_height: 1.0,
                        },
                    );
                    // bold 셀은 다른 가족으로 레이아웃되지만(renderer_egui의
                    // `layout_attr_text`) 셀 폭은 Monospace의 `M` 하나로 정해진다 —
                    // 두 가족의 `M`이 어긋나면 bold run 전체가 셀에서 밀린다.
                    for text_family in [
                        egui::FontFamily::Monospace,
                        egui::FontFamily::Name(terminal::MONO_BOLD_FAMILY.into()),
                    ] {
                        let font_id = egui::FontId::new(font_size, text_family.clone());
                        let narrow = galley_advance(&ctx, &font_id, &"M".repeat(RUN_LEN));
                        let wide = galley_advance(&ctx, &font_id, &"가".repeat(RUN_LEN));
                        let cells = RUN_LEN as f32;
                        assert!(
                            (narrow - cell.x * cells).abs() < ROUNDING_SLACK,
                            "{family}/{weight} {text_family} @{font_size}: \
좁은 글자 {RUN_LEN}개 폭 {narrow}가 {RUN_LEN}칸 {}와 다르다",
                            cell.x * cells
                        );
                        assert!(
                            (wide - cell.x * 2.0 * cells).abs() < ROUNDING_SLACK,
                            "{family}/{weight} {text_family} @{font_size}: \
한글 {RUN_LEN}개 폭 {wide}가 {}칸 {}와 다르다 — 자간이 섞였거나 CJK 폴백 폭이 셀에 맞지 않다",
                            RUN_LEN * 2,
                            cell.x * 2.0 * cells
                        );
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn 기본_macos_ui와_사이드바는_같은_폰트를_공유하고_중복하지_않는다() {
        let fonts = super::build_font_definitions(
            None,
            super::DEFAULT_MONO_FONT,
            super::DEFAULT_MONO_WEIGHT,
        );
        assert!(fonts.font_data.contains_key("cjk"));
        assert!(
            !fonts.font_data.contains_key("ui"),
            "기본 UI와 사이드바가 같은 Apple SD Gothic Neo인데 별도 원본을 보관하면 안 됨"
        );
        // 워크스페이스/세션 트리와 그 옆 작업 영역이 같은 한글 서체를 쓰는지가 핵심 —
        // 두 벌이 나란히 놓이면 "설명할 수 없이 불편한" 화면이 된다(2026-08-06).
        let proportional = &fonts.families[&egui::FontFamily::Proportional];
        assert_eq!(
            proportional.first().map(String::as_str),
            Some(super::SIDEBAR_FONT_FAMILY)
        );
        assert_eq!(
            proportional
                .iter()
                .filter(|name| *name == super::SIDEBAR_FONT_FAMILY)
                .count(),
            1
        );
        assert_eq!(proportional.iter().filter(|name| *name == "cjk").count(), 1);

        let rebuilt = super::build_font_definitions(
            None,
            super::DEFAULT_MONO_FONT,
            super::DEFAULT_MONO_WEIGHT,
        );
        assert!(std::sync::Arc::ptr_eq(
            &fonts.font_data["cjk"],
            &rebuilt.font_data["cjk"]
        ));

        let sidebar = egui::FontFamily::Name(super::SIDEBAR_FONT_FAMILY.into());
        assert_eq!(
            fonts.families[&sidebar].first().map(String::as_str),
            Some("sidebar_apple_sd_gothic")
        );
        assert!(std::sync::Arc::ptr_eq(
            &fonts.font_data["sidebar_apple_sd_gothic"],
            &rebuilt.font_data["sidebar_apple_sd_gothic"]
        ));
    }

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
