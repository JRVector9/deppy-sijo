use super::*;
use egui_kittest::kittest::Queryable as _;

fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
    let luminance = |color: egui::Color32| {
        let linear = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    };
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn text_shape<'a>(output: &'a egui::FullOutput, text: &str) -> &'a egui::epaint::TextShape {
    output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(shape) if shape.galley.text() == text => Some(shape),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing painted text: {text}"))
}

#[test]
fn design_rule_selected_text_and_theme_labels_have_readable_contrast() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let harness = egui_kittest::Harness::builder()
            .with_theme(theme)
            .build_ui(|ui| {
                apply_settings_palette(ui);
                apply_component_style(ui);
                assert!(
                    contrast(
                        ui.visuals().selection.stroke.color,
                        ui.visuals().selection.bg_fill
                    ) >= 4.5
                );
                segmented(
                    ui,
                    &mut Theme::Dark,
                    &[(Theme::Dark, Icon::Moon, "Selected theme".into())],
                );
            });
        let text = text_shape(harness.output(), "Selected theme");
        let bg = if theme == egui::Theme::Dark {
            rgb(0x2e, 0x3e, 0x5e)
        } else {
            rgb(0xcc, 0xd7, 0xed)
        };
        assert!(
            contrast(text.fallback_color, bg) >= 4.5,
            "selected theme contrast: {}",
            contrast(text.fallback_color, bg)
        );
    }
}

#[test]
fn design_rule_warnings_remain_readable_in_both_themes() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let _harness = egui_kittest::Harness::builder()
            .with_theme(theme)
            .build_ui(|ui| {
                apply_settings_palette(ui);
                assert!(contrast(ui.visuals().warn_fg_color, ui.visuals().extreme_bg_color) >= 4.5);
                assert!(contrast(ui.visuals().warn_fg_color, ui.visuals().panel_fill) >= 4.5);
            });
    }
}

#[test]
fn design_rule_standard_typography_is_rendered_in_management_controls() {
    let harness = egui_kittest::Harness::new_ui(|ui| {
        apply_component_style(ui);
        let _ = ui.button("Control type");
        ui.heading("Page type");
    });
    for (label, expected) in [("Control type", 14.0), ("Page type", 15.0)] {
        let shape = text_shape(harness.output(), label);
        assert_eq!(
            shape.galley.job.sections[0].format.font_id.size, expected,
            "{label}"
        );
    }
}

#[test]
fn design_rule_mobile_hostname_has_the_same_height_as_its_action() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let detect = catalog.t("settings.mobile_web.detect", &[]);
    let harness = egui_kittest::Harness::new_ui(move |ui| {
        apply_settings_palette(ui);
        apply_component_style(ui);
        mobile_web_page(
            ui,
            &mut Config::default(),
            &WebRemoteView {
                running: false,
                addr: None,
                url: None,
                error: None,
                ts_detect: TsDetectView::Idle,
                serve: ServeView::Idle,
            },
            &mut false,
            &mut None,
            &mut false,
            &mut WebRemoteAction::None,
            &catalog,
        );
    });
    let input = harness.get_by_role(egui::accesskit::Role::TextInput).rect();
    let button = harness.get_by_label(&detect).rect();
    assert!(
        (input.height() - 34.0).abs() < 0.1,
        "hostname input: {input:?}"
    );
    assert!((input.center().y - button.center().y).abs() < 0.1);
}

#[test]
fn design_rule_long_descriptions_wrap_without_clipping_or_overlapping_next_row() {
    let description = "A long setting description must remain readable when a narrow settings window leaves little room beside the controls.";
    let harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(658.0, 700.0))
        .build_ui(|ui| {
            apply_component_style(ui);
            row(ui, "Setting title", Some(description), |ui| {
                let _ = ui.button("Action");
            });
            row(ui, "Next setting", None, |_| {});
        });
    let description_shape = text_shape(harness.output(), description);
    assert!(
        description_shape.galley.rows.len() > 1,
        "description is still a clipped single line"
    );
    assert!(description_shape.galley.size().x <= 278.0);
    let next = text_shape(harness.output(), "Next setting");
    assert!(description_shape.pos.y + description_shape.galley.size().y <= next.pos.y);
}

