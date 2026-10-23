use std::sync::Arc;
pub use terminal::{TerminalCell, TerminalViewportSnapshot};
mod viewport_snapshot { pub use terminal::{CellAttrs, CellRange, CursorShape, TerminalViewportSnapshot}; }
#[path = "/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/crates/terminal/src/renderer_egui.rs"]
mod renderer;
use terminal::{AlacrittyBackend, TerminalBackend, CellRange};
fn render(ctx: &egui::Context, cache: &mut renderer::TerminalRenderCache, snap: &TerminalViewportSnapshot, generation: u64) -> String {
    let out=ctx.run_ui(egui::RawInput::default(), |ui| {
        renderer::draw(ui,snap,renderer::CellMetrics{font_size:12.0,line_height:1.0},cache,None,false,None,generation);
    });
    let result=out.shapes.iter().filter_map(|s|match &s.shape {egui::Shape::Text(t)=>Some(t.galley.job.text.clone()),_=>None}).collect::<Vec<_>>().join("|");
    out.drop_without_applying_deltas();
    result
}
fn main(){
 let ctx=egui::Context::default();let mut cache=renderer::TerminalRenderCache::default();
 let mut backend=AlacrittyBackend::new(10,3,100);
 backend.feed(b"\x1b[1;1HOLD0\x1b[2;1HOLD1").unwrap();
 let mut baseline=backend.viewport_snapshot().unwrap();baseline.dirty_ranges=vec![CellRange{start:0,end:30}];
 render(&ctx,&mut cache,&baseline,1);render(&ctx,&mut cache,&baseline,1);
 // Workspace selection freeze: successive deliveries replace pending_snapshot without drawing.
 backend.feed(b"\x1b[1;1HNEW0").unwrap();
 let mut pending=Arc::new(backend.viewport_snapshot().unwrap());Arc::make_mut(&mut pending).dirty_ranges=vec![CellRange{start:0,end:10}];
 assert_eq!(pending.visible_cells[0].c,'N');
 backend.feed(b"\x1b[2;1HNEW1").unwrap();
 pending=Arc::new(backend.viewport_snapshot().unwrap());Arc::make_mut(&mut pending).dirty_ranges=vec![CellRange{start:10,end:20}];
 // Workspace installs the final pending snapshot and increments generation without clearing its cache.
 let visible=render(&ctx,&mut cache,&pending,2);
 println!("snapshot row0={}; row1={}",pending.visible_cells[0].c,pending.visible_cells[10].c);
 println!("painted text={visible:?}; rebuilt_rows={}",cache.rebuilt_rows_last_frame());
 assert!(visible.contains("OLD0"),"Expected source-proven stale first row");
 assert!(visible.contains("NEW1"));
 assert!(!visible.contains("NEW0"));
 println!("REPRODUCED: latest snapshot says NEW0, renderer still paints OLD0");
 // Clearing cache is the minimal conservative fix and must produce the correct result.
 cache.clear();let repaired=render(&ctx,&mut cache,&pending,3);
 assert!(repaired.contains("NEW0")&&repaired.contains("NEW1"));
 println!("CONTROL: cache.clear() paints both latest rows");
}
