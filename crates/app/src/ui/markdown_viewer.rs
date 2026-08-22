//! Deppy가 소유하는 Markdown 뷰어 facade.
//!
//! 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md` §5,
//! `docs/document-editor-lightweight-core-plan.md` §6(Viewer 계약)·§7(보안 정책).
//!
//! 앱의 다른 코드가 `egui_commonmark` 타입에 직접 의존하지 않게 감싼다 — 나중에
//! 갈아끼울 수 있어야 한다. leaf이므로 intent만 올린다(파일·URL을 직접 열지 않는다).
//!
//! ## 경계 요약
//! - `egui_commonmark::CommonMarkViewer`/`CommonMarkCache`는 이 파일 밖으로 나가지
//!   않는다. 공개 타입은 [`MarkdownViewer`], [`MarkdownViewerContext`],
//!   [`MarkdownLinkIntent`], [`MarkdownDocumentSlot`], [`MarkdownSourceRevision`]뿐이다.
//! - 로컬 이미지는 [`show`](MarkdownViewer::show) 안에서 워크스페이스 루트 밖 탈출을
//!   막고 나서 직접 읽는다(§7.2) — 이건 App host의 intent 왕복 없이 leaf가 소유하는
//!   "렌더에 필요한 바이트 읽기"이지, "다른 문서를 연다"류의 탐색 액션이 아니다.
//! - 링크 클릭·상대 `.md` 문서 열기처럼 **탐색을 일으키는 동작**은 절대 여기서
//!   처리하지 않는다. `show`는 클릭된 링크의 [`MarkdownLinkIntent`]만 돌려주고,
//!   실제로 브라우저를 열거나 다른 문서를 로드하는 일은 호출부(App host) 몫이다.

use std::path::Path;

use egui_commonmark::{Alert, AlertBundle, CommonMarkCache, CommonMarkViewer};

use crate::ui::designall;

// ── 페이지 디자인 상수 (§6.2, 2026-08-22 앱 통일성 재조정) ──────────────────
// 터미널 11pt · 사이드바 12-13pt인 앱 안에서 문서 표면만 웹 아티클 스케일(본문
// 16pt · 폭 880px · 헤딩 30pt 단일)을 그대로 써서 "다른 앱처럼" 보였다(2026-08-22
// 사용자 스크린샷 지적). 본문·폭·여백을 앱 스케일로 낮춘다. 좁은 pane에서는
// `min`으로 자연스럽게 줄어든다(하한을 강제로 두지 않는다 — 좁은 보조 탭에서
// 억지로 폭을 넓히면 잘림만 는다).
const PAGE_MAX_CONTENT_WIDTH: f32 = 640.0;
const PAGE_PADDING_X: i8 = 26;
const PAGE_PADDING_Y: i8 = 22;
/// 본문 크기. 터미널 11pt·사이드바 12~13pt와 같은 계열로 둔다 — 이 값이 헤딩 위계의
/// **아래쪽 기준점**이기도 해서, 낮출수록 헤딩 단계 간격이 벌어진다(아래 참고).
const PAGE_BODY_FONT_SIZE: f32 = 11.5;
/// H1의 실제 렌더 크기. `apply_page_style`가 `TextStyle::Heading`에 심는 값이고,
/// egui_commonmark는 H1(레벨 0)에서 이 값을 그대로 쓴다(보간 없음).
///
/// **왜 이 두 상수가 함께 정해지는가**(2026-08-22): egui_commonmark_backend 0.24는
/// 헤딩 크기를 `본문 + (헤딩 − 본문) × 고정비율`(H2=0.835, H3=0.668, …)로 계산하고
/// 그 비율을 바꿀 공개 API가 없다. 즉 우리가 돌릴 수 있는 손잡이는 이 두 개뿐이고,
/// **단계 간격은 (헤딩 − 본문)에 비례**한다.
///
/// 본문 13 / H1 19이던 시절엔 간격이 1pt라 H1~H4가 전부 bold인 상태에서 사실상
/// 구분되지 않았다(사용자 보고: "H1과 H2가 같아 보인다"). 본문을 11.5로 낮추고 H1을
/// 23으로 올려 간격을 1.9pt로 벌린다 — H1 23 / H2 21.1 / H3 19.2 / H4 17.3.
///
/// 계산식 자체를 바꾸려면 포크(`egui_commonmark_extended`, 다운로드 737회)나 자체
/// 패치가 필요한데, 미관 문제에 안정성을 남에게 맡기는 거래라 택하지 않았다.
const PAGE_HEADING_FONT_SIZE: f32 = 23.0;
/// 문단·리스트·인용 사이 세로 리듬 — egui 기본 item_spacing.y(4px 안팎)보다 넉넉하게.
const PAGE_ITEM_SPACING_Y: f32 = 10.0;
/// 캐시 무효화 세분도용 폭 버킷 크기 — 이보다 작은 리사이즈는 같은 버킷으로 묶여
/// 재캐시를 트리거하지 않는다(§6.4 WidthBucket).
const CONTENT_WIDTH_BUCKET_PX: f32 = 40.0;

// ── 로컬 이미지 broker 상한 (§7.2) ──────────────────────────────────────────
/// 인코딩된 PNG 바이트 상한 — 문서 탭 설계 §6의 8 MiB Refuse 티어와 같은 기준을
/// 재사용한다(별도 숫자를 새로 정의하지 않는다).
const MAX_IMAGE_ENCODED_BYTES: u64 = 8 * 1024 * 1024;
/// 디코드된 한 변 상한(px) — 가늘고 긴 픽셀폭탄 PNG(예: 1×500000)을 차단한다.
const MAX_IMAGE_DIMENSION_PX: u32 = 6000;
/// 디코드된 총 픽셀 수 상한 — RGBA 기준 대략 64MB 텍스처로 수렴한다.
const MAX_IMAGE_PIXELS: u64 = 16_000_000;

