mod ui {
    pub mod designall { pub struct Tokens {pub error: egui::Color32} pub fn tokens(_: &egui::Visuals)->Tokens { Tokens{error:egui::Color32::RED} } }
    #[path="/Users/jr/Desktop/projects/deppy-sijo-performance/crates/app/src/ui/context_menu.rs"] pub mod context_menu;
}
#[path="/Users/jr/Desktop/projects/deppy-sijo-performance/crates/terminal/src/input_mapper.rs"] mod input_mapper;
fn main() {
    let ctx=egui::Context::default();
    let mut target=egui::Pos2::ZERO;
    for frame in 0..6 {
        let events=match frame {
            1=>vec![egui::Event::PointerMoved(target),egui::Event::PointerButton{pos:target,button:egui::PointerButton::Secondary,pressed:true,modifiers:egui::Modifiers::NONE}],
            2=>vec![egui::Event::PointerButton{pos:target,button:egui::PointerButton::Secondary,pressed:false,modifiers:egui::Modifiers::NONE}],
            5=>vec![egui::Event::Key{key:egui::Key::Escape,physical_key:None,pressed:true,repeat:false,modifiers:egui::Modifiers::NONE}],
            _=>vec![]
        };
        let output=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(800.0,600.0))),events,..Default::default()},|root| {
            let popup_before=ctx.any_popup_open();
            let mut sidebar=root.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(300.0,300.0))));
            let r=sidebar.add(egui::Label::new("tree row").sense(egui::Sense::click()));
            if frame==0 { target=r.rect.center(); }
            let memory_before=egui::Popup::is_id_open(&ctx,egui::Popup::default_response_id(&r));
            ui::context_menu::show(&r,|ui| { ui.button("rename"); });
            let memory_after=egui::Popup::is_id_open(&ctx,egui::Popup::default_response_id(&r));
            let popup_after=ctx.any_popup_open();
            let active=!ctx.any_popup_open()&&!ctx.text_edit_focused();
            let mut terminal=root.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::pos2(300.0,0.0),egui::vec2(400.0,400.0))));
            let (_,r)=terminal.allocate_exact_size(egui::vec2(400.0,400.0),egui::Sense::click_and_drag());
            if active&&!terminal.memory(|m|m.owns_ime_events(r.id)){r.request_focus();}
            let accepts=active&&terminal.memory(|m|m.owns_ime_events(r.id))&&!ctx.text_edit_focused()&&!ctx.any_popup_open();
            let bytes=if accepts {terminal.input(|i|i.raw.events.iter().filter_map(|e|input_mapper::map_event(e,false,&i.modifiers)).flatten().collect::<Vec<u8>>())} else {vec![]};
            println!("frame={frame} popup_before={popup_before} popup_after={popup_after} memory_before={memory_before} memory_after={memory_after} terminal_accepts={accepts} PTY_BYTES={bytes:?}");
            if frame==5 { assert!(memory_before,"precondition: popup memory open"); assert!(!memory_after,"precondition: Escape closed popup memory"); assert!(bytes.is_empty(),"context-menu Escape leaked {bytes:?} to production PTY mapper"); }
        });
        output.drop_without_applying_deltas();
    }
}
