fn main(){
 let ctx=egui::Context::default();let mut buffer=String::from("b.txt");let mut focus=true;let mut old_id=None;
 for frame in 0..5{
  let rows:Vec<&str>=if frame<2{vec!["b.txt","c.txt"]}else{vec!["a.txt","b.txt","c.txt"]};
  let events=if frame==3{vec![egui::Event::Text(String::from("x"))]}else{vec![]};
  let output=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(400.0,600.0))),events,..Default::default()},|ui|{
   egui::ScrollArea::vertical().id_salt("file_tree_rows").auto_shrink([false,false]).show_rows(ui,25.0,rows.len(),|ui,range|{
    let _background=ui.interact(ui.clip_rect(),ui.id().with("tree_background"),egui::Sense::click_and_drag());
    for index in range{
     let name=rows[index];
     if name=="b.txt"{
      ui.horizontal(|ui|{
       ui.add_space(12.0);
       let resp=ui.add(egui::TextEdit::singleline(&mut buffer).margin(egui::Margin::ZERO).desired_width(f32::INFINITY));
       if focus{resp.request_focus();focus=false;}
       if frame==0{old_id=Some(resp.id);}
       println!("frame={frame} row_index={index} old_id={old_id:?} current_id={:?} focused={} buffer={buffer}",resp.id,resp.has_focus());
       if frame==4{assert!(resp.has_focus(),"watcher inserted preceding sibling: rename target remains visible but loses its input id/focus");}
      });continue;
     }
     // Allocation flow from file_tree.rs:3523–3599, decorative paint omitted.
     let response=ui.scope(|ui|{
      ui.allocate_ui_with_layout(egui::vec2(ui.available_width(),25.0),egui::Layout::left_to_right(egui::Align::Center),|ui|{
       ui.spacing_mut().item_spacing.x=5.0;ui.add_space(10.0);
       let _=ui.allocate_exact_size(egui::vec2(10.0,16.0),egui::Sense::hover());
       let _=ui.allocate_exact_size(egui::vec2(17.0,16.0),egui::Sense::hover());
       ui.add(egui::Label::new(egui::RichText::new(name).size(12.5)).sense(egui::Sense::click()).truncate())
      }).inner
     }).response;
     let row_rect=egui::Rect::from_min_max(egui::pos2(ui.max_rect().left(),response.rect.min.y),egui::pos2(ui.max_rect().right(),response.rect.max.y));
     let _row_response=ui.interact(row_rect,egui::Id::new(("file_tree_row",name)).with("row"),egui::Sense::click_and_drag());
    }
   });
  });output.drop_without_applying_deltas();
 }
}