/// 문서 탭 하나를 식별하는 opaque 슬롯. 실제 `DocumentId`(문서 I/O 쪽 소유)와의
/// 결합은 tab 배선 몫이라 여기서는 호출부가 안정적으로 배정하는 `u64`로만 다룬다 —
/// 이 leaf는 다른 진행 중인 작업(`document_io.rs`)의 타입에 의존하지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkdownDocumentSlot(pub u64);

/// source 내용이 바뀔 때마다 호출부가 올리는 리비전. 캐시 무효화 키의 일부(§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkdownSourceRevision(pub u64);

/// [`MarkdownViewer::show`] 한 번의 호출 컨텍스트.
pub struct MarkdownViewerContext<'a> {
    /// 문서 탭 식별자 — 캐시 키와 이미지 broker 세대 계산에 쓴다.
    pub slot: MarkdownDocumentSlot,
    /// source 리비전 — 바뀌면 scrollable 캐시와 이미지 등록을 다시 만든다.
    pub revision: MarkdownSourceRevision,
    /// 워크스페이스 루트(canonical 여부는 broker가 스스로 보장한다) — 로컬 이미지가
    /// 이 밖으로 나가지 못하게 막는 경계.
    pub workspace_root: &'a Path,
    /// 이 문서 파일이 있는 디렉터리 — 상대 이미지 경로의 기준(§7.2).
    pub base_directory: &'a Path,
}

/// Markdown 안 링크를 클릭했을 때 leaf가 올리는 intent. 여기서 브라우저를 열거나
/// 다른 문서를 로드하지 않는다 — 호출부가 스킴별로 실제 동작을 수행한다(§7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownLinkIntent {
    /// `http`/`https` 링크 — 호출부가 기존 외부 열기 경로로 처리한다.
    OpenExternal(String),
    /// 스킴이 없는 상대 경로 — 호출부가 document-open action seam으로 넘긴다.
    OpenRelativeDocument(String),
    /// 화이트리스트 밖 스킴(`javascript:`, `data:`, 그 외 커스텀 스킴) — 열지 않는다.
    /// 호출부가 무시하거나 명시적으로 표시할지 결정한다.
    Rejected(String),
}

/// scrollable 캐시(§6.4) 무효화 키. 문서·리비전·테마·폭 버킷 중 하나라도 바뀌면
/// 새 키가 되어 이전 캐시 항목을 지우고 다시 만든다. 그대로면 `show_scrollable`이
/// 내부에 들고 있는 static-content 캐시가 그대로 재사용된다(재파싱하지 않는다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ScrollCacheKey {
    slot: u64,
    revision: u64,
    dark_mode: bool,
    width_bucket: u32,
}

fn width_bucket(available_width: f32) -> u32 {
    (available_width.max(0.0) / CONTENT_WIDTH_BUCKET_PX).floor() as u32
}

/// 로컬 PNG 이미지 검증 실패 사유. 등록을 생략하는 이유일 뿐 — 실패해도 렌더는
/// 멈추지 않는다(§7.2: "decode 실패는 page 안 placeholder로 표시하고 render를
/// 중단하지 않는다"). egui가 loader 없음으로 처리해 알아서 실패 placeholder를 그린다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageRejection {
    AbsolutePath,
    WrongExtension,
    OutsideWorkspace,
    NotARegularFile,
    TooLarge,
    ReadFailed,
    DecodeRejected,
}

/// 워크스페이스 루트 밖 파일을 로컬 이미지로 읽지 않는다(§7.2, 이 PR의 최우선 테스트
/// 대상). 허용 조건: 스킴 없는 상대경로 + `.png` 확장자 + canonicalize 후 워크스페이스
/// 루트 안 + 일반 파일 + 인코딩 바이트 상한 + 디코드 치수/픽셀수 상한.
///
/// `canonicalize`는 symlink 체인을 전부 실제 경로로 풀어주므로 `../..` 탈출과
/// symlink 탈출을 같은 검사(`starts_with`)로 동시에 막는다 — app.rs의
/// `run_file_tree_listing`이 파일 트리 사이드바에 쓰는 것과 같은 패턴이다.
fn validate_and_read(
    workspace_root: &Path,
    base_directory: &Path,
    relative: &str,
) -> Result<Vec<u8>, ImageRejection> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err(ImageRejection::AbsolutePath);
    }
    let has_png_extension = candidate
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    if !has_png_extension {
        return Err(ImageRejection::WrongExtension);
    }
    let canonical_root =
        std::fs::canonicalize(workspace_root).map_err(|_| ImageRejection::OutsideWorkspace)?;
    let joined = base_directory.join(candidate);
    let canonical_target =
        std::fs::canonicalize(&joined).map_err(|_| ImageRejection::OutsideWorkspace)?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(ImageRejection::OutsideWorkspace);
    }
    let metadata = std::fs::metadata(&canonical_target).map_err(|_| ImageRejection::ReadFailed)?;
    if !metadata.is_file() {
        // 심볼릭 링크 자체는 canonicalize가 이미 실제 파일로 풀었으므로 여기 남는
        // is_file() == false는 디렉터리·소켓·FIFO 같은 special file이다.
        return Err(ImageRejection::NotARegularFile);
    }
    if metadata.len() > MAX_IMAGE_ENCODED_BYTES {
        return Err(ImageRejection::TooLarge);
    }
    let bytes = std::fs::read(&canonical_target).map_err(|_| ImageRejection::ReadFailed)?;
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes.as_slice()))
        .with_guessed_format()
        .map_err(|_| ImageRejection::DecodeRejected)?
        .into_dimensions()
        .map_err(|_| ImageRejection::DecodeRejected)?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION_PX
        || height > MAX_IMAGE_DIMENSION_PX
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(ImageRejection::DecodeRejected);
    }
    Ok(bytes)
}