#[test]
fn design_rule_general_toggles_expose_their_actual_setting_name() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let label = catalog.t("settings.file_tree_sidebar", &[]);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1100.0, 1800.0))
        .build_ui_state(
            move |ui, config: &mut Config| {
                apply_component_style(ui);
                general_page(ui, config, &mut false, &catalog);
            },
            Config::default(),
        );
    let initial = harness.state().ui.file_tree_enabled;
    harness
        .get_by_role_and_label(egui::accesskit::Role::CheckBox, &label)
        .focus();
    harness.run();
    harness.key_press(egui::Key::Space);
    harness.run();
    assert_ne!(harness.state().ui.file_tree_enabled, initial);
    assert!(harness.get_by_label(&label).is_focused());
}

#[test]
fn design_rule_custom_select_exposes_name_and_paints_keyboard_focus() {
    let mut harness = egui_kittest::Harness::new_ui(|ui| {
        apply_settings_palette(ui);
        settings_select_button(ui, "Selected font value", "Selected font");
    });
    let control = harness.get_by_role_and_label(egui::accesskit::Role::ComboBox, "Selected font");
    control.focus();
    harness.run();
    assert!(harness.get_by_label("Selected font").is_focused());
    let focused = harness
        .output()
        .shapes
        .iter()
        .any(|shape| match &shape.shape {
            egui::Shape::Rect(rect) => rect.stroke.width >= 2.0,
            _ => false,
        });
    assert!(focused, "custom select must paint a visible focus stroke");
}

#[test]
fn design_rule_management_content_uses_the_shared_shell_inset_once() {
    for category in [
        Category::Connectors,
        Category::Agents,
        Category::Notifications,
    ] {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1500.0, 1000.0))
            .build_ui(move |ui| {
                let catalog = i18n::Catalog::load("en-US").unwrap();
                let remote = RemoteView {
                    running: false,
                    addr: None,
                    fingerprint: None,
                    token: None,
                    error: None,
                    known_hosts_path: String::new(),
                    known_hosts: &[],
                };
                let web = WebRemoteView {
                    running: false,
                    addr: None,
                    url: None,
                    error: None,
                    ts_detect: TsDetectView::Idle,
                    serve: ServeView::Idle,
                };
                let relay = RelayView {
                    running: false,
                    connection: RelayConnectionView::Disabled,
                    error: None,
                    pairing: RelayPairingView::NotReady,
                    devices: &[],
                    now: 0,
                };
                let mut selected_category = category;
                let _ = show(
                    ui.ctx(),
                    &mut true,
                    &mut selected_category,
                    &mut Config::default(),
                    &remote,
                    &mut false,
                    &web,
                    &mut false,
                    &mut None,
                    &relay,
                    &mut None,
                    0,
                    &mut String::new(),
                    &catalog,
                    crate::scrollback_policy::View::default(),
                    |ui, _| {
                        ui.label("Managed content");
                    },
                );
            });
        harness.run();
        let content = harness.get_by_label("Managed content").rect();
        let general = harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "General")
            .rect();
        let search = harness.get_by_role(egui::accesskit::Role::TextInput).rect();
        assert!(
            (content.left() - general.left() - 208.0).abs() < 1.0,
            "{category:?}: {content:?} nav {general:?}"
        );
        assert!(
            (content.top() - search.top() - 6.0).abs() < 1.0,
            "{category:?}: {content:?} search {search:?}"
        );
    }
}

