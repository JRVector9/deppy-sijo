//! Tests for the Grid.

use super::*;

use crate::term::cell::Cell;

impl GridCell for usize {
    fn is_empty(&self) -> bool {
        *self == 0
    }

    fn reset(&mut self, template: &Self) {
        *self = *template;
    }

    fn flags(&self) -> &Flags {
        unimplemented!();
    }

    fn flags_mut(&mut self) -> &mut Flags {
        unimplemented!();
    }
}

// Scroll up moves lines upward.
#[test]
fn scroll_up() {
    let mut grid = Grid::<usize>::new(10, 1, 0);
    for i in 0..10 {
        grid[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_up::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid[Line(0)][Column(0)], 2);
    assert_eq!(grid[Line(0)].occ, 1);
    assert_eq!(grid[Line(1)][Column(0)], 3);
    assert_eq!(grid[Line(1)].occ, 1);
    assert_eq!(grid[Line(2)][Column(0)], 4);
    assert_eq!(grid[Line(2)].occ, 1);
    assert_eq!(grid[Line(3)][Column(0)], 5);
    assert_eq!(grid[Line(3)].occ, 1);
    assert_eq!(grid[Line(4)][Column(0)], 6);
    assert_eq!(grid[Line(4)].occ, 1);
    assert_eq!(grid[Line(5)][Column(0)], 7);
    assert_eq!(grid[Line(5)].occ, 1);
    assert_eq!(grid[Line(6)][Column(0)], 8);
    assert_eq!(grid[Line(6)].occ, 1);
    assert_eq!(grid[Line(7)][Column(0)], 9);
    assert_eq!(grid[Line(7)].occ, 1);
    assert_eq!(grid[Line(8)][Column(0)], 0); // was 0.
    assert_eq!(grid[Line(8)].occ, 0);
    assert_eq!(grid[Line(9)][Column(0)], 0); // was 1.
    assert_eq!(grid[Line(9)].occ, 0);
}

// Scroll down moves lines downward.
#[test]
fn scroll_down() {
    let mut grid = Grid::<usize>::new(10, 1, 0);
    for i in 0..10 {
        grid[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_down::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid[Line(0)][Column(0)], 0); // was 8.
    assert_eq!(grid[Line(0)].occ, 0);
    assert_eq!(grid[Line(1)][Column(0)], 0); // was 9.
    assert_eq!(grid[Line(1)].occ, 0);
    assert_eq!(grid[Line(2)][Column(0)], 0);
    assert_eq!(grid[Line(2)].occ, 1);
    assert_eq!(grid[Line(3)][Column(0)], 1);
    assert_eq!(grid[Line(3)].occ, 1);
    assert_eq!(grid[Line(4)][Column(0)], 2);
    assert_eq!(grid[Line(4)].occ, 1);
    assert_eq!(grid[Line(5)][Column(0)], 3);
    assert_eq!(grid[Line(5)].occ, 1);
    assert_eq!(grid[Line(6)][Column(0)], 4);
    assert_eq!(grid[Line(6)].occ, 1);
    assert_eq!(grid[Line(7)][Column(0)], 5);
    assert_eq!(grid[Line(7)].occ, 1);
    assert_eq!(grid[Line(8)][Column(0)], 6);
    assert_eq!(grid[Line(8)].occ, 1);
    assert_eq!(grid[Line(9)][Column(0)], 7);
    assert_eq!(grid[Line(9)].occ, 1);
}

#[test]
fn scroll_down_with_history() {
    let mut grid = Grid::<usize>::new(10, 1, 1);
    grid.increase_scroll_limit(1);
    for i in 0..10 {
        grid[Line(i as i32)][Column(0)] = i;
    }

    grid.scroll_down::<usize>(&(Line(0)..Line(10)), 2);

    assert_eq!(grid[Line(0)][Column(0)], 0); // was 8.
    assert_eq!(grid[Line(0)].occ, 0);
    assert_eq!(grid[Line(1)][Column(0)], 0); // was 9.
    assert_eq!(grid[Line(1)].occ, 0);
    assert_eq!(grid[Line(2)][Column(0)], 0);
    assert_eq!(grid[Line(2)].occ, 1);
    assert_eq!(grid[Line(3)][Column(0)], 1);
    assert_eq!(grid[Line(3)].occ, 1);
    assert_eq!(grid[Line(4)][Column(0)], 2);
    assert_eq!(grid[Line(4)].occ, 1);
    assert_eq!(grid[Line(5)][Column(0)], 3);
    assert_eq!(grid[Line(5)].occ, 1);
    assert_eq!(grid[Line(6)][Column(0)], 4);
    assert_eq!(grid[Line(6)].occ, 1);
    assert_eq!(grid[Line(7)][Column(0)], 5);
    assert_eq!(grid[Line(7)].occ, 1);
    assert_eq!(grid[Line(8)][Column(0)], 6);
    assert_eq!(grid[Line(8)].occ, 1);
    assert_eq!(grid[Line(9)][Column(0)], 7);
    assert_eq!(grid[Line(9)].occ, 1);
}

// Test that GridIterator works.
#[test]
fn test_iter() {
    let assert_indexed = |value: usize, indexed: Option<Indexed<&usize>>| {
        assert_eq!(Some(&value), indexed.map(|indexed| indexed.cell));
    };

    let mut grid = Grid::<usize>::new(5, 5, 0);
    for i in 0..5 {
        for j in 0..5 {
            grid[Line(i)][Column(j)] = i as usize * 5 + j;
        }
    }

    let mut iter = grid.iter_from(Point::new(Line(0), Column(0)));

    assert_eq!(None, iter.prev());
    assert_indexed(1, iter.next());
    assert_eq!(Column(1), iter.point().column);
    assert_eq!(0, iter.point().line);

    assert_indexed(2, iter.next());
    assert_indexed(3, iter.next());
    assert_indexed(4, iter.next());

    // Test line-wrapping.
    assert_indexed(5, iter.next());
    assert_eq!(Column(0), iter.point().column);
    assert_eq!(1, iter.point().line);

    assert_indexed(4, iter.prev());
    assert_eq!(Column(4), iter.point().column);
    assert_eq!(0, iter.point().line);

    // Make sure iter.cell() returns the current iterator position.
    assert_eq!(&4, iter.cell());

    // Test that iter ends at end of grid.
    let mut final_iter = grid.iter_from(Point { line: Line(4), column: Column(4) });
    assert_eq!(None, final_iter.next());
    assert_indexed(23, final_iter.prev());
}

#[test]
fn shrink_reflow() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = cell('2');
    grid[Line(0)][Column(2)] = cell('3');
    grid[Line(0)][Column(3)] = cell('4');
    grid[Line(0)][Column(4)] = cell('5');

    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(grid[Line(-2)].len(), 2);
    assert_eq!(grid[Line(-2)][Column(0)], cell('1'));
    assert_eq!(grid[Line(-2)][Column(1)], wrap_cell('2'));

    assert_eq!(grid[Line(-1)].len(), 2);
    assert_eq!(grid[Line(-1)][Column(0)], cell('3'));
    assert_eq!(grid[Line(-1)][Column(1)], wrap_cell('4'));

    assert_eq!(grid[Line(0)].len(), 2);
    assert_eq!(grid[Line(0)][Column(0)], cell('5'));
    assert_eq!(grid[Line(0)][Column(1)], Cell::default());
}

#[test]
fn shrink_reflow_twice() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = cell('2');
    grid[Line(0)][Column(2)] = cell('3');
    grid[Line(0)][Column(3)] = cell('4');
    grid[Line(0)][Column(4)] = cell('5');

    grid.resize(true, 1, 4);
    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(grid[Line(-2)].len(), 2);
    assert_eq!(grid[Line(-2)][Column(0)], cell('1'));
    assert_eq!(grid[Line(-2)][Column(1)], wrap_cell('2'));

    assert_eq!(grid[Line(-1)].len(), 2);
    assert_eq!(grid[Line(-1)][Column(0)], cell('3'));
    assert_eq!(grid[Line(-1)][Column(1)], wrap_cell('4'));

    assert_eq!(grid[Line(0)].len(), 2);
    assert_eq!(grid[Line(0)][Column(0)], cell('5'));
    assert_eq!(grid[Line(0)][Column(1)], Cell::default());
}