/// egui_commonmark가 `enable_scroll_to_heading(true)`일 때 켜는 것과 같은
/// pulldown-cmark 옵션(업스트림 `egui_commonmark_backend::pulldown::parser_options`
/// 실측: 테이블/작업목록/취소선/각주/정의목록 + heading 속성). math는 켜지 않는다 —
/// 이 Viewer는 `render_math_fn`을 설정하지 않으므로 업스트림도 `ENABLE_MATH`를
/// 켜지 않는다.
fn destination_parser_options() -> pulldown_cmark::Options {
    pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_TASKLISTS
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_FOOTNOTES
        | pulldown_cmark::Options::ENABLE_DEFINITION_LIST
        | pulldown_cmark::Options::ENABLE_HEADING_ATTRIBUTES
}

/// 이미지·링크 목적지를 뽑는다: `(이미지 dest 목록, 링크 dest 목록)`.
///
/// 정규식으로 흉내 내지 않고 egui_commonmark(0.24)가 실제로 쓰는 것과 같은
/// pulldown-cmark 파서/옵션으로 직접 파싱한다. 이유: CommonMark의 링크 목적지는
/// 이스케이프 없이 균형 잡힌 괄호를 한 단계 허용한다(`[x](javascript:alert(1))`).
/// 정규식이 이 규칙을 놓치면 뽑아낸 문자열이 실제 렌더러가 쓰는 destination과
/// 달라지고, 그러면 위험한 링크가 [`WorkspaceImageBroker`]/link hook 등록을 비껴가
/// 라이브러리 기본 `hyperlink_to`(OS URL 열기)로 새 나간다 — §7.3이 실제로 걸리려면
/// 파서가 반드시 일치해야 한다.
fn extract_destinations(source: &str) -> (Vec<String>, Vec<String>) {
    let mut image_refs = Vec::new();
    let mut link_refs = Vec::new();
    let parser = pulldown_cmark::Parser::new_ext(source, destination_parser_options());
    for event in parser {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) => {
                let dest = dest_url.to_string();
                // 스킴이 있으면(원격 URL, data URI) broker 대상이 아니다 — 이번
                // 범위에서 로드하지 않는다(§7.2). default_implicit_uri_scheme
                // 접두사도 스킴이 있는 dest에는 붙지 않으므로 등록해 봐야 URI가
                // 어긋나 의미가 없다.
                if !dest.contains("://") && !dest.starts_with("data:") {
                    image_refs.push(dest);
                }
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) => {
                let dest = dest_url.to_string();
                // heading anchor(`#...`)는 라이브러리 내장 scroll-to-heading이
                // 처리해야 하므로 link hook 대상에서 뺀다(hook으로 등록하면 그
                // 내장 경로가 가려진다).
                if !dest.starts_with('#') {
                    link_refs.push(dest);
                }
            }
            _ => {}
        }
    }
    (image_refs, link_refs)
}

/// dest 맨 앞의 URI 스킴을 뽑는다(RFC 3986 §3.1: 알파벳으로 시작, 이후
/// 알파벳/숫자/`+`/`-`/`.`). 슬래시가 스킴 후보 안에 섞여 있으면 상대경로에 우연히
/// 들어간 콜론(예: `notes:section` 같은 파일명)이지 스킴이 아니라고 본다.
fn extract_scheme(dest: &str) -> Option<&str> {
    let colon = dest.find(':')?;
    let candidate = &dest[..colon];
    if candidate.is_empty() || candidate.contains('/') {
        return None;
    }
    let mut chars = candidate.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    Some(candidate)
}

/// 링크 목적지를 §7.3 화이트리스트로 분류한다. `http`/`https`만 외부로 열고, 스킴이
/// 없으면 상대 문서 열기 intent, 그 외(`javascript:`/`data:`/커스텀 스킴)는 거부한다.
fn classify_destination(dest: &str) -> MarkdownLinkIntent {
    match extract_scheme(dest) {
        Some(scheme)
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            MarkdownLinkIntent::OpenExternal(dest.to_owned())
        }
        Some(_) => MarkdownLinkIntent::Rejected(dest.to_owned()),
        None => MarkdownLinkIntent::OpenRelativeDocument(dest.to_owned()),
    }
}

/// GFM alert 5종의 강조색을 Deppy 토큰에서 가져온다(§6.2 "자체 색상표를 새로 만들지
/// 않는다"). 아이콘·식별자는 업스트림 `AlertBundle::gfm()` 기본값을 그대로 쓴다.
fn deppy_alert_bundle(tokens: designall::Tokens) -> AlertBundle {
    AlertBundle::from_alerts(vec![
        Alert {
            accent_color: tokens.accent,
            icon: '❕',
            identifier: "NOTE".to_owned(),
            identifier_rendered: "Note".to_owned(),
        },
        Alert {
            accent_color: tokens.success,
            icon: '💡',
            identifier: "TIP".to_owned(),
            identifier_rendered: "Tip".to_owned(),
        },
        Alert {
            accent_color: tokens.accent,
            icon: '💬',
            identifier: "IMPORTANT".to_owned(),
            identifier_rendered: "Important".to_owned(),
        },
        Alert {
            accent_color: tokens.warning,
            icon: '⚠',
            identifier: "WARNING".to_owned(),
            identifier_rendered: "Warning".to_owned(),
        },
        Alert {
            accent_color: tokens.error,
            icon: '🔴',
            identifier: "CAUTION".to_owned(),
            identifier_rendered: "Caution".to_owned(),
        },
    ])
}

