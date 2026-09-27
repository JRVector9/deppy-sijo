use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use terminal::{AlacrittyBackend, TerminalBackend};

struct CountAlloc;
static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) { System.dealloc(ptr, layout); }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        System.realloc(ptr, old, size)
    }
}
#[global_allocator]
static ALLOC: CountAlloc = CountAlloc;
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, usize, f64) {
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    let start = Instant::now();
    let result = f();
    let micros = start.elapsed().as_secs_f64() * 1e6;
    ENABLED.store(false, Ordering::Relaxed);
    (result, ALLOCS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed), micros)
}
fn main() {
    #[allow(dead_code)]
    struct PackedFlagsCell { c: char, fg: [u8; 3], bg: [u8; 3], flags: u8 }
    println!("TerminalCell size={}B; allocation counts include realloc; bytes are requested allocation traffic, NOT RSS", std::mem::size_of::<terminal::TerminalCell>());
    println!("normal-aligned one-flags-byte model size={}B", std::mem::size_of::<PackedFlagsCell>());
    for text in ["한", "\u{1100}\u{1161}\u{11f9}", "a\u{0301}\u{0308}"] {
        let mut backend = AlacrittyBackend::new(20, 2, 0);
        backend.feed(text.as_bytes()).unwrap();
        let snapshot = backend.viewport_snapshot().unwrap();
        let first = snapshot.visible_cells[0].c;
        let displayed = terminal::renderer_egui::selection_text(&snapshot, 0, 19);
        println!("cluster input={:?} first cell={:?} (U+{:04X}) displayed/copy={:?}", text, first, first as u32, displayed);
    }
    for history in [1000, 5000, 20000] {
        let mut backend = AlacrittyBackend::new(300, 80, history);
        let output = "sample line with fixed attributes\r\n".repeat(history + 100);
        backend.feed(output.as_bytes()).unwrap();
        let (footprint, _, _, _) = measure(|| backend.cache_footprint());
        let (_, n, bytes, us) = measure(|| {
            for _ in 0..1000 { std::hint::black_box(backend.cache_footprint()); }
        });
        println!("footprint history={} estimate={}B: {:.2}us/call allocs={} bytes={}", footprint.history_lines, footprint.estimated_bytes, us / 1000., n, bytes);
        for offset in [0, 400] {
            backend.scroll_to_bottom();
            backend.scroll(offset);
            let (_, n, bytes, us) = measure(|| {
                for _ in 0..100 { std::hint::black_box(backend.viewport_snapshot().unwrap()); }
            });
            println!("snapshot 300x80 history={} offset={}: {:.2}us/snapshot allocations={:.1} requested_bytes={:.0}", history, offset, us/100., n as f64/100., bytes as f64/100.);
        }
    }
    for (label, text) in [("ASCII", "a".repeat(300)), ("Hangul", "한".repeat(150))] {
        let mut backend = AlacrittyBackend::new(300, 80, 0);
        let output = (text + "\r\n").repeat(81);
        backend.feed(output.as_bytes()).unwrap();
        let snapshot = backend.viewport_snapshot().unwrap();
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("D2".into(), std::sync::Arc::new(egui::FontData::from_static(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../../crates/app/assets/fonts/D2Coding-Regular.ttf")))));
        fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap().insert(0, "D2".into());
        ctx.set_fonts(fonts);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(5000., 3000.))), ..Default::default() };
        let mut cache = terminal::renderer_egui::TerminalRenderCache::default();
        let mut last = terminal::renderer_egui::RenderCounters::default();
        for _ in 0..5 {
            let full = ctx.run_ui(raw.clone(), |ui| {
                last = terminal::renderer_egui::draw(ui, &snapshot, terminal::renderer_egui::CellMetrics {font_size:13., line_height:1.2}, &mut cache, None, false, None, 1).counters;
            });
            std::hint::black_box(ctx.tessellate(full.shapes, full.pixels_per_point));
        }
        let (_, n, bytes, us) = measure(|| {
            for _ in 0..100 {
                let full = ctx.run_ui(raw.clone(), |ui| {
                    last = terminal::renderer_egui::draw(ui, &snapshot, terminal::renderer_egui::CellMetrics {font_size:13., line_height:1.2}, &mut cache, None, false, None, 1).counters;
                });
                std::hint::black_box(ctx.tessellate(full.shapes, full.pixels_per_point));
            }
        });
        println!("{} 300x80 cached render+tess: {:.2}ms shapes={} rebuilt={} painted={} allocations/frame={:.0} requested_bytes/frame={:.0}", label, us/1e5, last.shapes, last.rows_rebuilt, last.rows_painted, n as f64/100., bytes as f64/100.);
    }
}
