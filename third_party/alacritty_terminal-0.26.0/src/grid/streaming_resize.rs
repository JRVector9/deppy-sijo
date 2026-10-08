//! 압축 이력을 한 행씩 소비하는 resize. 커서 계산 순서는 기존 resize와 동일하다.
//! 입력 한 행, 출력 마지막 행, shrink carry만 원시 셀로 유지한다. 완성 출력은
//! 즉시 압축하므로 하나의 logical line이 이력 전체에 걸쳐도 원시 배열로 합치지 않는다.

use std::cmp::{Ordering, max, min};
use std::collections::VecDeque;
use std::mem;

use super::compressed::CompressedRow;
use super::{Dimensions, Grid, GridCell, Row};
use crate::index::{Boundary, Column, Line};
use crate::term::cell::{Cell, Flags};

/// 리플로가 동시에 소유한 원시 작업 버퍼의 관찰 최대치. 화면/기존 history와 codec
/// 힙은 제외한다. allocator의 재할당 순간 피크는 별도 프로세스 계측으로 검증한다.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReflowMetrics {
    pub peak_scratch_cells: usize,
}

impl ReflowMetrics {
    fn observe(&mut self, cells: usize) {
        self.peak_scratch_cells = self.peak_scratch_cells.max(cells);
    }
}

/// 수정 가능한 마지막 행만 원시 형태로 남기는 출력 버퍼.
struct PackedRows {
    packed: VecDeque<CompressedRow>,
    limit: usize,
    tail: Option<Row<Cell>>,
}

impl PackedRows {
    fn with_capacity(capacity: usize, limit: usize) -> Self {
        Self {
            packed: VecDeque::with_capacity(capacity.min(limit)),
            limit,
            tail: None,
        }
    }

    fn len(&self) -> usize {
        self.packed.len() + usize::from(self.tail.is_some())
    }

    fn tail_capacity(&self) -> usize {
        self.tail.as_ref().map_or(0, Row::capacity)
    }

    fn last_mut(&mut self) -> Option<&mut Row<Cell>> {
        self.tail.as_mut()
    }

    fn push(&mut self, row: Row<Cell>) {
        if let Some(last) = self.tail.replace(row) {
            self.packed
                .push_back(CompressedRow::encode(&last, last.len()));
            // shrink는 한 입력 행에서 여러 출력 행을 만든다. 보존 한도를 넘는 오래된
            // 출력은 즉시 버려, 마지막 truncate 전까지 수백만 행이 쌓이지 않게 한다.
            if self.len() > self.limit {
                self.packed.pop_front();
            }
        }
    }

    fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.tail = None;
            self.packed.truncate(len);
        }
    }

    fn resize_with(&mut self, len: usize, mut row: impl FnMut() -> Row<Cell>) {
        while self.len() < len {
            self.push(row());
        }
    }

    fn finish(mut self, columns: usize) -> Vec<CompressedRow> {
        if let Some(row) = self.tail.take() {
            self.packed
                .push_back(CompressedRow::encode(&row, row.len()));
        }
        // 짧은 원본 행의 나머지는 decode가 default 셀로 채운다.
        debug_assert!(columns > 0);
        self.packed.into_iter().rev().collect()
    }
}

impl Grid<Cell> {
    /// 두 grid와 Term의 상태 변경 전에 공유하는 크기 검증.
    pub(crate) fn preflight_resize(&self, lines: usize, columns: usize) {
        assert!(
            (1..=u16::MAX as usize).contains(&columns),
            "리플로 폭 범위 초과"
        );
        assert!(lines > 0, "리플로 화면 행 수는 양수여야 한다");
        let capacity = self
            .max_scroll_limit
            .checked_add(lines)
            .expect("리플로 행 수 overflow");
        assert!(capacity <= i32::MAX as usize, "리플로 행 좌표 범위 초과");
        let _ = capacity
            .checked_mul(columns)
            .and_then(|n| n.checked_mul(mem::size_of::<Cell>()))
            .expect("리플로 셀 바이트 overflow");
    }