#[test]
fn design_rule_theme_options_expose_selection_and_keyboard_actions() {
    let mut harness = egui_kittest::Harness::new_ui_state(
        |ui, theme: &mut Theme| {
            apply_settings_palette(ui);
            segmented(
                ui,
                theme,
                &[
                    (Theme::Light, Icon::Sun, "Light option".into()),
                    (Theme::Dark, Icon::Moon, "Dark option".into()),
                ],
            );
        },
        Theme::Light,
    );
    harness
        .get_by_role_and_label(egui::accesskit::Role::RadioButton, "Dark option")
        .focus();
    harness.run();
    harness.key_press(egui::Key::Space);
    harness.run();
    assert_eq!(harness.state(), &Theme::Dark);
    assert!(harness.get_by_label("Dark option").is_focused());
    assert!(
        harness.output().shapes.iter().any(
            |shape| matches!(&shape.shape, egui::Shape::Rect(rect) if rect.stroke.width >= 2.0)
        )
    );
}

#[test]
fn design_rule_narrow_descriptions_remain_inside_their_column_in_all_locales() {
    for locale in ["en-US", "ko-KR", "ja-JP", "zh-Hans", "zh-Hant"] {
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let catalog = i18n::Catalog::load(locale).unwrap();
            let description = catalog.t("settings.composer_enabled.hint", &[]);
            let mut harness = egui_kittest::Harness::builder()
                .with_theme(theme)
                .with_size(egui::vec2(710.0, 2500.0))
                .build_ui_state(
                    move |ui, config: &mut Config| {
                        apply_settings_palette(ui);
                        apply_component_style(ui);
                        settings_detail_shell(ui, |ui| {
                            general_page(ui, config, &mut false, &catalog)
                        });
                    },
                    Config::default(),
                );
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "D2Coding", "Regular");
            harness.run();
            let shape = text_shape(harness.output(), &description);
            assert!(shape.galley.rows.len() > 1, "{locale} {theme:?}");
            assert!(
                shape.galley.size().x <= 278.0,
                "{locale}: {:?}",
                shape.galley.size()
            );
        }
    }
}

#[test]
fn design_rule_preset_and_agent_single_line_fields_have_34pt_hit_areas() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let presets = egui_kittest::Harness::new_ui(move |ui| {
        apply_component_style(ui);
        let mut config = Config::default();
        config.ui.agent_send_presets = vec!["Preset".to_owned()];
        agent_send_presets_section(ui, &mut config, &mut false, &catalog);
    });
    assert!(
        (presets
            .get_by_role(egui::accesskit::Role::TextInput)
            .rect()
            .height()
            - 34.0)
            .abs()
            < 0.1
    );
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let snapshot = crate::ui::agents::AgentsSnapshot::try_new(1, vec![], vec![], vec![]).unwrap();
    let mut agents_ui = crate::ui::agents::AgentsUi::new();
    let agents = egui_kittest::Harness::new_ui(move |ui| {
        apply_component_style(ui);
        let _ = agents_ui.contents(ui, &snapshot, &catalog);
    });
    let inputs: Vec<_> = agents
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .map(|node| node.rect())
        .collect();
    assert_eq!(inputs.len(), 2);
    // 이름/명령은 단일행이고 인수 입력은 별도 multiline 역할과 기존 3행을 유지한다.
    assert!(
        inputs.iter().all(|rect| (rect.height() - 34.0).abs() < 0.1),
        "{inputs:?}"
    );
    assert!(
        agents
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .rect()
            .height()
            > 34.0
    );
}