/// 색·타이포·간격을 전부 `designall` 토큰/스타일 조정에서 가져온다(§6.2). 이 함수가
/// 받은 `ui`(centered column 안쪽)에만 적용되고 바깥 UI로 새지 않는다.
///
/// ## 헤딩 레벨별 크기 — 시도한 것과 막힌 지점
///
/// egui는 `TextStyle::Heading`이 하나뿐이라 H1~H6이 전부 같은 크기로 나올 것 같지만,
/// `egui_commonmark_backend::misc::Style::to_richtext`(비공개 헬퍼, 0.24.0)가 이미
/// 레벨별로 크기를 갈아끼운다: 레벨 0(H1)은 `TextStyle::Heading` 값을 그대로 쓰고,
/// 레벨 1~5(H2~H6)는 `Body`~`Heading` 사이를 고정 비율로 보간한다
/// (0.835 / 0.668 / 0.501 / 0.334 / 0.167 — 소스에서 실측). 이 facade가 손댈 수 있는
/// 레버는 딱 두 개, `TextStyle::Body`·`TextStyle::Heading`의 크기뿐이다.
///
/// 문제는 이 두 레버로 표의 목표(H1 19 / H2 15.5 / H3 13.5, 본문 13)를 동시에 맞출
/// 수 없다는 점이다. H2 = Body + 0.835×(Heading−Body)인데, Body를 0으로 내려도
/// H2 ≥ 0.835×Heading = 0.835×19 ≈ 15.87로 목표 15.5보다 크다 — 즉 body가 몇이든
/// H1을 19로 고정하는 한 H2는 15.5 밑으로 내려갈 수 없다(수식으로 확인, 크레이트를
/// 고치지 않는 한 불가능). `render_math_fn`/`render_html_fn`처럼 레벨별로 갈아끼울
/// 콜백은 이 크레이트에 없다(공개 API 전수 확인: `indentation_spaces`,
/// `max_image_width`, `default_width`, `show_alt_text_on_hover`,
/// `default_implicit_uri_scheme`, `explicit_image_uri_scheme`, syntax 테마, `alerts`,
/// `render_math_fn`, `render_html_fn`, `enable_scroll_to_heading`뿐 — heading 전용
/// 훅은 없다). 이 값 자체가 `egui_commonmark_backend`(비공개 크레이트) 안에 박혀 있어
/// facade 밖에서 가로챌 지점이 없고, 크레이트를 포크/패치하는 건 이 작업의 범위(문서
/// 표면 스타일링)를 크게 벗어난다고 판단해 시도하지 않았다.
///
/// 그래서 표에서 정확히 맞출 수 있는 두 값(본문 13, H1 19)만 그대로 심는다. H2 이하는
/// 크레이트의 내장 보간이 대신 계산하며, H1(19, bold) > H2(≈18.0, bold) > H3(≈17.0,
/// bold) > H4(≈16.0, bold) > H5(≈15.0) > H6(≈14.0) 순으로 **단조 감소는 유지**하지만
/// 표의 간격(H1-H2 3.5pt)만큼 벌어지지는 않는다 — H1은 본문(13)과 6pt·46% 차이로
/// 뚜렷이 구분되고, 그 아래는 계단이 촘촘하다. 상위 설계에 보고: 정확한 표 값이
/// 필요하면 `egui_commonmark`를 포크하거나 다른 렌더 경로가 필요하다.
fn apply_page_style(ui: &mut egui::Ui) {
    let tokens = designall::tokens(ui.visuals());
    designall::apply_workspace_visuals(ui);
    let style = ui.style_mut();
    style.spacing.item_spacing.y = PAGE_ITEM_SPACING_Y;
    // 본문 텍스트를 마우스로 드래그해 복사할 수 있어야 한다(§6.2).
    style.interaction.selectable_labels = true;
    if let Some(body) = style.text_styles.get_mut(&egui::TextStyle::Body) {
        body.size = PAGE_BODY_FONT_SIZE;
    }
    if let Some(heading) = style.text_styles.get_mut(&egui::TextStyle::Heading) {
        heading.size = PAGE_HEADING_FONT_SIZE;
    }
    // 인라인 코드 span(`` `code` ``)의 배경은 `apply_workspace_visuals`가 짚어주는
    // `extreme_bg_color`(코드 블록이 쓴다)와 별개 필드(`code_bg_color`)라 egui 기본
    // 회색(gray(64)/gray(230))이 새 나가고 있었다 — 다크/라이트 둘 다 앱 팔레트 밖의
    // 무채색이라 여기서 토큰으로 덮는다.
    style.visuals.code_bg_color = tokens.input_background;
}

/// 워크스페이스 루트 밖 파일을 읽지 않는 로컬 PNG broker(§7.2). `MarkdownViewer`가
/// 소유하며 밖으로 나가지 않는다.
///
/// 세대(`slot`+`revision`) 단위로 한 번만 스캔한다 — 같은 세대에서 다시 `sync`를
/// 호출해도(매 프레임 호출된다) 파일을 다시 읽거나 재검증하지 않는다. 세대가
/// 바뀌면 이전에 등록한 이미지를 `forget_image`로 지우고 다시 스캔한다.
struct WorkspaceImageBroker {
    registered_uris: Vec<String>,
    last_generation: Option<(u64, u64)>,
}

impl WorkspaceImageBroker {
    fn new() -> Self {
        Self {
            registered_uris: Vec::new(),
            last_generation: None,
        }
    }