    /// 전체 history inflate 없이 resize한다. 공개 입력 크기는 변경 전에 검증한다.
    pub fn resize_streaming(
        &mut self,
        reflow: bool,
        lines: usize,
        columns: usize,
    ) -> ReflowMetrics {
        self.preflight_resize(lines, columns);
        let changed = self.lines != lines || self.columns != columns;

        let mut metrics = ReflowMetrics::default();
        let template = mem::take(&mut self.cursor.template);
        match self.lines.cmp(&lines) {
            Ordering::Less => self.grow_lines(lines),
            Ordering::Greater => self.shrink_lines(lines),
            Ordering::Equal => (),
        }
        self.raw.inflate_visible(self.columns);
        match self.columns.cmp(&columns) {
            Ordering::Less => self.grow_columns_streaming(reflow, columns, &mut metrics),
            Ordering::Greater => self.shrink_columns_streaming(reflow, columns, &mut metrics),
            Ordering::Equal => (),
        }
        self.cursor.template = template;
        self.compress_history(0);
        if changed {
            self.invalidate_history_identity();
        }
        metrics
    }

    fn grow_columns_streaming(
        &mut self,
        reflow: bool,
        columns: usize,
        metrics: &mut ReflowMetrics,
    ) {
        let should_reflow = |row: &Row<Cell>| -> bool {
            let len = Column(row.len());
            reflow && len.0 > 0 && len < columns && row[len - 1].flags().contains(Flags::WRAPLINE)
        };

        let old_columns = self.columns;
        self.columns = columns;

        let mut reversed =
            PackedRows::with_capacity(self.raw.len(), self.max_scroll_limit + self.lines);
        let mut cursor_line_delta = 0;

        if self.cursor.input_needs_wrap && reflow {
            self.cursor.input_needs_wrap = false;
            self.cursor.point.column += 1;
        }

        let rows = self.raw.take_rows_streaming(old_columns);

        for (i, mut row) in rows.enumerate().rev() {
            metrics.observe(row.capacity() + reversed.tail_capacity());
            let last_row = match reversed.last_mut() {
                Some(last_row) if should_reflow(last_row) => last_row,
                _ => {
                    reversed.push(row);
                    continue;
                }
            };

            if let Some(cell) = last_row.last_mut() {
                cell.flags_mut().remove(Flags::WRAPLINE);
            }

            let mut last_len = last_row.len();
            if last_len >= 1
                && last_row[Column(last_len - 1)]
                    .flags()
                    .contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                last_row.shrink(last_len - 1);
                last_len -= 1;
            }

            let mut num_wrapped = columns - last_len;
            let len = min(row.len(), num_wrapped);

            let mut cells = if row[Column(len - 1)].flags().contains(Flags::WIDE_CHAR) {
                num_wrapped -= 1;

                let mut cells = row.front_split_off(len - 1);

                let mut spacer = Cell::default();
                spacer.flags_mut().insert(Flags::LEADING_WIDE_CHAR_SPACER);
                cells.push(spacer);

                cells
            } else {
                row.front_split_off(len)
            };

            last_row.append(&mut cells);
            metrics.observe(row.capacity() + last_row.capacity() + cells.capacity());

            let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;

            if i == cursor_buffer_line && reflow {
                let mut target = self.cursor.point.sub(self, Boundary::Cursor, num_wrapped);

                if target.column.0 == 0 && row.is_clear() {
                    self.cursor.input_needs_wrap = true;
                    target = target.sub(self, Boundary::Cursor, 1);
                }
                self.cursor.point.column = target.column;

                let line_delta = self.cursor.point.line - target.line;

                if line_delta != 0 && row.is_clear() {
                    continue;
                }

                cursor_line_delta += line_delta.0 as usize;
            } else if row.is_clear() {
                if i < self.display_offset {
                    self.display_offset = self.display_offset.saturating_sub(1);
                }

                if i < cursor_buffer_line {
                    self.cursor.point.line += 1;
                }

                continue;
            }

            if let Some(cell) = last_row.last_mut() {
                cell.flags_mut().insert(Flags::WRAPLINE);
            }

            reversed.push(row);
        }

        if reversed.len() < self.lines {
            let delta = (self.lines - reversed.len()) as i32;
            self.cursor.point.line = max(self.cursor.point.line - delta, Line(0));
            reversed.resize_with(self.lines, || Row::new(columns));
        }