#[test]
fn shrink_reflow_empty_cell_inside_line() {
    let mut grid = Grid::<Cell>::new(1, 5, 3);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = Cell::default();
    grid[Line(0)][Column(2)] = cell('3');
    grid[Line(0)][Column(3)] = cell('4');
    grid[Line(0)][Column(4)] = Cell::default();

    grid.resize(true, 1, 2);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(grid[Line(-1)].len(), 2);
    assert_eq!(grid[Line(-1)][Column(0)], cell('1'));
    assert_eq!(grid[Line(-1)][Column(1)], wrap_cell(' '));

    assert_eq!(grid[Line(0)].len(), 2);
    assert_eq!(grid[Line(0)][Column(0)], cell('3'));
    assert_eq!(grid[Line(0)][Column(1)], cell('4'));

    grid.resize(true, 1, 1);

    assert_eq!(grid.total_lines(), 4);

    assert_eq!(grid[Line(-3)].len(), 1);
    assert_eq!(grid[Line(-3)][Column(0)], wrap_cell('1'));

    assert_eq!(grid[Line(-2)].len(), 1);
    assert_eq!(grid[Line(-2)][Column(0)], wrap_cell(' '));

    assert_eq!(grid[Line(-1)].len(), 1);
    assert_eq!(grid[Line(-1)][Column(0)], wrap_cell('3'));

    assert_eq!(grid[Line(0)].len(), 1);
    assert_eq!(grid[Line(0)][Column(0)], cell('4'));
}

