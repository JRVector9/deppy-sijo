//! 목업 팔레트를 egui Visuals로 심는다 (2026-07-06). 실제 앱은 그동안 egui 기본
//! 다크/라이트 색을 썼는데, 목업은 시안 액센트 + 쿨그레이 팔레트를 쓴다 — 컴포넌트들이
//! `ui.visuals().selection.bg_fill`(액센트)·`hairline` 등을 참조하므로 여기 한 번 심으면
//! 전 화면에 전파된다. `set_visuals_of`로 테마별로 등록하면 set_theme이 알아서 고른다.

use egui::{Color32, Stroke};

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// 다크 팔레트. 목업 §토큰(bg #131317 · panel #1b1b21)의 **명도**는 유지하되 색상축은
/// designall(hsl 210도)로 맞췄다 — 아래 주석 참고.
fn dark() -> egui::Visuals {
    // 액센트는 designall 토큰이 단일 출처다 — "선택됨/링크/포커스"라는 **같은 의미**가
    // 사이드바(#39b8e8)·앱 크롬(#43b8cd)·설정(#4da6c8) 세 가지 청록으로 갈려 있었다.
    // 색상 7도, 밝기 13%p 차이라 다르다고 인식하기엔 가깝고 같다고 넘기기엔 멀어서,
    // 의도한 대비가 아니라 렌더링 오류처럼 읽혔다(2026-08-06). 라이트 모드는 이미
    // designall과 값이 같았다 — 어긋난 건 다크뿐이었다.
    let accent = crate::ui::designall::DARK.accent;
    // 표면·텍스트는 designall과 **색상축**을 공유한다 (hsl 210도). 명도 위계(사이드바가
    // 더 어두움)는 의도된 것이라 L은 전부 그대로 두고 H/S만 맞췄다 — 사이드바는 210도
    // 청록빛 남색인데 여기는 240도 보랏빛 남색이라, 맞닿은 두 면이 서로를 물들여
    // 보이게 했다(2026-08-06). 채도도 역할별로 designall 실측치를 따른다:
    // 표면 27% · 본문 22% · dim 10%. 명도를 유지하므로 본문 대비는 12.02 -> 11.80으로
    // 사실상 그대로다. 라이트 팔레트는 이미 H 214~216도로 축 위에 있어 건드리지 않았다
    // — 어긋난 건 다크뿐이었다.
    let text = rgb(0xd1, 0xd9, 0xe1);
    let dim = rgb(0x87, 0x92, 0x9c);
    // faint는 palette()가 `_faint`로 받아 쓰지 않는다(기존부터 미사용). 팔레트가 한
    // 축에서 갈라지지 않게 값만 맞춰 둔다.
    let faint = rgb(0x58, 0x61, 0x6b);
    let bg = rgb(0x0f, 0x15, 0x1b);
    let panel = rgb(0x16, 0x1e, 0x26);
    let panel2 = rgb(0x1c, 0x26, 0x30);
    let panel_hi = rgb(0x22, 0x2e, 0x3b);
    // 구분선은 designall(#26303a)과 **값**을 합칠 수 없다 — 이 패널(#1b1b21) 위에 올리면
    // 명암비가 1.28로, #58에서 "안 보인다"고 보고된 #2d2d36(1.26)과 같아진다. 대신
    // 색상축만 designall과 맞추고(hsl 210도·S 21%) 명도는 이 표면에 맞춰 유지한다 —
    // 그래야 사이드바 경계선과 같은 재질로 보인다. 명암비 1.52 -> 1.57로 오히려 개선.
    let hair = rgb(0x31, 0x3e, 0x4b);
    palette(
        egui::Visuals::dark(),
        accent,
        text,
        dim,
        faint,
        bg,
        panel,
        panel2,
        panel_hi,
        hair,
    )
}

/// 라이트 팔레트 (목업 라이트 §토큰: bg #fbfcfd · panel #f1f3f5 · accent는 designall …).
fn light() -> egui::Visuals {
    // 값은 그대로(#1c93aa) — 라이트는 원래 designall과 일치했다. 출처만 합친다.
    let accent = crate::ui::designall::LIGHT.accent;
    let text = rgb(0x23, 0x26, 0x2c);
    let dim = rgb(0x65, 0x6b, 0x74);
    let faint = rgb(0x9a, 0xa0, 0xa9);
    let bg = rgb(0xfb, 0xfc, 0xfd);
    let panel = rgb(0xf1, 0xf3, 0xf5);
    let panel2 = rgb(0xe7, 0xea, 0xee);
    let panel_hi = rgb(0xdd, 0xe1, 0xe7);
    // 다크와 달리 라이트 구분선은 designall과 값이 이미 같았다 — 출처만 합친다.
    let hair = crate::ui::designall::LIGHT.separator;
    palette(
        egui::Visuals::light(),
        accent,
        text,
        dim,
        faint,
        bg,
        panel,
        panel2,
        panel_hi,
        hair,
    )
}

#[allow(clippy::too_many_arguments)]
fn palette(
    mut v: egui::Visuals,
    accent: Color32,
    text: Color32,
    dim: Color32,
    _faint: Color32,
    bg: Color32,
    panel: Color32,
    panel2: Color32,
    panel_hi: Color32,
    hair: Color32,
) -> egui::Visuals {
    v.override_text_color = Some(text);
    v.panel_fill = panel;
    v.window_fill = panel;
    v.window_stroke = Stroke::new(1.0, hair);
    v.window_corner_radius = egui::CornerRadius::same(2);
    v.menu_corner_radius = egui::CornerRadius::same(1);
    v.extreme_bg_color = bg; // TextEdit/깊은 배경
    v.faint_bg_color = panel2;
    v.hyperlink_color = accent;
    // 컴포넌트들이 accent로 참조하는 selection.bg_fill/stroke = 풀 시안.
    v.selection.bg_fill = accent;
    v.selection.stroke = Stroke::new(1.0, accent);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = panel;
    w.noninteractive.weak_bg_fill = panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, hair); // 헤어라인 기본색
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.noninteractive.corner_radius = egui::CornerRadius::same(2);
    w.inactive.bg_fill = panel2;
    w.inactive.weak_bg_fill = panel2;
    w.inactive.bg_stroke = Stroke::new(1.0, hair);
    w.inactive.fg_stroke = Stroke::new(1.0, dim);
    w.inactive.corner_radius = egui::CornerRadius::same(1);
    w.hovered.bg_fill = panel_hi;
    w.hovered.weak_bg_fill = panel_hi;
    w.hovered.bg_stroke = Stroke::new(1.0, hair);
    w.hovered.fg_stroke = Stroke::new(1.0, text);
    w.hovered.corner_radius = egui::CornerRadius::same(1);
    w.active.bg_fill = panel_hi;
    w.active.weak_bg_fill = panel_hi;
    w.active.bg_stroke = Stroke::new(1.0, accent);
    w.active.fg_stroke = Stroke::new(1.0, text);
    w.active.corner_radius = egui::CornerRadius::same(1);
    w.open.bg_fill = panel_hi;
    w.open.weak_bg_fill = panel_hi;
    w.open.fg_stroke = Stroke::new(1.0, text);
    w.open.corner_radius = egui::CornerRadius::same(1);
    v
}

/// 다크/라이트 커스텀 팔레트를 egui에 등록한다. set_theme이 프리퍼런스에 맞춰 고른다.
pub fn install_palette(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, dark());
    ctx.set_visuals_of(egui::Theme::Light, light());
}