    /// `default_implicit_uri_scheme`에 넘길 접두사를 돌려준다. 이 접두사가 있어야
    /// CommonMarkViewer가 스킴 없는 상대경로에 `deppy-image://<세대>/`를 붙여
    /// broker가 등록한 것과 같은 URI를 만든다. Viewer에는 canonical 절대경로가 아니라
    /// 이 세대-스코프 URI만 전달된다(§7.2).
    fn sync(
        &mut self,
        ctx: &egui::Context,
        image_refs: &[String],
        view: &MarkdownViewerContext<'_>,
    ) -> String {
        let generation = (view.slot.0, view.revision.0);
        let prefix = format!("deppy-image://{}-{}/", view.slot.0, view.revision.0);
        if self.last_generation == Some(generation) {
            return prefix;
        }
        for uri in self.registered_uris.drain(..) {
            ctx.forget_image(&uri);
        }
        self.last_generation = Some(generation);
        for relative in image_refs {
            let uri = format!("{prefix}{relative}");
            if self.registered_uris.contains(&uri) {
                continue; // 같은 세대에서 같은 이미지가 여러 번 참조돼도 한 번만 읽는다.
            }
            if let Ok(bytes) = validate_and_read(view.workspace_root, view.base_directory, relative)
            {
                ctx.include_bytes(uri.clone(), bytes);
                self.registered_uris.push(uri);
            }
            // 실패 시 아무것도 등록하지 않는다 — egui가 loader 없음으로 처리해 알아서
            // 실패 placeholder를 그린다(§7.2, render를 중단하지 않는다).
        }
        prefix
    }
}

/// Deppy가 소유하는 Markdown 뷰어. `egui_commonmark`는 이 구조체 안에서만 쓴다.
pub struct MarkdownViewer {
    cache: CommonMarkCache,
    scroll_key: Option<ScrollCacheKey>,
    image_broker: WorkspaceImageBroker,
}

impl Default for MarkdownViewer {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownViewer {
    pub fn new() -> Self {
        Self {
            cache: CommonMarkCache::default(),
            scroll_key: None,
            image_broker: WorkspaceImageBroker::new(),
        }
    }

