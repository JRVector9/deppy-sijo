use terminal::{AlacrittyBackend, TerminalBackend};
fn text(backend: &AlacrittyBackend) -> String { backend.viewport_snapshot().unwrap().visible_cells.iter().map(|c|c.c).collect() }
fn main(){
 let mut original=AlacrittyBackend::new(20,3,100);
 original.feed(b"\x1b[8mprivate-value\x1b[0m\r\n\x1b[1;3;4;9mSTYLED\x1b[0m").unwrap();
 assert!(!text(&original).contains("private-value"));
 let dump=original.serialize_scrollback().unwrap();
 let mut restored=AlacrittyBackend::new(20,3,100);restored.feed(&dump).unwrap();
 println!("before={:?}",text(&original));println!("after={:?}",text(&restored));
 println!("styles before={:?} after={:?}",original.viewport_snapshot().unwrap().visible_cells[20].attrs(),restored.viewport_snapshot().unwrap().visible_cells[20].attrs());
 assert!(text(&restored).contains("private-value"));
 assert_ne!(original.viewport_snapshot().unwrap().visible_cells[20].attrs(),restored.viewport_snapshot().unwrap().visible_cells[20].attrs());
 println!("REPRODUCED: hidden text becomes visible after archive round-trip; SGR attributes also lost");
}
