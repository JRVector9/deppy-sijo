fn tree_keyboard_focus_id() -> egui::Id {
    egui::Id::new("file_tree_keyboard_focus")
}

fn register_tree_keyboard_focus_target(ui: &egui::Ui) {
    let response = ui.interact(
        egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::ZERO),
        tree_keyboard_focus_id(),
        egui::Sense::focusable_noninteractive(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), "파일 트리")
    });
}


fn terminal_keyboard_input_allowed(
    text_edit_focused: bool,
    popup_open: bool,
    top_window_open: bool,
    terminal_refocus_pending: bool,
) -> bool {
    // TextEditState는 widget이 사라진 뒤 한 프레임 더 memory에 남을 수 있다. terminal
    // refocus가 명시적으로 대기 중이면 그 stale 상태는 무시해야 첫 글자가 빠지지 않는다.
    !popup_open && !top_window_open && (terminal_refocus_pending || !text_edit_focused)
}


fn main(){
 let ctx=egui::Context::default();let mut tree_point=egui::Pos2::ZERO;
 for frame in 0..4{
  let events=match frame{1=>vec![egui::Event::PointerMoved(tree_point),egui::Event::PointerButton{pos:tree_point,button:egui::PointerButton::Primary,pressed:true,modifiers:egui::Modifiers::NONE}],2=>vec![egui::Event::PointerButton{pos:tree_point,button:egui::PointerButton::Primary,pressed:false,modifiers:egui::Modifiers::NONE}],_=>vec![]};
  let output=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(800.0,600.0))),events,..Default::default()},|ui|{
   let mut tree_ui=ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(300.0,400.0))));
   register_tree_keyboard_focus_target(&tree_ui);
   let (rect,_)=tree_ui.allocate_exact_size(egui::vec2(300.0,30.0),egui::Sense::hover());
   let row=tree_ui.interact(rect,egui::Id::new("tree_file_row"),egui::Sense::click_and_drag()); if frame==0{tree_point=row.rect.center();}
   if row.clicked(){tree_ui.memory_mut(|memory|memory.request_focus(tree_keyboard_focus_id()));}
   let before=ctx.memory(|memory|memory.has_focus(tree_keyboard_focus_id()));
   // Actual production gate and renderer_egui.rs:599–603 focus claim.
   let active=terminal_keyboard_input_allowed(ctx.text_edit_focused(),ctx.any_popup_open(),false,false);
   let mut terminal_ui=ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::pos2(300.0,0.0),egui::vec2(400.0,400.0))));
   let(_,response)=terminal_ui.allocate_exact_size(egui::vec2(400.0,400.0),egui::Sense::click_and_drag());
   if active&&!terminal_ui.memory(|memory|memory.owns_ime_events(response.id)){response.request_focus();}
   let after=ctx.memory(|memory|memory.has_focus(tree_keyboard_focus_id()));
   println!("frame={frame} tree_row_clicked={} tree_focus_before_terminal={before} terminal_active={active} tree_focus_after_terminal={after}",row.clicked());
   if frame==3{assert!(before,"CmdDelete/CmdOptionV require tree focus, but prior file click lost it");}
  });output.drop_without_applying_deltas();
 }
}
