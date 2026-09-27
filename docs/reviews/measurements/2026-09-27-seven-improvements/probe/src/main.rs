use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use terminal::{AlacrittyBackend, TerminalBackend};

struct CountAlloc;
static ENABLED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) { System.dealloc(ptr, layout); }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        System.realloc(ptr, old, size)
    }
}
#[global_allocator]
static ALLOC: CountAlloc = CountAlloc;
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, usize, f64) {
    CALLS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    let start = Instant::now();
    let value = f();
    let us = start.elapsed().as_secs_f64() * 1e6;
    ENABLED.store(false, Ordering::Relaxed);
    (value, CALLS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed), us)
}
fn main() {
    println!("cell_bytes={}", std::mem::size_of::<terminal::TerminalCell>());
    println!("allocator=System; requested allocation traffic includes realloc, excludes PTY feed; not RSS/GPU/native IME");
    for history in [1000, 5000, 20000] {
        let mut backend = AlacrittyBackend::new(300, 80, history);
        backend.feed("sample line with fixed attributes\r\n".repeat(history + 100).as_bytes()).unwrap();
        let (_, calls, bytes, us) = measure(|| {
            for _ in 0..10000 {
                std::hint::black_box(std::hint::black_box(&backend).cache_footprint());
            }
        });
        println!("footprint history={history} us={:.6} calls={calls} bytes={bytes}", us / 10000.);
    }
    for scenario in ["cold", "metadata", "dirty1", "dirty80", "compressed_steady", "compressed_scroll"] {
        let mut backend = AlacrittyBackend::new(300, 80, 2000);
        backend.feed(("a".repeat(299) + "\r\n").repeat(2100).as_bytes()).unwrap();
        if scenario.starts_with("compressed") { backend.scroll(400); }
        if scenario != "cold" { std::hint::black_box(backend.viewport_snapshot().unwrap()); }
        let iterations = if scenario == "cold" { 1 } else { 200 };
        let mut sum_calls = 0;
        let mut sum_bytes = 0;
        let mut sum_us = 0.;
        let before = backend.cache_footprint().estimated_bytes;
        for i in 0..iterations {
            match scenario {
                "metadata" => { backend.feed(if i % 2 == 0 { b"\x1b[40;2H" } else { b"\x1b[41;2H" }).unwrap(); }
                "dirty1" => { backend.feed(if i % 2 == 0 { b"\x1b[40;1Hb" } else { b"\x1b[40;1Hc" }).unwrap(); }
                "dirty80" => {
                    let text = (if i % 2 == 0 { "b" } else { "c" }).repeat(299);
                    let mut data = String::from("\x1b[H");
                    for row in 0..80 { data.push_str(&text); if row < 79 { data.push_str("\r\n"); } }
                    backend.feed(data.as_bytes()).unwrap();
                }
                "compressed_scroll" => backend.scroll(if i % 2 == 0 { 1 } else { -1 }),
                _ => {}
            }
            let (snapshot, calls, bytes, us) = measure(|| std::hint::black_box(&backend).viewport_snapshot().unwrap());
            assert_eq!(snapshot.visible_cells.len(), 24000);
            if scenario == "dirty1" { assert_eq!(snapshot.visible_cells[39 * 300].c, if i % 2 == 0 { 'b' } else { 'c' }); }
            if scenario == "dirty80" { assert_eq!(snapshot.visible_cells[79 * 300].c, if i % 2 == 0 { 'b' } else { 'c' }); }
            std::hint::black_box(snapshot);
            sum_calls += calls;
            sum_bytes += bytes;
            sum_us += us;
        }
        println!("snapshot scenario={scenario} iterations={iterations} us={:.4} calls={:.4} bytes={:.1} backend_resident_before={before} backend_resident_after={}", sum_us / iterations as f64, sum_calls as f64 / iterations as f64, sum_bytes as f64 / iterations as f64, backend.cache_footprint().estimated_bytes);
    }
    for text in ["한", "\u{1100}\u{1161}\u{11f9}", "a\u{0301}\u{0308}"] {
        let mut backend = AlacrittyBackend::new(20, 2, 0);
        backend.feed(text.as_bytes()).unwrap();
        let snapshot = backend.viewport_snapshot().unwrap();
        println!("cluster input={text:?} copy={:?}", terminal::renderer_egui::selection_text(&snapshot, 0, 19));
    }
}