#[test]
#[ignore = "명시적으로 요청한 오프스크린 시각 검증에서만 GPU를 초기화한다"]
fn design_rule_render_native_review_images() {
    let path = std::path::PathBuf::from(
        std::env::var_os("DEPPY_SETTINGS_DESIGN_IMAGES").expect("set image output directory"),
    );
    std::fs::create_dir_all(&path).unwrap();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        for (category, width, suffix) in [
            (Category::General, 710.0, ""),
            (Category::MobileWeb, 710.0, ""),
            (Category::Agents, 710.0, ""),
            (Category::Connectors, 710.0, ""),
            (Category::Notifications, 710.0, ""),
            (Category::General, 410.0, "-narrow"),
        ] {
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let snapshot =
                crate::ui::agents::AgentsSnapshot::try_new(1, vec![], vec![], vec![]).unwrap();
            let mut agents = crate::ui::agents::AgentsUi::new();
            let mut connectors = connector_ui::ConnectorUi::new(&catalog);
            let mut notifications = crate::ui::notifications::NotificationsUi::new();
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(width, 730.0))
                .with_theme(theme)
                .build_ui_state(
                    move |ui, config: &mut Config| {
                        apply_settings_palette(ui);
                        apply_component_style(ui);
                        ui.painter()
                            .rect_filled(ui.max_rect(), 0.0, ui.visuals().extreme_bg_color);
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            settings_detail_shell(ui, |ui| match category {
                                Category::General => general_page(ui, config, &mut false, &catalog),
                                Category::MobileWeb => mobile_web_page(
                                    ui,
                                    config,
                                    &WebRemoteView {
                                        running: false,
                                        addr: None,
                                        url: None,
                                        error: Some("연결 상태를 확인해 주세요."),
                                        ts_detect: TsDetectView::NoCli,
                                        serve: ServeView::Idle,
                                    },
                                    &mut false,
                                    &mut None,
                                    &mut false,
                                    &mut WebRemoteAction::None,
                                    &catalog,
                                ),
                                Category::Agents => {
                                    let _ = agents.contents(ui, &snapshot, &catalog);
                                }
                                Category::Connectors => {
                                    let _ = connectors.render(
                                        ui,
                                        &connector_contract::ConnectorSnapshot::default(),
                                    );
                                }
                                Category::Notifications => {
                                    let _ = notifications.contents(ui, &catalog);
                                }
                                _ => unreachable!(),
                            })
                        });
                    },
                    Config::default(),
                );
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "D2Coding", "Regular");
            harness.run();
            harness
                .render()
                .unwrap()
                .save(path.join(format!("settings-{category:?}-{theme:?}{suffix}.png")))
                .unwrap();
        }
    }
}

#[test]
fn design_rule_native_text_input_keeps_accent_focus() {
    let mut failures = Vec::new();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let mut harness = egui_kittest::Harness::builder()
            .with_theme(theme)
            .build_ui_state(
                |ui, value: &mut String| {
                    apply_settings_palette(ui);
                    apply_component_style(ui);
                    settings_text_input(ui, 230.0, egui::TextEdit::singleline(value));
                },
                "Focus this actual settings field".to_owned(),
            );
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .focus();
        harness.run();
        let input = harness.get_by_role(egui::accesskit::Role::TextInput).rect();
        let mut expected = None;
        let mut text_color = None;
        // 실제 설정 팔레트에서 포커스색과 본문색을 확인한다.
        let palette = egui_kittest::Harness::builder()
            .with_theme(theme)
            .build_ui(|ui| {
                apply_settings_palette(ui);
                expected = Some(ui.visuals().hyperlink_color);
                text_color = Some(ui.visuals().text_color());
            });
        drop(palette);
        let strokes: Vec<_> = harness
            .output()
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Rect(rect) = &shape.shape
                    && rect.stroke.width > 0.0
                    && rect.rect.intersects(input)
                    && (rect.rect.width() - input.width()).abs() <= 4.0
                    && (rect.rect.height() - input.height()).abs() <= 4.0
                {
                    return Some((rect.rect, rect.stroke));
                }
                None
            })
            .collect();
        eprintln!(
            "FOCUS {theme:?}: input={input:?}; accent={expected:?}; text={text_color:?}; strokes={strokes:?}"
        );
        if !strokes
            .iter()
            .any(|(_, stroke)| Some(stroke.color) == expected)
        {
            failures.push(format!(
                "{theme:?}: no accent focus border, actual {strokes:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn design_rule_narrow_effective_body_keeps_readable_rows() {
    let description = "A long setting description must remain readable when a narrow settings window leaves little room beside the controls.";
    let harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(410.0, 3000.0))
        .build_ui(|ui| {
            apply_settings_palette(ui);
            apply_component_style(ui);
            settings_detail_shell(ui, |ui| {
                row(ui, "Setting title", Some(description), |ui| {
                    let _ = ui.button("Action");
                });
                row(ui, "Next setting", None, |_| {});
            });
        });
    let shape = harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape
                && text.galley.text() == description
            {
                return Some((shape.clip_rect, text));
            }
            None
        })
        .expect("actual description paint call");
    let next = text_shape(harness.output(), "Next setting");
    eprintln!(
        "NARROW: description_clip={:?}; text_size={:?}; rows={}; next_top={}",
        shape.0,
        shape.1.galley.size(),
        shape.1.galley.rows.len(),
        next.pos.y
    );
    assert!(
        shape.0.width() >= 80.0 && next.pos.y < 300.0,
        "narrow body hides text and reserves an excessive blank row: clip={:?}, next_top={}",
        shape.0,
        next.pos.y
    );
}