        if cursor_line_delta != 0 {
            let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
            let available = min(cursor_buffer_line, reversed.len() - self.lines);
            let overflow = cursor_line_delta.saturating_sub(available);
            reversed.truncate(reversed.len() + overflow - cursor_line_delta);
            self.cursor.point.line = max(self.cursor.point.line - overflow, Line(0));
        }

        // 완성 행은 압축한 채 뒤집고, 최종 화면만 목표 폭으로 복원한다.
        self.raw
            .replace_compressed(reversed.finish(columns), columns);

        self.display_offset = min(self.display_offset, self.history_size());
    }

    fn shrink_columns_streaming(
        &mut self,
        reflow: bool,
        columns: usize,
        metrics: &mut ReflowMetrics,
    ) {
        let old_columns = self.columns;
        self.columns = columns;

        if self.cursor.input_needs_wrap && reflow {
            self.cursor.input_needs_wrap = false;
            self.cursor.point.column += 1;
        }

        let mut new_raw =
            PackedRows::with_capacity(self.raw.len(), self.max_scroll_limit + self.lines);
        let mut buffered: Option<Vec<Cell>> = None;

        let rows = self.raw.take_rows_streaming(old_columns);
        for (i, mut row) in rows.enumerate().rev() {
            metrics.observe(
                row.capacity()
                    + new_raw.tail_capacity()
                    + buffered.as_ref().map_or(0, Vec::capacity),
            );
            if let Some(buffered) = buffered.take() {
                let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                if i == cursor_buffer_line {
                    self.cursor.point.column += buffered.len();
                }

                row.append_front(buffered);
                metrics.observe(row.capacity() + new_raw.tail_capacity());
            }

            loop {
                let mut wrapped = match row.shrink(columns) {
                    Some(wrapped) if reflow => wrapped,
                    _ => {
                        let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                        if reflow && i == cursor_buffer_line && self.cursor.point.column > columns {
                            Vec::new()
                        } else {
                            new_raw.push(row);
                            break;
                        }
                    }
                };

                metrics.observe(row.capacity() + wrapped.capacity() + new_raw.tail_capacity());

                if row.len() >= columns
                    && row[Column(columns - 1)].flags().contains(Flags::WIDE_CHAR)
                {
                    let mut spacer = Cell::default();
                    spacer.flags_mut().insert(Flags::LEADING_WIDE_CHAR_SPACER);

                    let wide_char = mem::replace(&mut row[Column(columns - 1)], spacer);
                    wrapped.insert(0, wide_char);
                    metrics.observe(row.capacity() + wrapped.capacity() + new_raw.tail_capacity());
                }

                let len = wrapped.len();
                if len > 0
                    && wrapped[len - 1]
                        .flags()
                        .contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    if len == 1 {
                        row[Column(columns - 1)].flags_mut().insert(Flags::WRAPLINE);
                        new_raw.push(row);
                        break;
                    } else {
                        wrapped[len - 2].flags_mut().insert(Flags::WRAPLINE);
                        wrapped.truncate(len - 1);
                    }
                }

                new_raw.push(row);

                if let Some(cell) = new_raw.last_mut().and_then(|r| r.last_mut()) {
                    cell.flags_mut().insert(Flags::WRAPLINE);
                }

                if wrapped
                    .last()
                    .map(|c| c.flags().contains(Flags::WRAPLINE) && i >= 1)
                    .unwrap_or(false)
                    && wrapped.len() < columns
                {
                    if let Some(cell) = wrapped.last_mut() {
                        cell.flags_mut().remove(Flags::WRAPLINE);
                    }

                    buffered = Some(wrapped);
                    break;
                } else {
                    let cursor_buffer_line = self.lines - self.cursor.point.line.0 as usize - 1;
                    if (i == cursor_buffer_line && self.cursor.point.column < columns)
                        || i < cursor_buffer_line
                    {
                        self.cursor.point.line = max(self.cursor.point.line - 1, Line(0));
                    }

                    if i == cursor_buffer_line && self.cursor.point.column >= columns {
                        self.cursor.point.column -= columns;
                    }

                    let occ = wrapped.len();
                    if occ < columns {
                        wrapped.resize_with(columns, Cell::default);
                    }
                    row = Row::from_vec(wrapped, occ);

                    if i < self.display_offset {
                        self.display_offset += 1;
                    }
                }
            }
        }

        let mut reversed = new_raw.finish(columns);
        reversed.truncate(self.max_scroll_limit + self.lines);
        self.raw.replace_compressed(reversed, columns);

        self.display_offset = min(self.display_offset, self.history_size());

        if !reflow {
            self.cursor.point.column = min(self.cursor.point.column, Column(columns - 1));
        } else if self.cursor.point.column == columns
            && !self[self.cursor.point.line][Column(columns - 1)]
                .flags()
                .contains(Flags::WRAPLINE)
        {
            self.cursor.input_needs_wrap = true;
            self.cursor.point.column -= 1;
        } else {
            self.cursor.point = self.cursor.point.grid_clamp(self, Boundary::Cursor);
        }

        self.saved_cursor.point.column = min(self.saved_cursor.point.column, Column(columns - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::VoidListener;
    use crate::grid::Scroll;
    use crate::term::{Config, Term, test::TermSize};
    use crate::vte::ansi::Processor;

    fn assert_same(actual: &Grid<Cell>, expected: &Grid<Cell>, context: &str) {
        assert_eq!(
            actual.total_lines(),
            expected.total_lines(),
            "{context}: 행 수"
        );
        assert_eq!(actual.columns(), expected.columns(), "{context}: 열 수");
        assert_eq!(actual.cursor, expected.cursor, "{context}: 커서");
        assert_eq!(
            actual.saved_cursor, expected.saved_cursor,
            "{context}: 저장 커서"
        );
        assert_eq!(
            actual.display_offset(),
            expected.display_offset(),
            "{context}: 스크롤 위치"
        );
        let mut scratch = Row::new(actual.columns());
        for line in -(actual.history_size() as i32)..actual.screen_lines() as i32 {
            assert_eq!(
                actual.read_line(Line(line), &mut scratch),
                &expected[Line(line)],
                "{context}: 행 {line}"
            );
        }
    }

    #[test]
    fn streaming_resize_matches_stock_for_unicode_cursor_and_scrollback() {
        let samples = [
            "abcdefghijklmnopqrstuvw가나다e\u{301}👩\u{200d}💻",
            "a\r\nb\r\n  \r\nc\r\n",
            "\x1b[31m색상\x1b[0m\x1b]8;id=test;https://example.com\x1b\\링크\x1b]8;;\x1b\\\r\n",
            "ab가\x1b7\r\n12345\x1b8",
        ];
        for (old_columns, history_limit) in [(4, 19), (7, 19), (12, 19), (80, 19), (80, 10000)] {
            for sample in samples {
                let size = TermSize::new(old_columns, 6);
                let mut term = Term::new(
                    Config {
                        scrolling_history: history_limit,
                        ..Config::default()
                    },
                    &size,
                    VoidListener,
                );
                let mut parser: Processor = Processor::new();
                for _ in 0..50 {
                    parser.advance(&mut term, sample.as_bytes());
                }
                for reflow in [true, false] {
                    let mut expected = term.grid().clone();
                    expected.scroll_display(Scroll::Delta(9));
                    let mut actual = expected.clone();
                    actual.compress_history(2);
                    for (columns, lines) in [(3, 4), (19, 10), (5, 3), (80, 6), (80, 12)] {
                        expected.resize(reflow, lines, columns);
                        actual.resize_streaming(reflow, lines, columns);
                        let context = format!(
                            "{old_columns} → {columns}x{lines}, reflow={reflow}, {sample:?}"
                        );
                        assert_same(&actual, &expected, &context);
                        assert_eq!(actual.compressed_row_count(), actual.history_size());
                    }
                }
            }
        }
    }

    #[test]
    fn streaming_resize_rejects_invalid_sizes_before_mutation() {
        for (lines, columns) in [(0, 80), (24, 0), (24, 65536), (usize::MAX, 80)] {
            let mut grid = Grid::<Cell>::new(24, 80, 100);
            grid[Line(0)][Column(0)].c = '가';
            let before = grid.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                grid.resize_streaming(true, lines, columns);
            }));
            assert!(result.is_err());
            assert_same(&grid, &before, "잘못된 크기 거부");
        }
    }
}