    /// `source`를 읽기 좋은 문서 페이지로 그린다. 클릭된 링크가 있으면 intent를
    /// 돌려준다 — 파일을 열거나 URL을 여는 건 호출부 몫이다.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        source: &str,
        view: MarkdownViewerContext<'_>,
    ) -> Option<MarkdownLinkIntent> {
        // "file"/"http" feature를 켜지 않았으므로 이 호출은 PNG 디코드 loader만
        // 설치한다(§7.1) — 켤 때마다 새로 설치하지 않고 이미 있으면 건너뛴다.
        egui_extras::install_image_loaders(ui.ctx());

        let (image_refs, link_targets) = extract_destinations(source);
        let uri_prefix = self.image_broker.sync(ui.ctx(), &image_refs, &view);

        // 매 프레임 다시 등록한다 — `add_link_hook`은 매번 훅 상태를 false로 리셋하고
        // `CommonMarkViewer::show*`도 호출 시작 시 전체를 리셋하므로(업스트림 문서),
        // 여기서 소스가 바뀌어도 항상 최신 목적지 집합을 반영한다.
        for dest in &link_targets {
            self.cache.add_link_hook(dest.clone());
        }

        let tokens = designall::tokens(ui.visuals());

        egui::Frame::NONE
            .fill(tokens.content_canvas)
            .inner_margin(egui::Margin::symmetric(PAGE_PADDING_X, PAGE_PADDING_Y))
            .show(ui, |ui| {
                apply_page_style(ui);
                ui.vertical_centered(|ui| {
                    let column_width = ui.available_width().clamp(0.0, PAGE_MAX_CONTENT_WIDTH);
                    ui.set_max_width(column_width);

                    let scroll_key = ScrollCacheKey {
                        slot: view.slot.0,
                        revision: view.revision.0,
                        dark_mode: ui.visuals().dark_mode,
                        width_bucket: width_bucket(column_width),
                    };
                    if self.scroll_key != Some(scroll_key) {
                        if let Some(old_key) = self.scroll_key {
                            self.cache.clear_scrollable_with_id(old_key);
                        }
                        self.scroll_key = Some(scroll_key);
                    }

                    CommonMarkViewer::new()
                        .default_implicit_uri_scheme(uri_prefix)
                        .enable_scroll_to_heading(true)
                        .show_alt_text_on_hover(true)
                        .max_image_width(Some(column_width as usize))
                        .alerts(deppy_alert_bundle(tokens))
                        // raw HTML은 절대 켜지 않는다(§7.3) — `html_fn`을 `None`으로
                        // 두면 HTML 블록/인라인이 텍스트로만 표시되고 실행되지 않는다
                        // (업스트림 기본값, 여기서 명시적으로 강조해 둔다).
                        .show_scrollable(scroll_key, ui, &mut self.cache, source);
                });
            });

        link_targets
            .into_iter()
            .find(|dest| self.cache.get_link_hook(dest) == Some(true))
            .map(|dest| classify_destination(&dest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        image.save(path).expect("테스트 PNG 저장 실패");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "deppy-markdown-viewer-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // macOS의 /tmp는 /private/tmp 심볼릭 링크라 canonicalize로 대칭을 맞춘다
        // (file_tree.rs 테스트와 같은 관례).
        base.canonicalize().unwrap()
    }

    // ── ① 이미지 broker: 워크스페이스 밖 경로 거부 (가장 중요한 테스트) ──────────

    #[test]
    fn broker는_상대경로_탈출을_거부한다() {
        let workspace = temp_dir("root-escape");
        let doc_dir = workspace.join("docs");
        std::fs::create_dir_all(&doc_dir).unwrap();
        let secret_dir = workspace.parent().unwrap().join(format!(
            "deppy-markdown-viewer-secret-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&secret_dir).unwrap();
        write_png(&secret_dir.join("secret.png"), 4, 4);

        let result = validate_and_read(&workspace, &doc_dir, "../../etc/passwd");
        assert_eq!(result, Err(ImageRejection::WrongExtension));

        // 확장자를 우회해도(가짜로 .png를 붙여도) 루트 밖이면 여전히 막힌다 —
        // canonicalize+starts_with 검사가 확장자 검사와 독립적으로 걸린다는 걸
        // 확인한다.
        let escape_png = format!(
            "../../{}/secret.png",
            secret_dir.file_name().unwrap().to_str().unwrap()
        );
        let result = validate_and_read(&workspace, &doc_dir, &escape_png);
        assert_eq!(result, Err(ImageRejection::OutsideWorkspace));
    }

    #[test]
    fn broker는_절대경로를_거부한다() {
        let workspace = temp_dir("absolute");
        write_png(&workspace.join("inside.png"), 4, 4);
        let result = validate_and_read(&workspace, &workspace, "/etc/passwd");
        assert_eq!(result, Err(ImageRejection::AbsolutePath));

        // 워크스페이스 안을 가리키는 절대경로도 거부한다 — §7.2는 "document
        // directory 기준 상대경로만" 해석한다고 못박는다.
        let inside_absolute = workspace.join("inside.png");
        let result = validate_and_read(&workspace, &workspace, inside_absolute.to_str().unwrap());
        assert_eq!(result, Err(ImageRejection::AbsolutePath));
    }

    #[cfg(unix)]
    #[test]
    fn broker는_symlink_탈출을_거부한다() {
        let workspace = temp_dir("symlink-escape");
        let doc_dir = workspace.join("docs");
        std::fs::create_dir_all(&doc_dir).unwrap();
        let outside = temp_dir("symlink-target");
        write_png(&outside.join("real.png"), 4, 4);

        std::os::unix::fs::symlink(outside.join("real.png"), doc_dir.join("link.png")).unwrap();

        let result = validate_and_read(&workspace, &doc_dir, "link.png");
        assert_eq!(result, Err(ImageRejection::OutsideWorkspace));
    }

    #[test]
    fn broker는_루트_안의_유효한_png는_허용한다() {
        let workspace = temp_dir("valid-png");
        let doc_dir = workspace.join("docs");
        std::fs::create_dir_all(&doc_dir).unwrap();
        write_png(&doc_dir.join("diagram.png"), 8, 8);

        let result = validate_and_read(&workspace, &doc_dir, "diagram.png");
        assert!(
            result.is_ok(),
            "루트 안 유효한 PNG는 통과해야 한다: {result:?}"
        );
    }

    #[test]
    fn broker는_png가_아닌_확장자를_거부한다() {
        let workspace = temp_dir("wrong-ext");
        std::fs::write(workspace.join("script.txt"), b"not an image").unwrap();
        let result = validate_and_read(&workspace, &workspace, "script.txt");
        assert_eq!(result, Err(ImageRejection::WrongExtension));
    }

    #[test]
    fn broker는_인코딩_바이트_상한을_넘는_png를_거부한다() {
        let workspace = temp_dir("too-large");
        let path = workspace.join("huge.png");
        // 진짜 유효한 PNG일 필요 없다 — 바이트 상한 검사가 디코드보다 먼저 걸린다.
        std::fs::write(&path, vec![0u8; (MAX_IMAGE_ENCODED_BYTES + 1) as usize]).unwrap();
        let result = validate_and_read(&workspace, &workspace, "huge.png");
        assert_eq!(result, Err(ImageRejection::TooLarge));
    }

    #[test]
    fn broker는_디코드_치수_상한을_넘는_png를_거부한다() {
        let workspace = temp_dir("too-wide");
        write_png(&workspace.join("wide.png"), MAX_IMAGE_DIMENSION_PX + 1, 1);
        let result = validate_and_read(&workspace, &workspace, "wide.png");
        assert_eq!(result, Err(ImageRejection::DecodeRejected));
    }

    // ── ② broker 캐시: 세대가 같으면 다시 읽지 않는다 (되돌리면 실패) ───────────

    #[test]
    fn broker는_같은_세대에서_다시_읽지_않는다() {
        let workspace = temp_dir("cache-generation");
        let doc_dir = workspace.join("docs");
        std::fs::create_dir_all(&doc_dir).unwrap();
        write_png(&doc_dir.join("a.png"), 4, 4);
        let image_refs = vec!["a.png".to_owned()];
        let view = MarkdownViewerContext {
            slot: MarkdownDocumentSlot(1),
            revision: MarkdownSourceRevision(1),
            workspace_root: &workspace,
            base_directory: &doc_dir,
        };

        let ctx = egui::Context::default();
        let mut broker = WorkspaceImageBroker::new();
        broker.sync(&ctx, &image_refs, &view);
        assert_eq!(broker.registered_uris.len(), 1);

        // 파일을 지운다 — 만약 두 번째 `sync`가 다시 검증/등록을 시도한다면 이제는
        // 실패해서 등록 목록에서 빠질 것이다. 같은 세대에서는 재검증하지 않아야
        // 등록이 그대로 남는다.
        std::fs::remove_file(doc_dir.join("a.png")).unwrap();
        broker.sync(&ctx, &image_refs, &view);
        assert_eq!(
            broker.registered_uris.len(),
            1,
            "같은 (slot, revision)이면 파일이 사라져도 재검증하지 않아야 한다"
        );
    }

    #[test]
    fn broker는_세대가_바뀌면_다시_스캔한다() {
        let workspace = temp_dir("cache-new-generation");
        let doc_dir = workspace.join("docs");
        std::fs::create_dir_all(&doc_dir).unwrap();
        write_png(&doc_dir.join("a.png"), 4, 4);
        let image_refs = vec!["a.png".to_owned()];
        let slot = MarkdownDocumentSlot(1);

        let ctx = egui::Context::default();
        let mut broker = WorkspaceImageBroker::new();
        broker.sync(
            &ctx,
            &image_refs,
            &MarkdownViewerContext {
                slot,
                revision: MarkdownSourceRevision(1),
                workspace_root: &workspace,
                base_directory: &doc_dir,
            },
        );
        assert_eq!(broker.registered_uris.len(), 1);

        std::fs::remove_file(doc_dir.join("a.png")).unwrap();
        broker.sync(
            &ctx,
            &image_refs,
            &MarkdownViewerContext {
                slot,
                revision: MarkdownSourceRevision(2),
                workspace_root: &workspace,
                base_directory: &doc_dir,
            },
        );
        assert_eq!(
            broker.registered_uris.len(),
            0,
            "리비전이 바뀌면 다시 스캔해 사라진 파일은 등록에서 빠져야 한다"
        );
    }

    // ── ③ link policy: 화이트리스트 밖 스킴은 통과시키지 않는다 ────────────────

    #[test]
    fn link_policy는_http_https만_외부로_연다() {
        assert_eq!(
            classify_destination("https://example.com/doc"),
            MarkdownLinkIntent::OpenExternal("https://example.com/doc".to_owned())
        );
        assert_eq!(
            classify_destination("HTTP://example.com"),
            MarkdownLinkIntent::OpenExternal("HTTP://example.com".to_owned())
        );
    }

    #[test]
    fn link_policy는_javascript와_data_스킴을_거부한다() {
        assert_eq!(
            classify_destination("javascript:alert(1)"),
            MarkdownLinkIntent::Rejected("javascript:alert(1)".to_owned())
        );
        assert_eq!(
            classify_destination("data:text/html,<script>alert(1)</script>"),
            MarkdownLinkIntent::Rejected("data:text/html,<script>alert(1)</script>".to_owned())
        );
        assert_eq!(
            classify_destination("file:///etc/passwd"),
            MarkdownLinkIntent::Rejected("file:///etc/passwd".to_owned())
        );
        assert_eq!(
            classify_destination("ftp://example.com/x"),
            MarkdownLinkIntent::Rejected("ftp://example.com/x".to_owned())
        );
    }

    #[test]
    fn link_policy는_스킴_없는_상대경로를_문서_열기_intent로_분류한다() {
        assert_eq!(
            classify_destination("other.md"),
            MarkdownLinkIntent::OpenRelativeDocument("other.md".to_owned())
        );
        assert_eq!(
            classify_destination("../sibling/notes.md"),
            MarkdownLinkIntent::OpenRelativeDocument("../sibling/notes.md".to_owned())
        );
    }

    #[test]
    fn extract_destinations는_이미지와_heading_anchor를_링크에서_뺀다() {
        let source = "![img](pic.png) [본문 링크](https://a.example) [앵커](#heading)";
        let (_, links) = extract_destinations(source);
        assert_eq!(links, vec!["https://a.example".to_owned()]);
    }

    #[test]
    fn extract_destinations는_원격_스킴을_이미지_broker_대상에서_뺀다() {
        let source = "![로컬](local.png) ![원격](https://a.example/x.png) ![데이터](data:image/png;base64,AAAA)";
        let (images, _) = extract_destinations(source);
        assert_eq!(images, vec!["local.png".to_owned()]);
    }

    #[test]
    fn extract_destinations는_균형_잡힌_괄호가_있는_목적지도_정확히_뽑는다() {
        // CommonMark는 링크 목적지 안에서 이스케이프 없는 균형 괄호를 한 단계
        // 허용한다 — 정규식이 아니라 실제 파서를 써야 하는 이유(§7.3 위험 사례).
        let source = "[클릭](javascript:alert(1))";
        let (_, links) = extract_destinations(source);
        assert_eq!(links, vec!["javascript:alert(1)".to_owned()]);
    }

    // ── ④ 캐시: 같은 소스는 scrollable 캐시를 지우지 않는다 (되돌리면 실패) ────

    #[test]
    fn scroll_cache_key는_소스가_그대로면_바뀌지_않는다() {
        let mut viewer = MarkdownViewer::new();
        let workspace = temp_dir("scroll-cache");
        let source = "# 제목\n\n본문";
        let ctx = egui::Context::default();

        // 실제 `show`는 egui::Ui가 필요해 `run_ui`로 감싼다.
        let _ = ctx.run_ui(Default::default(), |ui| {
            viewer.show(
                ui,
                source,
                MarkdownViewerContext {
                    slot: MarkdownDocumentSlot(7),
                    revision: MarkdownSourceRevision(1),
                    workspace_root: &workspace,
                    base_directory: &workspace,
                },
            );
        });
        let first_key = viewer.scroll_key;
        assert!(first_key.is_some());

        let _ = ctx.run_ui(Default::default(), |ui| {
            viewer.show(
                ui,
                source,
                MarkdownViewerContext {
                    slot: MarkdownDocumentSlot(7),
                    revision: MarkdownSourceRevision(1),
                    workspace_root: &workspace,
                    base_directory: &workspace,
                },
            );
        });
        assert_eq!(
            viewer.scroll_key, first_key,
            "리비전이 그대로면 scroll_key(=scrollable 캐시 id)가 바뀌면 안 된다"
        );
    }

    // ── ⑤ egui_kittest: 대표 문서가 패닉 없이 그려진다 ──────────────────────────

    struct MarkdownHarnessState {
        viewer: MarkdownViewer,
        source: String,
        workspace: PathBuf,
        last_intent: Option<MarkdownLinkIntent>,
    }

    fn representative_fixture() -> &'static str {
        "# 제목 H1\n\n\
         ## 부제 H2\n\n\
         일반 문단. **강조**, *기울임*, ~~취소선~~, `인라인 코드`. \\*이스케이프된 별표\\*\n\n\
         - 목록 항목\n\
         - [ ] 미완료 작업\n\
         - [x] 완료 작업\n\n\
         > 인용문\n\n\
         > [!WARNING]\n\
         > GFM 경고 블록\n\n\
         | 열A | 열B |\n\
         |---|---|\n\
         | 1 | 2 |\n\n\
         ```rust\n\
         fn main() {}\n\
         ```\n\n\
         각주 참조[^1]\n\n\
         [^1]: 각주 본문\n\n\
         [외부 링크](https://example.com/doc)\n\n\
         [자바스크립트](javascript:alert(1))\n\n\
         ![로컬 이미지](missing.png)\n\n\
         <script>alert(1)</script>\n"
    }

    fn harness_for(
        state: MarkdownHarnessState,
    ) -> egui_kittest::Harness<'static, MarkdownHarnessState> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut MarkdownHarnessState| {
                let workspace = state.workspace.clone();
                state.last_intent = state.viewer.show(
                    ui,
                    &state.source,
                    MarkdownViewerContext {
                        slot: MarkdownDocumentSlot(1),
                        revision: MarkdownSourceRevision(1),
                        workspace_root: &workspace,
                        base_directory: &workspace,
                    },
                );
            },
            state,
        )
    }