#[test]
fn design_rule_shortcut_recording_text_has_readable_contrast() {
    let mut failures = Vec::new();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let recording = catalog.t("shortcuts.recording", &[]);
        let mut harness = egui_kittest::Harness::builder()
            .with_theme(theme)
            .with_size(egui::vec2(1100.0, 2200.0))
            .build_ui_state(
                move |ui, config: &mut Config| {
                    apply_settings_palette(ui);
                    apply_component_style(ui);
                    ui.ctx().data_mut(|data| {
                        data.insert_temp(
                            egui::Id::new("settings_shortcuts_page_state"),
                            ShortcutPageState {
                                recording: Some(ShortcutAction::ToggleSidebar),
                                ..Default::default()
                            },
                        )
                    });
                    shortcuts_page(ui, config, &mut false, &catalog);
                },
                Config::default(),
            );
        harness.get_by_label(&recording).focus();
        harness.run();
        let button = harness.get_by_label(&recording).rect();
        let text = text_shape(harness.output(), &recording);
        let color = text.galley.job.sections[0].format.color;
        let foreground = if color == egui::Color32::PLACEHOLDER {
            text.fallback_color
        } else {
            color
        };
        let backgrounds: Vec<_> = harness
            .output()
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Rect(rect) = &shape.shape
                    && rect.rect.contains(button.center())
                    && (rect.rect.width() - button.width()).abs() <= 4.0
                    && (rect.rect.height() - button.height()).abs() <= 4.0
                {
                    return Some((rect.fill, contrast(foreground, rect.fill)));
                }
                None
            })
            .collect();
        eprintln!(
            "RECORDING {theme:?}: text={recording}; button={button:?}; foreground={foreground:?}; backgrounds={backgrounds:?}"
        );
        if backgrounds.is_empty() || backgrounds.iter().any(|(_, ratio)| *ratio < 4.5) {
            failures.push(format!("{theme:?}: recording contrast {backgrounds:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn design_rule_actual_search_and_multiline_fields_keep_accent_focus() {
    let mut failures = Vec::new();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        for kind in 0..3 {
            let catalog = i18n::Catalog::load("en-US").unwrap();
            let snapshot =
                crate::ui::agents::AgentsSnapshot::try_new(1, vec![], vec![], vec![]).unwrap();
            let mut agents = crate::ui::agents::AgentsUi::new();
            let mut connectors = connector_ui::ConnectorUi::new(&catalog);
            let mut query = String::new();
            let mut harness = egui_kittest::Harness::builder()
                .with_theme(theme)
                .with_size(egui::vec2(1100.0, 1000.0))
                .build_ui(move |ui| {
                    apply_settings_palette(ui);
                    apply_component_style(ui);
                    match kind {
                        0 => {
                            let _ = nav(ui, Category::General, 0, &mut query, &catalog);
                        }
                        1 => {
                            let _ = agents.contents(ui, &snapshot, &catalog);
                        }
                        _ => {
                            let _ = connectors
                                .render(ui, &connector_contract::ConnectorSnapshot::default());
                        }
                    }
                });
            let role = if kind == 0 {
                egui::accesskit::Role::TextInput
            } else {
                egui::accesskit::Role::MultilineTextInput
            };
            harness.get_by_role(role).focus();
            harness.run();
            let input = harness.get_by_role(role).rect();
            let expected = if theme == egui::Theme::Dark {
                crate::ui::designall::DARK.accent
            } else {
                crate::ui::designall::LIGHT.accent
            };
            let last = harness
                .output()
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Rect(rect) = &shape.shape
                        && rect.stroke.width > 0.0
                        && rect.rect.intersects(input)
                        && (rect.rect.width() - input.width()).abs() <= 4.0
                        && (rect.rect.height() - input.height()).abs() <= 4.0
                    {
                        return Some(rect);
                    }
                    None
                })
                .next_back();
            if !last.is_some_and(|rect| {
                rect.stroke.color == expected && rect.stroke.width >= 2.0 && rect.rect == input
            }) {
                failures.push(format!("{theme:?} field {kind}: {last:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn design_rule_actual_narrow_localized_rows_place_controls_below_descriptions() {
    for locale in ["en-US", "ko-KR", "ja-JP", "zh-Hans", "zh-Hant"] {
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let catalog = i18n::Catalog::load(locale).unwrap();
            let description = catalog.t("settings.composer_enabled.hint", &[]);
            let label = catalog.t("settings.composer_enabled", &[]);
            let mut harness = egui_kittest::Harness::builder()
                .with_theme(theme)
                .with_size(egui::vec2(410.0, 4000.0))
                .build_ui_state(
                    move |ui, config: &mut Config| {
                        apply_settings_palette(ui);
                        apply_component_style(ui);
                        settings_detail_shell(ui, |ui| {
                            general_page(ui, config, &mut false, &catalog)
                        });
                    },
                    Config::default(),
                );
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "D2Coding", "Regular");
            harness.run();
            let text = text_shape(harness.output(), &description);
            let control = harness
                .get_by_role_and_label(egui::accesskit::Role::CheckBox, &label)
                .rect();
            let clip = harness
                .output()
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == description => {
                        Some(shape.clip_rect)
                    }
                    _ => None,
                })
                .unwrap();
            assert!(clip.width() >= 200.0, "{locale} {theme:?}: {clip:?}");
            assert!(
                text.pos.x + text.galley.size().x <= clip.right() + 0.1,
                "{locale} {theme:?}"
            );
            assert!(
                text.pos.y + text.galley.size().y <= control.top(),
                "{locale} {theme:?}: text={text:?}, control={control:?}"
            );
        }
    }
}