#[test]
fn grow_reflow() {
    let mut grid = Grid::<Cell>::new(2, 2, 0);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = wrap_cell('2');
    grid[Line(1)][Column(0)] = cell('3');
    grid[Line(1)][Column(1)] = Cell::default();

    grid.resize(true, 2, 3);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(grid[Line(0)].len(), 3);
    assert_eq!(grid[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid[Line(0)][Column(1)], cell('2'));
    assert_eq!(grid[Line(0)][Column(2)], cell('3'));

    // Make sure rest of grid is empty.
    assert_eq!(grid[Line(1)].len(), 3);
    assert_eq!(grid[Line(1)][Column(0)], Cell::default());
    assert_eq!(grid[Line(1)][Column(1)], Cell::default());
    assert_eq!(grid[Line(1)][Column(2)], Cell::default());
}

#[test]
fn grow_reflow_multiline() {
    let mut grid = Grid::<Cell>::new(3, 2, 0);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = wrap_cell('2');
    grid[Line(1)][Column(0)] = cell('3');
    grid[Line(1)][Column(1)] = wrap_cell('4');
    grid[Line(2)][Column(0)] = cell('5');
    grid[Line(2)][Column(1)] = cell('6');

    grid.resize(true, 3, 6);

    assert_eq!(grid.total_lines(), 3);

    assert_eq!(grid[Line(0)].len(), 6);
    assert_eq!(grid[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid[Line(0)][Column(1)], cell('2'));
    assert_eq!(grid[Line(0)][Column(2)], cell('3'));
    assert_eq!(grid[Line(0)][Column(3)], cell('4'));
    assert_eq!(grid[Line(0)][Column(4)], cell('5'));
    assert_eq!(grid[Line(0)][Column(5)], cell('6'));

    // Make sure rest of grid is empty.
    for r in (1..3).map(Line::from) {
        assert_eq!(grid[r].len(), 6);
        for c in 0..6 {
            assert_eq!(grid[r][Column(c)], Cell::default());
        }
    }
}

#[test]
fn grow_reflow_disabled() {
    let mut grid = Grid::<Cell>::new(2, 2, 0);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = wrap_cell('2');
    grid[Line(1)][Column(0)] = cell('3');
    grid[Line(1)][Column(1)] = Cell::default();

    grid.resize(false, 2, 3);

    assert_eq!(grid.total_lines(), 2);

    assert_eq!(grid[Line(0)].len(), 3);
    assert_eq!(grid[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid[Line(0)][Column(1)], wrap_cell('2'));
    assert_eq!(grid[Line(0)][Column(2)], Cell::default());

    assert_eq!(grid[Line(1)].len(), 3);
    assert_eq!(grid[Line(1)][Column(0)], cell('3'));
    assert_eq!(grid[Line(1)][Column(1)], Cell::default());
    assert_eq!(grid[Line(1)][Column(2)], Cell::default());
}

#[test]
fn shrink_reflow_disabled() {
    let mut grid = Grid::<Cell>::new(1, 5, 2);
    grid[Line(0)][Column(0)] = cell('1');
    grid[Line(0)][Column(1)] = cell('2');
    grid[Line(0)][Column(2)] = cell('3');
    grid[Line(0)][Column(3)] = cell('4');
    grid[Line(0)][Column(4)] = cell('5');

    grid.resize(false, 1, 2);

    assert_eq!(grid.total_lines(), 1);

    assert_eq!(grid[Line(0)].len(), 2);
    assert_eq!(grid[Line(0)][Column(0)], cell('1'));
    assert_eq!(grid[Line(0)][Column(1)], cell('2'));
}

#[test]
fn accurate_size_hint() {
    let grid = Grid::<Cell>::new(5, 5, 2);

    size_hint_matches_count(grid.iter_from(Point::new(Line(0), Column(0))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(2), Column(3))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(4), Column(4))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(4), Column(2))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(10), Column(10))));
    size_hint_matches_count(grid.iter_from(Point::new(Line(2), Column(10))));

    let mut iterator = grid.iter_from(Point::new(Line(3), Column(1)));
    iterator.next();
    iterator.next();
    size_hint_matches_count(iterator);

    size_hint_matches_count(grid.display_iter());
}