    #[test]
    fn kittest_대표_문서는_패닉_없이_그려진다() {
        let workspace = temp_dir("kittest-fixture");
        let state = MarkdownHarnessState {
            viewer: MarkdownViewer::new(),
            source: representative_fixture().to_owned(),
            workspace,
            last_intent: None,
        };
        let mut harness = harness_for(state);
        harness.run();
        // 두 번째 프레임도 패닉 없이 그려져야 한다(scrollable 캐시 재사용 경로).
        harness.run();
    }

    #[test]
    fn kittest_raw_html은_텍스트로만_보이고_사라지지_않는다() {
        use egui_kittest::kittest::Queryable;

        let workspace = temp_dir("kittest-raw-html");
        let state = MarkdownHarnessState {
            viewer: MarkdownViewer::new(),
            source: "<script>alert(1)</script>".to_owned(),
            workspace,
            last_intent: None,
        };
        let mut harness = harness_for(state);
        harness.run();
        assert!(
            harness
                .query_by_label_contains("<script>alert(1)</script>")
                .is_some(),
            "raw HTML은 실행되지 않고 있는 그대로 텍스트로 남아야 한다"
        );
    }

    #[test]
    fn kittest_javascript_링크를_클릭해도_intent가_외부열기로_새지_않는다() {
        use egui_kittest::kittest::Queryable;

        let workspace = temp_dir("kittest-js-link");
        let state = MarkdownHarnessState {
            viewer: MarkdownViewer::new(),
            source: "[클릭](javascript:alert(1))".to_owned(),
            workspace,
            last_intent: None,
        };
        let mut harness = harness_for(state);
        harness.run();

        // `harness.run()`은 접근성 트리가 안정될 때까지 내부적으로 여러 프레임을
        // 돌린다(egui_kittest 기본 `max_steps`, 실측 3회). egui_commonmark의
        // `Link::end`는 매 프레임 시작 시 `CommonMarkCache::deactivate_link_hooks`로
        // 훅을 false로 리셋하므로, 클릭 이벤트가 소비된 그 프레임 *다음*에 오는
        // 추가 settle 프레임들이 `last_intent`를 다시 `None`으로 덮어써 버린다
        // (실측: `run()`을 쓰면 클릭 프레임에서 true였다가 바로 다음 프레임에서
        // false로 되돌아간다). 그래서 클릭을 처리하는 그 프레임만은 `step()`으로
        // 정확히 한 번만 그려 이 값을 확인한다.
        harness.get_by_label_contains("클릭").click();
        harness.step();

        assert_eq!(
            harness.state().last_intent,
            Some(MarkdownLinkIntent::Rejected(
                "javascript:alert(1)".to_owned()
            ))
        );
    }