#[test]
fn design_rule_api_fields_keep_accent_focus() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let snapshot = crate::ui::credentials::CredentialsSnapshot::try_new(1, vec![]).unwrap();
        let mut credentials = crate::ui::credentials::CredentialsUi::new();
        credentials.prefill_modern(
            crate::ui::environment::EnvironmentSelectionKind::ApiName,
            crate::ui::credentials::SensitiveInput::try_new("Focus test".into()).unwrap(),
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_theme(theme)
            .with_size(egui::vec2(700.0, 1400.0))
            .build_ui(move |ui| {
                apply_settings_palette(ui);
                apply_component_style(ui);
                credentials.modern_form(ui, &snapshot, &catalog, &mut None);
            });
        for (role, count) in [
            (egui::accesskit::Role::TextInput, 3),
            (egui::accesskit::Role::PasswordInput, 1),
        ] {
            assert_eq!(harness.get_all_by_role(role).count(), count);
            for index in 0..count {
                harness.get_all_by_role(role).nth(index).unwrap().focus();
                harness.run();
                let input = harness.get_all_by_role(role).nth(index).unwrap().rect();
                let expected = if theme == egui::Theme::Dark {
                    crate::ui::designall::DARK.accent
                } else {
                    crate::ui::designall::LIGHT.accent
                };
                let last = harness
                    .output()
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Rect(rect) = &shape.shape
                            && rect.stroke.width > 0.0
                            && rect.rect == input
                        {
                            return Some(rect);
                        }
                        None
                    })
                    .next_back();
                assert!(
                    last.is_some_and(
                        |rect| rect.stroke.color == expected && rect.stroke.width >= 2.0
                    ),
                    "{theme:?} API {role:?} field {index}: {last:?}"
                );
            }
        }
    }
}
