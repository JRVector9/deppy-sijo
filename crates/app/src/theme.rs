//! 목업 팔레트를 egui Visuals로 심는다 (2026-07-06). 실제 앱은 그동안 egui 기본
//! 다크/라이트 색을 썼는데, 목업은 시안 액센트 + 쿨그레이 팔레트를 쓴다 — 컴포넌트들이
//! `ui.visuals().selection.bg_fill`(액센트)·`hairline` 등을 참조하므로 여기 한 번 심으면
//! 전 화면에 전파된다. `set_visuals_of`로 테마별로 등록하면 set_theme이 알아서 고른다.

use egui::{Color32, Stroke};

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// 다크 팔레트 (목업 다크 §토큰: bg #131317 · panel #1b1b21 · accent #43b8cd …).
fn dark() -> egui::Visuals {
    let accent = rgb(0x43, 0xb8, 0xcd);
    let text = rgb(0xd7, 0xd8, 0xdb);
    let dim = rgb(0x8b, 0x8f, 0x98);
    let faint = rgb(0x5b, 0x5f, 0x68);
    let bg = rgb(0x13, 0x13, 0x17);
    let panel = rgb(0x1b, 0x1b, 0x21);
    let panel2 = rgb(0x22, 0x22, 0x2a);
    let panel_hi = rgb(0x2a, 0x2a, 0x33);
    let hair = rgb(0x2d, 0x2d, 0x36);
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

/// 라이트 팔레트 (목업 라이트 §토큰: bg #fbfcfd · panel #f1f3f5 · accent #1c93aa …).
fn light() -> egui::Visuals {
    let accent = rgb(0x1c, 0x93, 0xaa);
    let text = rgb(0x23, 0x26, 0x2c);
    let dim = rgb(0x65, 0x6b, 0x74);
    let faint = rgb(0x9a, 0xa0, 0xa9);
    let bg = rgb(0xfb, 0xfc, 0xfd);
    let panel = rgb(0xf1, 0xf3, 0xf5);
    let panel2 = rgb(0xe7, 0xea, 0xee);
    let panel_hi = rgb(0xdd, 0xe1, 0xe7);
    let hair = rgb(0xd5, 0xd9, 0xdf);
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
    w.inactive.bg_fill = panel2;
    w.inactive.weak_bg_fill = panel2;
    w.inactive.bg_stroke = Stroke::new(1.0, hair);
    w.inactive.fg_stroke = Stroke::new(1.0, dim);
    w.hovered.bg_fill = panel_hi;
    w.hovered.weak_bg_fill = panel_hi;
    w.hovered.bg_stroke = Stroke::new(1.0, hair);
    w.hovered.fg_stroke = Stroke::new(1.0, text);
    w.active.bg_fill = panel_hi;
    w.active.weak_bg_fill = panel_hi;
    w.active.bg_stroke = Stroke::new(1.0, accent);
    w.active.fg_stroke = Stroke::new(1.0, text);
    w.open.bg_fill = panel_hi;
    w.open.weak_bg_fill = panel_hi;
    w.open.fg_stroke = Stroke::new(1.0, text);
    v
}

/// 다크/라이트 커스텀 팔레트를 egui에 등록한다. set_theme이 프리퍼런스에 맞춰 고른다.
pub fn install_palette(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, dark());
    ctx.set_visuals_of(egui::Theme::Light, light());
}