fn size_hint_matches_count<T>(iter: impl Iterator<Item = T>) {
    let iterator = iter.into_iter();
    let (lower, upper) = iterator.size_hint();
    let count = iterator.count();
    assert_eq!(lower, count);
    assert_eq!(upper, Some(count));
}

// https://github.com/rust-lang/rust-clippy/pull/6375
#[allow(clippy::all)]
fn cell(c: char) -> Cell {
    let mut cell = Cell::default();
    cell.c = c;
    cell
}

fn wrap_cell(c: char) -> Cell {
    let mut cell = cell(c);
    cell.flags.insert(Flags::WRAPLINE);
    cell
}

// ── deppy-sijo 옵션 D: 스크롤백 라인 압축 ───────────────────────────────────

/// 가시 1줄·`cols`폭 그리드에 `rows`개의 서로 다른 줄을 스크롤아웃시켜 history를
/// 만든다. Line(-1)=마지막, Line(-rows)=처음 쓴 줄.
fn grid_with_history(rows: usize, cols: usize) -> Grid<Cell> {
    let mut grid = Grid::<Cell>::new(1, cols, rows + 5);
    for r in 0..rows {
        let ch = char::from(b'A' + (r % 26) as u8);
        for c in 0..cols {
            grid[Line(0)][Column(c)] = cell(ch);
        }
        grid.scroll_up::<crate::vte::ansi::Color>(&(Line(0)..Line(1)), 1);
    }
    grid
}

fn assert_line_eq(grid: &Grid<Cell>, line: Line, expected: &Row<Cell>, cols: usize) {
    let mut scratch = Row::<Cell>::new(cols);
    let got = grid.read_line(line, &mut scratch);
    for c in 0..cols {
        assert_eq!(got[Column(c)], expected[Column(c)], "{line:?} col {c} 불일치");
    }
    assert_eq!(got.occ, expected.occ, "{line:?} occ 불일치");
}

#[test]
fn deppy_compress_history_roundtrip_and_frees_heap() {
    let (rows, cols) = (6, 4);
    let mut grid = grid_with_history(rows, cols);
    assert_eq!(grid.history_size(), rows);

    // 압축 전 스냅샷.
    let snap: Vec<Row<Cell>> = (1..=rows).map(|d| grid[Line(-(d as i32))].clone()).collect();

    // hot_lines=0 → history 전체 압축.
    let freed = grid.compress_history(0, rows);
    assert!(freed > 0, "회수 바이트가 0");
    assert!(grid.compressed_heap_bytes() > 0, "압축 곁가지가 비어 있음");

    // read_line 왕복 = 원본과 완전히 동일.
    for (i, expected) in snap.iter().enumerate() {
        assert_line_eq(&grid, Line(-((i + 1) as i32)), expected, cols);
    }

    // 재호출은 idempotent(이미 압축된 것은 0 회수).
    assert_eq!(grid.compress_history(0, rows), 0, "재압축이 추가 회수");
}

#[test]
fn deppy_compress_history_respects_hot_lines() {
    let (rows, cols) = (6, 4);
    let mut grid = grid_with_history(rows, cols);
    let snap: Vec<Row<Cell>> = (1..=rows).map(|d| grid[Line(-(d as i32))].clone()).collect();

    // hot_lines >= history → 아무것도 압축 안 함.
    assert_eq!(grid.compress_history(rows, rows), 0);
    assert_eq!(grid.compressed_heap_bytes(), 0);

    // hot_lines=2 → 오래된 rows-2줄만 압축. 그래도 모든 줄 read_line은 원본과 동일.
    let freed = grid.compress_history(2, rows);
    assert!(freed > 0);
    for (i, expected) in snap.iter().enumerate() {
        assert_line_eq(&grid, Line(-((i + 1) as i32)), expected, cols);
    }
}

/// 현재 맨 아래 가시줄에 `n` 라벨을 쓰고 스크롤아웃한다(재활용 테스트용).
fn push_labeled_line(grid: &mut Grid<Cell>, n: usize, cols: usize) {
    let ch = char::from(b'A' + (n % 26) as u8);
    for c in 0..cols {
        grid[Line(0)][Column(c)] = cell(ch);
    }
    grid.scroll_up::<crate::vte::ansi::Color>(&(Line(0)..Line(1)), 1);
}