    #[test]
    fn kittest_http_링크를_클릭하면_외부열기_intent를_올린다() {
        use egui_kittest::kittest::Queryable;

        let workspace = temp_dir("kittest-http-link");
        let state = MarkdownHarnessState {
            viewer: MarkdownViewer::new(),
            source: "[문서](https://example.com/doc)".to_owned(),
            workspace,
            last_intent: None,
        };
        let mut harness = harness_for(state);
        harness.run();

        // 클릭을 처리하는 프레임만 `step()`으로 한 번 그린다 — 위 테스트와 같은 이유
        // (`run()`의 추가 settle 프레임이 클릭 직후의 `last_intent`를 덮어쓴다).
        harness.get_by_label_contains("문서").click();
        harness.step();

        assert_eq!(
            harness.state().last_intent,
            Some(MarkdownLinkIntent::OpenExternal(
                "https://example.com/doc".to_owned()
            ))
        );
    }

    /// 페이지 색은 전부 `designall::tokens`/`apply_workspace_visuals`에서 와야 한다 —
    /// 리터럴 `Color32`를 직접 적으면 다크/라이트 중 한쪽에서만 어긋나는 색이 슬쩍
    /// 섞여 들어올 수 있다(`activity.rs`의
    /// `production_source_has_no_render_host_or_polling_edges`와 같은 소스 스캔 관례).
    #[test]
    fn 프로덕션_코드는_색을_리터럴로_적지_않고_토큰에서만_가져온다() {
        let source = include_str!("markdown_viewer.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "Color32::from_rgb",
            "Color32::from_gray",
            "Color32::from_rgba",
            "Color32::WHITE",
            "Color32::BLACK",
        ] {
            assert!(
                !production.contains(forbidden),
                "markdown_viewer.rs leaf는 색을 하드코딩하면 안 된다, 발견: {forbidden}"
            );
        }
    }
}
