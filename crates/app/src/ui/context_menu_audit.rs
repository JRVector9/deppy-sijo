//! Opt-in offscreen evidence for the HTML/source menu comparison.
//! This calls the production renderers; no app process or runtime is started.
use egui_kittest::kittest::{NodeT, Queryable};

pub(crate) fn prepare(ctx: &egui::Context) {
    crate::theme::install_palette(ctx);
    ctx.set_theme(egui::Theme::Dark);
    crate::fonts::install_cjk_fallback(ctx, None, "JetBrainsMono", "Regular");
}

pub(crate) fn save<State>(harness: &mut egui_kittest::Harness<'_, State>, name: &str) {
    harness.remove_cursor();
    harness.run();
    let rect = harness.ctx.memory(|memory| {
        let layer = memory
            .areas()
            .top_layer_id(egui::Order::Foreground)
            .expect("menu layer");
        memory.area_rect(layer.id).expect("actual popup bounds")
    });
    let mut rows: Vec<_> = harness
        .query_all_by_role(egui::accesskit::Role::Button)
        // A child popup can overlap a parent's last row. Count only whole rows
        // inside the captured popup, not a parent row whose center is behind it.
        .filter(|node| rect.shrink(3.0).contains_rect(node.rect()))
        .map(|node| {
            let item = node.rect();
            let ax = node.accesskit_node();
            serde_json::json!({
                "label": ax.label().unwrap_or_default(),
                "disabled": ax.is_disabled(),
                "rect": [item.left(), item.top(), item.width(), item.height()]
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a["rect"][1]
            .as_f64()
            .unwrap()
            .total_cmp(&b["rect"][1].as_f64().unwrap())
    });
    assert!(!rows.is_empty(), "empty menu evidence: {name}");
    let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/mockups/context-menu-audit-assets");
    std::fs::create_dir_all(&output).unwrap();
    let rendered = harness.render().expect("offscreen egui render");
    rendered.save(output.join(format!("{name}.png"))).unwrap();
    let evidence = serde_json::json!({
        "id": name,
        "rect": [rect.left(), rect.top(), rect.width(), rect.height()],
        "pixels_per_point": harness.ctx.pixels_per_point(),
        "image_pixels": [rendered.width(), rendered.height()],
        "rows": rows,
        "font": "Apple SD Gothic Neo (app default)",
        "font_points": harness.ctx.style_of(egui::Theme::Dark).text_styles[&egui::TextStyle::Button].size,
        "row_gap_points": super::context_menu::ROW_GAP,
        "image": format!("context-menu-audit-assets/{name}.png")
    });
    std::fs::write(
        output.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    eprintln!("MENU_AUDIT {name} {} x {}", rect.width(), rect.height());
}
