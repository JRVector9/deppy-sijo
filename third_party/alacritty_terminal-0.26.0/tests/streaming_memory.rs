//! 메모리 측정은 전용 테스트 프로세스에서 실행한다. 앱/GUI는 빌드하지 않는다.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as Counter;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

thread_local! {
    static ENABLED: Counter<bool> = const { Counter::new(false) };
    static LIVE: Counter<isize> = const { Counter::new(0) };
    static PEAK: Counter<isize> = const { Counter::new(0) };
}

struct MeasuredAllocator;

fn account(delta: isize) {
    if ENABLED.try_with(Counter::get).unwrap_or(false) {
        let _ = LIVE.try_with(|live| {
            let next = live.get() + delta;
            live.set(next);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(next)));
        });
    }
}

unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        account(-(layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, size) };
        if !out.is_null() {
            account(size as isize - layout.size() as isize);
        }
        out
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

fn long_wrapped_grid(history: usize, columns: usize) -> Grid<Cell> {
    let mut grid = Grid::new(24, columns, history);
    let mut row = Row::<Cell>::new(columns);
    for col in 0..columns {
        row[Column(col)].c = 'x';
    }
    row[Column(columns - 1)].flags.insert(Flags::WRAPLINE);
    for _ in 0..history + 24 {
        grid[Line(23)] = row.clone();
        grid.scroll_up(&(Line(0)..Line(24)), 1);
        grid.compress_history(0);
    }
    grid
}

fn measure(history: usize, old: usize, new: usize, stock: bool) -> (usize, u128) {
    let mut grid = long_wrapped_grid(history, old);
    LIVE.set(0);
    PEAK.set(0);
    ENABLED.set(true);
    let started = Instant::now();
    if stock {
        grid.inflate_all();
        grid.resize(true, 24, new);
    } else {
        let metrics = grid.resize_streaming(true, 24, new);
        assert!(
            metrics.peak_scratch_cells <= 8 * (old + new),
            "논리 줄 길이와 독립적인 scratch 상한 위반: {metrics:?}"
        );
        eprintln!("scratch_cells={}", metrics.peak_scratch_cells);
    }
    let elapsed = started.elapsed().as_millis();
    ENABLED.set(false);
    let peak = PEAK.get().max(0) as usize;
    assert!(grid.history_size() <= history);
    if !stock {
        assert_eq!(grid.history_size(), grid.compressed_row_count());
    }
    eprintln!(
        "history={history} cols={old}->{new} stock={stock} peak_extra_bytes={peak} elapsed_ms={elapsed}"
    );
    (peak, elapsed)
}

#[test]
fn streaming_resize_narrow_output_memory_is_bounded_by_retention() {
    let history = 10_000;
    let (peak, _) = measure(history, 200, 2, false);
    assert!(
        peak < 24 * 1024 * 1024,
        "보존 한도 밖 출력까지 쌓음: {peak} bytes"
    );
}

#[test]
#[ignore = "100k 메모리/시간 측정: --ignored --nocapture로 명시 실행"]
fn streaming_resize_memory_benchmark() {
    for history in [10_000, 100_000] {
        for (old, new) in [(80, 120), (200, 100), (500, 80), (200, 2)] {
            let (peak, _) = measure(history, old, new, false);
            let raw_history = history * old * std::mem::size_of::<Cell>();
            assert!(
                peak < raw_history / 2,
                "원시 전체 확장에 비례하는 피크: {peak}"
            );
        }
    }
    measure(10_000, 200, 100, true);
}