/// 스크롤백이 가득 찬 상태에서 압축된 가장 오래된 슬롯이 재활용될 때 — reset_row가
/// placeholder를 안전하게 inflate하는지(패닉 없이) + 남은 줄 내용 정확성.
#[test]
fn deppy_compress_then_recycle_survives() {
    let (cols, cap) = (4, 6);
    let mut grid = Grid::<Cell>::new(1, cols, cap);
    let mut wrote = 0usize;
    for _ in 0..cap {
        push_labeled_line(&mut grid, wrote, cols);
        wrote += 1;
    }
    assert_eq!(grid.history_size(), cap);

    grid.compress_history(0, cap);
    assert!(grid.compressed_heap_bytes() > 0);

    // 가득 찬 상태에서 4줄 더 → 가장 오래된 압축 슬롯 4개 재활용(패닉 없어야).
    for _ in 0..4 {
        push_labeled_line(&mut grid, wrote, cols);
        wrote += 1;
    }
    assert_eq!(grid.history_size(), cap);

    // Line(-1)=마지막 쓴 것(새 비압축), Line(-cap)=재활용에서 살아남은 압축 줄.
    let mut scratch = Row::<Cell>::new(cols);
    let newest = char::from(b'A' + ((wrote - 1) % 26) as u8);
    let row = grid.read_line(Line(-1), &mut scratch);
    for c in 0..cols {
        assert_eq!(row[Column(c)].c, newest, "Line(-1) col {c}");
    }
    // cap=6, 총 10줄 → 남은 6줄은 wrote 4..=9 (E..J). Line(-cap)=가장 오래된 남은 줄 E.
    let oldest_remaining = char::from(b'A' + ((wrote - cap) % 26) as u8);
    let row = grid.read_line(Line(-(cap as i32)), &mut scratch);
    for c in 0..cols {
        assert_eq!(row[Column(c)].c, oldest_remaining, "Line(-cap) col {c}");
    }
}

/// inflate_all 후에는 원시 Index로 직접 읽어도(비압축) 원본과 동일해야 한다.
#[test]
fn deppy_inflate_all_restores_stock_reads() {
    let (rows, cols) = (6, 4);
    let mut grid = grid_with_history(rows, cols);
    let snap: Vec<Row<Cell>> = (1..=rows).map(|d| grid[Line(-(d as i32))].clone()).collect();

    grid.compress_history(0, rows);
    assert!(grid.compressed_heap_bytes() > 0);

    grid.inflate_all();
    assert_eq!(grid.compressed_heap_bytes(), 0);

    for (i, expected) in snap.iter().enumerate() {
        let line = Line(-((i + 1) as i32));
        for c in 0..cols {
            assert_eq!(grid[line][Column(c)], expected[Column(c)], "{line:?} col {c}");
        }
    }
}

/// inflate_all 후에는 resize(reflow)가 방어 assert 없이 통과한다.
#[test]
fn deppy_resize_after_inflate_ok() {
    let (rows, cols) = (6, 4);
    let mut grid = grid_with_history(rows, cols);
    grid.compress_history(0, rows);
    grid.inflate_all();
    grid.resize::<crate::vte::ansi::Color>(true, 1, 2);
    assert!(grid.history_size() >= 1);
}

/// 압축 상태에서 inflate 없이 resize하면 방어 debug_assert가 걸린다.
#[test]
#[should_panic(expected = "compressed")]
fn deppy_resize_on_compressed_trips_assert() {
    let (rows, cols) = (6, 4);
    let mut grid = grid_with_history(rows, cols);
    grid.compress_history(0, rows);
    grid.resize::<crate::vte::ansi::Color>(true, 1, 2);
}

/// 압축된 그리드도 truncate(가장 오래된 history 제거) 후 남은 줄이 온전한지 —
/// 곁가지 truncate 동기화 검증.
#[test]
fn deppy_compress_survives_history_shrink() {
    let (rows, cols) = (8, 4);
    let mut grid = grid_with_history(rows, cols);
    let snap: Vec<Row<Cell>> = (1..=rows).map(|d| grid[Line(-(d as i32))].clone()).collect();

    grid.compress_history(0, rows);

    // 스크롤백을 3줄로 줄인다(가장 오래된 rows-3줄 제거).
    grid.update_history(3);
    assert_eq!(grid.history_size(), 3);

    // 남은 최근 3줄(Line(-1..=-3))은 여전히 원본과 동일하게 읽혀야 한다.
    for d in 1..=3 {
        assert_line_eq(&grid, Line(-(d as i32)), &snap[d - 1], cols);
    }
}
