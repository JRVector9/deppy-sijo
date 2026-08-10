//! 목업 팔레트를 egui Visuals로 심는다 (2026-07-06). 실제 앱은 그동안 egui 기본
//! 다크/라이트 색을 썼는데, 목업은 시안 액센트 + 쿨그레이 팔레트를 쓴다 — 컴포넌트들이
//! `ui.visuals().selection.bg_fill`(액센트)·`hairline` 등을 참조하므로 여기 한 번 심으면
//! 전 화면에 전파된다. `set_visuals_of`로 테마별로 등록하면 set_theme이 알아서 고른다.

use egui::{Color32, Stroke};

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// 다크 팔레트. 목업 §토큰(bg #131317 · panel #1b1b21)의 **명도**는 유지하되 색상축은
/// designall(hsl 220도)로 맞췄다 — 아래 주석 참고.
fn dark() -> egui::Visuals {
    // 액센트는 designall 토큰이 단일 출처다 — "선택됨/링크/포커스"라는 **같은 의미**가
    // 사이드바(#39b8e8)·앱 크롬(#43b8cd)·설정(#4da6c8) 세 가지 청록으로 갈려 있었다.
    // 색상 7도, 밝기 13%p 차이라 다르다고 인식하기엔 가깝고 같다고 넘기기엔 멀어서,
    // 의도한 대비가 아니라 렌더링 오류처럼 읽혔다(2026-08-06). 라이트 모드는 이미
    // designall과 값이 같았다 — 어긋난 건 다크뿐이었다.
    let accent = crate::ui::designall::DARK.accent;
    // 표면은 designall과 **같은 4단 계단**을 쓴다 (hsl 220도, Nord·One Dark 자리):
    //   elev0 #0f1115 (L 7%)  깊은 배경·입력   elev1 #181b20 (L 11%) 표면·패널·사이드바
    //   elev2 #21242c (L 15%) 올라옴·hover      elev3 #292e38 (L 19%) 강조
    //
    // 여기까지 온 경위: 원래 사이드바 210도 · 여기 240도 · 설정 창 무채색으로 갈려 맞닿은
    // 면들이 서로를 물들여 보이게 했다. 먼저 210도로 합쳤지만 그건 "라이트가 designall과
    // 일치하니 다크만 어긋난 것"이라는 내부 정합성 근거였지 미적 판단이 아니었고, 화면으로
    // 보니 참조 테마 7개 중 가장 청록이면서 채도도 가장 높았다. 220도·채도 절반으로 옮긴 뒤
    // 명도까지 4단으로 정리했다(2026-08-06~07, 목업 A/B 확인).
    //
    // 사이드바와 이 크롬은 **같은 단(elev1)**이다. 이전에는 사이드바가 한 단 아래라
    // 터미널(L 5.5%)과 명암비 1.01로 붙어 경계가 사라졌다 — 지금은 1.13이라 구분선 없이도
    // 단이 보이고, 구분선이 그 위에 얹힌다.
    //
    // 라이트도 같은 축·같은 4단 구조를 쓴다 — 아래 light() 주석 참고.
    let text = rgb(0xd5, 0xd8, 0xdd);
    let dim = rgb(0x8a, 0x8f, 0x99);
    // faint는 palette()가 `_faint`로 받아 쓰지 않는다(기존부터 미사용). 팔레트가 한
    // 축에서 갈라지지 않게 값만 맞춰 둔다.
    let faint = rgb(0x5b, 0x5f, 0x68);
    let bg = rgb(0x0f, 0x11, 0x15);
    let panel = rgb(0x18, 0x1b, 0x20);
    let panel2 = rgb(0x21, 0x24, 0x2c);
    let panel_hi = rgb(0x29, 0x2e, 0x38);
    // 구분선은 이제 designall과 **값까지 같다**. 사이드바와 크롬이 같은 단(elev1)이 되면서
    // "표면 밝기가 달라 같은 값을 못 쓴다"던 제약(#58: 어두운 선이 밝은 패널에서 안 보임)이
    // 사라졌다. 이 패널 위에서 명암비 1.42, 터미널 위에서 1.60.
    let hair = rgb(0x32, 0x36, 0x3e);
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

/// 라이트 팔레트. 다크와 **같은 축(hsl 220도)·같은 4단 구조**를 쓴다.
fn light() -> egui::Visuals {
    // 값은 그대로(#1c93aa) — 라이트 액센트는 원래 designall과 일치했다. 출처만 합친다.
    let accent = crate::ui::designall::LIGHT.accent;
    // 라이트도 다크와 같은 정리를 거쳤다(2026-08-07). 그 전에는 색상이 200~220도로
    // 흩어지고 명도가 21단계였으며, 무엇보다 다크만 평평·4단으로 바꾼 탓에 **두 모드의
    // 구조가 서로 달랐다**.
    //
    // 계단은 다크의 "표면 기준 상대 거리"를 그대로 뒤집는다. 라이트에서는 올라올수록
    // 밝고 상호작용(hover/선택)은 반대로 어두워지는 게 관례라서다:
    //   입력 L 99.3 (+4) · 표면 L 95.3 · hover L 93.3 (-2) · 선택 L 91.3 (-4)
    // 레일·사이드바·크롬은 전부 표면 한 단(평평) — 다크와 같다.
    //
    // 구분선은 명도를 85.5 -> 81.2로 낮췄다. 기존 값은 표면 위 명암비 1.27로 다크(1.42)
    // 보다 흐려 두 모드에서 경계가 다르게 보였다.
    let text = rgb(0x23, 0x26, 0x2c);
    let dim = rgb(0x65, 0x6a, 0x74);
    let faint = rgb(0x9b, 0x9f, 0xa8);
    let bg = rgb(0xfd, 0xfd, 0xfd);
    let panel = rgb(0xf1, 0xf2, 0xf5);
    let panel2 = rgb(0xeb, 0xed, 0xf0);
    let panel_hi = rgb(0xe5, 0xe8, 0xec);
    // 구분선은 designall과 값까지 같다 — 다크와 마찬가지로 표면이 한 단이라 공유 가능하다.
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
    // 컴포넌트들이 accent로 참조하는 selection.bg_fill = 풀 시안.
    v.selection.bg_fill = accent;
    // stroke는 accent가 아니다 — egui는 이 색을 selection.bg_fill(accent) **배경 위에
    // 그려지는 전경색**으로 쓴다: 드래그로 선택한 글자 색, selectable_label 선택 글자 색
    // 등(자세한 근거는 `designall::selection_text_color` 문서). 예전엔 여기도 accent를
    // 그대로 써서 accent 배경 위 accent 글자가 되어 드래그 선택한 글자가 통째로
    // 사라졌다(2026-08-10 실증). 휘도 기반 대비색(흑/백)으로 바꾼다.
    //
    // accent **원색**이 필요한 곳은 `selection.bg_fill`을 읽는다(그쪽은 그대로 accent다).
    // 이 커밋에서 activity/file_tree/workspace 세 곳을 그렇게 옮겼다.
    v.selection.stroke = Stroke::new(1.0, crate::ui::designall::selection_text_color(accent));

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

#[cfg(test)]
mod tests {
    use super::*;

    /// HSL 색상(0~360)과 채도(0~1). 무채색이면 색상은 None.
    fn hue_sat(c: Color32) -> (Option<f32>, f32) {
        let [r, g, b] = [c.r(), c.g(), c.b()].map(|v| f32::from(v) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        if delta < 1e-6 {
            return (None, 0.0);
        }
        let lightness = (max + min) / 2.0;
        let sat = delta / (1.0 - (2.0 * lightness - 1.0).abs());
        let hue = if max == r {
            60.0 * ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };
        (Some(hue), sat)
    }

    #[track_caller]
    fn assert_on_axis(label: &str, color: Color32) {
        let (hue, sat) = hue_sat(color);
        let Some(hue) = hue else {
            panic!("{label}: 무채색이다 — 표면은 220도 축 위에 있어야 한다");
        };
        assert!(
            (210.0..=230.0).contains(&hue),
            "{label}: 색상 {hue:.0}도 — 220도 축을 벗어났다"
        );
        assert!(
            sat >= 0.05,
            "{label}: 채도 {:.0}% — 너무 낮아 무채색으로 보인다",
            sat * 100.0
        );
    }

    /// 2026-08-06: 같은 다크 테마가 세 번 구현돼 색상축이 210도(사이드바)·240도(앱
    /// 크롬)·무채색(설정 창)으로 갈려 있었다. 맞닿은 면들이 서로를 물들여 보이게 해서
    /// "설명할 수 없이 불편한" 화면이 됐다. 명도는 화면마다 달라도 되지만(사이드바가 더
    /// 어두운 위계는 의도된 것) 색상축은 하나여야 한다. 값을 손볼 때 다시 갈라지는 걸
    /// 이 테스트가 막는다 — 지금은 주석 말고는 막는 장치가 없다.
    #[test]
    fn 다크_표면은_세_팔레트가_같은_색상축을_공유한다() {
        let tokens = crate::ui::designall::DARK;
        for (label, color) in [
            ("designall app_background", tokens.app_background),
            (
                "designall workspace_background",
                tokens.workspace_background,
            ),
            ("designall input_background", tokens.input_background),
            ("designall selected_background", tokens.selected_background),
            ("designall hover_background", tokens.hover_background),
            ("designall separator", tokens.separator),
            ("designall content_canvas", tokens.content_canvas),
        ] {
            assert_on_axis(label, color);
        }

        let chrome = dark();
        for (label, color) in [
            ("theme panel_fill", chrome.panel_fill),
            ("theme extreme_bg_color", chrome.extreme_bg_color),
            ("theme faint_bg_color", chrome.faint_bg_color),
            ("theme hovered bg_fill", chrome.widgets.hovered.bg_fill),
            (
                "theme separator",
                chrome.widgets.noninteractive.bg_stroke.color,
            ),
        ] {
            assert_on_axis(label, color);
        }

        egui::__run_test_ui(|ui| {
            ui.visuals_mut().dark_mode = true;
            crate::ui::settings::apply_settings_palette(ui);
            let settings = ui.visuals();
            for (label, color) in [
                ("settings panel_fill", settings.panel_fill),
                ("settings window_fill", settings.window_fill),
                ("settings faint_bg_color", settings.faint_bg_color),
                ("settings extreme_bg_color", settings.extreme_bg_color),
                ("settings hovered bg_fill", settings.widgets.hovered.bg_fill),
                (
                    "settings separator",
                    settings.widgets.noninteractive.bg_stroke.color,
                ),
            ] {
                assert_on_axis(label, color);
            }
        });
    }

    /// 라이트도 같은 축을 쓴다. 2026-08-07 이전에는 다크만 정리해서 **두 모드의 구조가
    /// 서로 달랐다** — 라이트는 색상이 200~220도로 흩어지고 레일/사이드바가 2단이었다.
    /// 한쪽만 손보면 다시 갈라지므로 두 모드를 같은 잣대로 검사한다.
    #[test]
    fn 라이트_표면도_세_팔레트가_같은_색상축을_공유한다() {
        let tokens = crate::ui::designall::LIGHT;
        for (label, color) in [
            ("designall app_background", tokens.app_background),
            (
                "designall workspace_background",
                tokens.workspace_background,
            ),
            ("designall selected_background", tokens.selected_background),
            ("designall hover_background", tokens.hover_background),
            ("designall separator", tokens.separator),
            ("designall content_canvas", tokens.content_canvas),
        ] {
            assert_on_axis(label, color);
        }

        let chrome = light();
        for (label, color) in [
            ("theme panel_fill", chrome.panel_fill),
            ("theme faint_bg_color", chrome.faint_bg_color),
            ("theme hovered bg_fill", chrome.widgets.hovered.bg_fill),
            (
                "theme separator",
                chrome.widgets.noninteractive.bg_stroke.color,
            ),
        ] {
            assert_on_axis(label, color);
        }

        egui::__run_test_ui(|ui| {
            ui.visuals_mut().dark_mode = false;
            crate::ui::settings::apply_settings_palette(ui);
            let settings = ui.visuals();
            for (label, color) in [
                ("settings panel_fill", settings.panel_fill),
                ("settings window_fill", settings.window_fill),
                ("settings hovered bg_fill", settings.widgets.hovered.bg_fill),
                (
                    "settings separator",
                    settings.widgets.noninteractive.bg_stroke.color,
                ),
            ] {
                assert_on_axis(label, color);
            }
        });
    }

    /// 레일·사이드바·크롬은 두 모드 모두 **같은 단**이다(평평). 한쪽만 어긋나면 그
    /// 모드에서만 경계가 사라지거나 계단이 생긴다.
    #[test]
    fn 레일과_사이드바와_크롬은_두_모드_모두_같은_단이다() {
        for (mode, tokens, chrome) in [
            ("다크", crate::ui::designall::DARK, dark()),
            ("라이트", crate::ui::designall::LIGHT, light()),
        ] {
            assert_eq!(
                tokens.app_background, tokens.workspace_background,
                "{mode}: 레일과 사이드바 본문이 다른 단이다"
            );
            assert_eq!(
                tokens.app_background, tokens.folder_tree_background,
                "{mode}: 레일과 파일트리가 다른 단이다"
            );
            assert_eq!(
                tokens.app_background, chrome.panel_fill,
                "{mode}: 사이드바와 앱 크롬이 다른 단이다"
            );
        }
    }

    /// 2026-08-08: 홈·작업함을 띄우면 레일·사이드바·페이지 바닥·카드가 전부
    /// app_background라 화면이 "한 판"으로 보였다. 정보 페이지 바닥은 크롬에서
    /// 눈에 띄게 물러난 별도의 단이어야 하고, 그래야 그 위 카드(app_background)가
    /// 떠오른다. 값을 손보다 다시 같은 단으로 붙는 걸 막는다.
    #[test]
    fn 정보페이지_바닥은_크롬에서_한_단_물러나_있다() {
        for (mode, tokens) in [
            ("다크", crate::ui::designall::DARK),
            ("라이트", crate::ui::designall::LIGHT),
        ] {
            let canvas = tokens.content_canvas;
            let chrome = tokens.app_background;
            assert_ne!(canvas, chrome, "{mode}: 페이지 바닥이 크롬과 같은 단이다");

            // 밝기 차가 너무 작으면 색만 다르고 눈에는 한 판으로 보인다. 다크는
            // #181b20(L11) → #0f1115(L7), 라이트는 #f1f2f5(L95) → #e0e4ea(L90)로
            // 둘 다 4단계 이상 떨어져 있다.
            let step = |c: Color32| {
                let [r, g, b, _] = c.to_array().map(f32::from);
                (r.max(g).max(b) + r.min(g).min(b)) / 2.0
            };
            let gap = (step(canvas) - step(chrome)).abs();
            assert!(
                gap >= 8.0,
                "{mode}: 페이지 바닥과 크롬의 밝기 차 {gap:.1}/255 — 너무 붙어 한 판으로 보인다"
            );

            // 다크는 물러남 = 어두워짐, 라이트는 = 밝기가 낮아짐. 두 모드 모두
            // 바닥이 크롬보다 어두워야 카드가 위로 떠오른다.
            assert!(
                step(canvas) < step(chrome),
                "{mode}: 페이지 바닥이 크롬보다 밝다 — 카드가 가라앉는다"
            );
        }
    }
}
