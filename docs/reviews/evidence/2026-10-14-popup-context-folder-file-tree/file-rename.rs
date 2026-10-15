#[path="/Users/jr/Desktop/projects/deppy-sijo-performance/crates/terminal/src/input_mapper.rs"]
mod input_mapper;
fn main() {
    let mut violations=0;
    for key in [egui::Key::Enter, egui::Key::Escape] {
        let ctx = egui::Context::default();
        let mut text=String::from("old.txt");
        let mut focus=true;
        for frame in 0..3 {
            let mut done=None;
            let input=egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(800.0,600.0))),
                events: if frame==2 { vec![egui::Event::Key{key, physical_key: None, pressed: true, repeat:false, modifiers:egui::Modifiers::NONE}] } else { vec![] },
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let resp=ui.add(egui::TextEdit::singleline(&mut text).margin(egui::Margin::ZERO).desired_width(f32::INFINITY));
                    if focus { resp.request_focus(); focus=false; }
                    if resp.lost_focus() && ui.input(|i|i.key_pressed(egui::Key::Enter)) { done=Some(true); }
                    else if ui.input(|i|i.key_pressed(egui::Key::Escape)) { done=Some(false); }
                    if frame==2 { println!("key={key:?} has_focus={} lost_focus={} done={done:?} text_edit_focused={} raw={:?} events={:?}", resp.has_focus(),resp.lost_focus(),ctx.text_edit_focused(),ui.input(|i|i.raw.events.clone()),ui.input(|i|i.events.clone())); }
                });
                // Exact production terminal_keyboard_input_allowed, no modal/popups/refocus.
                let active=!ctx.any_popup_open() && !ctx.text_edit_focused();
                let (_,resp)=ui.allocate_exact_size(egui::vec2(300.0,100.0),egui::Sense::click_and_drag());
                if active && !ui.memory(|m|m.owns_ime_events(resp.id)) { resp.request_focus(); }
                let accepts=active && ui.memory(|m|m.owns_ime_events(resp.id)) && !ctx.text_edit_focused() && !ctx.any_popup_open();
                if frame==2 {
                    let bytes=if accepts { ui.input(|i|i.raw.events.iter().filter_map(|e|input_mapper::map_event(e,false,&i.modifiers)).flatten().collect::<Vec<u8>>()) } else {vec![]};
                    println!("key={key:?} terminal_active={active} terminal_accepts={accepts} PTY_BYTES={bytes:?}");
                    if !bytes.is_empty() { violations += 1; }
                }
            });
            output.drop_without_applying_deltas();
        }
    }    assert_eq!(violations,0,"file rename submit/cancel key leaked into production PTY mapper");
}
