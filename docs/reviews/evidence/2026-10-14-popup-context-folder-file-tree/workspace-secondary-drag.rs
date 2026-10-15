fn track_workspace_drag(
    response: &egui::Response,
    workspace_id: &str,
    drag: &mut Option<String>,
) -> bool {
    if response.drag_started() {
        *drag = Some(workspace_id.to_owned());
    }
    if response.dragged() {
        response.ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    // Escape로 **취소한** 드래그도 `drag_stopped`를 낸다 — egui는 Escape에서 dragged id를
    // 지우기 때문이다(egui 0.35 `interaction.rs`). 버튼이 실제로 떨어졌는지까지 보는
    // `drag_stopped_by`만이 취소와 드롭을 가른다. 덤으로 왼쪽 버튼으로 놓은 것만 확정돼
    // 오른쪽 버튼으로 끌다 뗀 것이 순서를 바꾸지 않는다(2026-09-03 리뷰 H1).
    response.drag_stopped_by(egui::PointerButton::Primary) && drag.as_deref() == Some(workspace_id)
}


fn drag_autoscroll_delta(viewport: egui::Rangef, pointer_y: f32, dt: f32) -> f32 {
    /// 가장자리로 인정하는 두께(pt) — 행 하나(29pt)보다 약간 작아 실수로 걸리지 않는다.
    const EDGE: f32 = 24.0;
    /// 최대 속도(pt/s) — 화면 하나를 1초 남짓에 훑는 정도.
    const MAX_SPEED: f32 = 600.0;

    // 뷰포트가 가장자리 둘을 담지 못할 만큼 짧으면 위·아래 구역이 겹쳐 방향이 튄다.
    let edge = EDGE.min(viewport.span() / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let ratio = if pointer_y < viewport.min + edge {
        ((viewport.min + edge - pointer_y) / edge).min(1.0)
    } else if pointer_y > viewport.max - edge {
        -((pointer_y - (viewport.max - edge)) / edge).min(1.0)
    } else {
        return 0.0;
    };
    ratio * MAX_SPEED * dt
}


fn main(){
 let ctx=egui::Context::default(); let mut point=egui::Pos2::ZERO; let mut drag=None;
 let mut saw_secondary_drag=false; let mut final_offset=0.0;
 for frame in 0..6 {
  let pos=if frame>=2{egui::pos2(point.x,295.0)}else{point};
  let events=match frame{1=>vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton{pos,button:egui::PointerButton::Secondary,pressed:true,modifiers:egui::Modifiers::NONE}],2=>vec![egui::Event::PointerMoved(pos)],_=>vec![]};
  let output=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(400.0,300.0))),events,..Default::default()},|ui|{
   let out=egui::ScrollArea::vertical().id_salt("workspace_list_scroll").auto_shrink([false,false]).show(ui,|ui|{
    for row in 0..30 {let(rect,_)=ui.allocate_exact_size(egui::vec2(300.0,30.0),egui::Sense::hover()); let response=ui.interact(rect,egui::Id::new(("workspace",row)),egui::Sense::click_and_drag()); if row==0&&frame==0{point=rect.center();} let _released=track_workspace_drag(&response,&format!("workspace-{row}"),&mut drag);}
    if let Some(dragged)=&drag{let pointer=ctx.pointer_interact_pos().unwrap(); let delta=drag_autoscroll_delta(ui.clip_rect().y_range(),pointer.y,ui.input(|i|i.stable_dt)); println!("frame={frame} SECONDARY={} drag={dragged} autoscroll_delta={delta}",ui.input(|i|i.pointer.secondary_down())); if ui.input(|i|i.pointer.secondary_down()){saw_secondary_drag=true;} if delta!=0.0{ui.scroll_with_delta_animation(egui::vec2(0.0,delta),egui::style::ScrollAnimation::none());ctx.request_repaint();}}
   });
   final_offset=out.state.offset.y; println!("frame={frame} scroll_offset={final_offset}");
  });output.drop_without_applying_deltas();
 }
 assert!(saw_secondary_drag,"precondition secondary drag detected"); assert_eq!(final_offset,0.0,"right-button drag scrolled workspace list");
}
