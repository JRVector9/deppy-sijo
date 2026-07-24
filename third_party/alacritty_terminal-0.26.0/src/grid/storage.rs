use std::cmp::max;
use std::mem;
use std::mem::MaybeUninit;
use std::ops::{Index, IndexMut};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::Row;
use super::compressed::CompressedRow;
use crate::grid::GridCell;
use crate::index::Line;
use crate::term::cell::{Cell, ResetDiscriminant};

/// Maximum number of buffered lines outside of the grid for performance optimization.
const MAX_CACHE_SIZE: usize = 1_000;

/// A ring buffer for optimizing indexing and rotation.
///
/// The [`Storage::rotate`] and [`Storage::rotate_down`] functions are fast modular additions on
/// the internal [`zero`] field. As compared with [`slice::rotate_left`] which must rearrange items
/// in memory.
///
/// As a consequence, both [`Index`] and [`IndexMut`] are reimplemented for this type to account
/// for the zeroth element not always being at the start of the allocation.
///
/// Because certain [`Vec`] operations are no longer valid on this type, no [`Deref`]
/// implementation is provided. Anything from [`Vec`] that should be exposed must be done so
/// manually.
///
/// [`slice::rotate_left`]: https://doc.rust-lang.org/std/primitive.slice.html#method.rotate_left
/// [`Deref`]: std::ops::Deref
/// [`zero`]: #structfield.zero
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Storage<T> {
    inner: Vec<Row<T>>,

    /// Starting point for the storage of rows.
    ///
    /// This value represents the starting line offset within the ring buffer. The value of this
    /// offset may be larger than the `len` itself, and will wrap around to the start to form the
    /// ring buffer. It represents the bottommost line of the terminal.
    zero: usize,

    /// Number of visible lines.
    visible_lines: usize,

    /// Total number of lines currently active in the terminal (scrollback + visible)
    ///
    /// Shrinking this length allows reducing the number of lines in the scrollback buffer without
    /// having to truncate the raw `inner` buffer.
    /// As long as `len` is bigger than `inner`, it is also possible to grow the scrollback buffer
    /// without any additional insertions.
    len: usize,

    /// deppy-sijo 옵션 D: 스크롤아웃된 행의 압축 표현 곁가지 배열.
    ///
    /// `inner`와 물리 슬롯을 1:1로 공유한다(같은 `zero`/`compute_index` 사용). 규약:
    /// 비어 있으면(len 0) "압축 없음"이고, 하나라도 압축되면 `inner.len()`과 길이가
    /// 같아진다. 슬롯 `i`가 `Some`이면 `inner[i]`는 빈 placeholder(힙 0)이고 실제
    /// 내용은 여기 압축돼 있다. 구조 변경(swap/rezero/truncate/initialize)마다
    /// `(inner[i], compressed[i])`를 **쌍으로** 이동시켜 정합성을 유지한다.
    ///
    /// serde에서는 건너뛴다(ref-test JSON 불변 + 역직렬화 시 빈 채로 시작 = 압축 없음).
    #[cfg_attr(feature = "serde", serde(skip))]
    compressed: Vec<Option<CompressedRow>>,
}

impl<T: PartialEq> PartialEq for Storage<T> {
    fn eq(&self, other: &Self) -> bool {
        // Both storage buffers need to be truncated and zeroed.
        assert_eq!(self.zero, 0);
        assert_eq!(other.zero, 0);

        self.inner == other.inner && self.len == other.len
    }
}

impl<T> Storage<T> {
    #[inline]
    pub fn with_capacity(visible_lines: usize, columns: usize) -> Storage<T>
    where
        T: Default,
    {
        // Initialize visible lines; the scrollback buffer is initialized dynamically.
        let mut inner = Vec::with_capacity(visible_lines);
        inner.resize_with(visible_lines, || Row::new(columns));

        // compressed는 lazy: 첫 압축 전까지 빈 채로 둔다(오버헤드 0).
        Storage { inner, zero: 0, visible_lines, len: visible_lines, compressed: Vec::new() }
    }

    /// Increase the number of lines in the buffer.
    #[inline]
    pub fn grow_visible_lines(&mut self, next: usize)
    where
        T: Default,
    {
        // Number of lines the buffer needs to grow.
        let additional_lines = next - self.visible_lines;

        let columns = self[Line(0)].len();
        self.initialize(additional_lines, columns);

        // Update visible lines.
        self.visible_lines = next;
    }

    /// Decrease the number of lines in the buffer.
    #[inline]
    pub fn shrink_visible_lines(&mut self, next: usize) {
        // Shrink the size without removing any lines.
        let shrinkage = self.visible_lines - next;
        self.shrink_lines(shrinkage);

        // Update visible lines.
        self.visible_lines = next;
    }

    /// Shrink the number of lines in the buffer.
    #[inline]
    pub fn shrink_lines(&mut self, shrinkage: usize) {
        self.len -= shrinkage;

        // Free memory.
        if self.inner.len() > self.len + MAX_CACHE_SIZE {
            self.truncate();
        }
    }

    /// Truncate the invisible elements from the raw buffer.
    #[inline]
    pub fn truncate(&mut self) {
        self.rezero();

        self.inner.truncate(self.len);
        // 곁가지도 같은 꼬리 슬롯(가장 오래된 history)을 버린다.
        if !self.compressed.is_empty() {
            self.compressed.truncate(self.len);
        }
    }

    /// Dynamically grow the storage buffer at runtime.
    #[inline]
    pub fn initialize(&mut self, additional_rows: usize, columns: usize)
    where
        T: Default,
    {
        if self.len + additional_rows > self.inner.len() {
            self.rezero();

            let realloc_size = self.inner.len() + max(additional_rows, MAX_CACHE_SIZE);
            self.inner.resize_with(realloc_size, || Row::new(columns));
            // 곁가지도 같은 길이로: 새로 추가된(가장 최근) 슬롯은 None(비압축).
            if !self.compressed.is_empty() {
                self.compressed.resize_with(realloc_size, || None);
            }
        }

        self.len += additional_rows;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// 압축된 슬롯이 하나라도 있는지 (디버그 방어용). O(n)이라 debug_assert에서만 쓴다.
    #[inline]
    pub fn has_compressed(&self) -> bool {
        self.compressed.iter().any(Option::is_some)
    }

    /// `line`을 template으로 초기화한다. **압축된 슬롯 재활용을 안전하게 처리한다**:
    /// 압축된 placeholder(0용량)를 그대로 `Row::reset`하면 `inner[len-1]` 접근에서
    /// 패닉/UB가 나므로, 압축돼 있으면 곁가지를 버리고(내용은 어차피 폐기됨) full-width
    /// 행으로 되살린 뒤 reset한다. 비압축 슬롯이면 기존과 동일한 reset일 뿐이다.
    #[inline]
    pub fn reset_row<D>(&mut self, line: Line, template: &T, columns: usize)
    where
        T: ResetDiscriminant<D> + GridCell + Default,
        D: PartialEq,
    {
        let idx = self.compute_index(line);
        if !self.compressed.is_empty() {
            if let Some(slot) = self.compressed.get_mut(idx) {
                if slot.take().is_some() {
                    self.inner[idx] = Row::new(columns);
                }
            }
        }
        self.inner[idx].reset(template);
    }

    /// Swap implementation for Row<T>.
    ///
    /// Exploits the known size of Row<T> to produce a slightly more efficient
    /// swap than going through slice::swap.
    ///
    /// The default implementation from swap generates 8 movups and 4 movaps
    /// instructions. This implementation achieves the swap in only 8 movups
    /// instructions.
    pub fn swap(&mut self, a: Line, b: Line) {
        debug_assert_eq!(mem::size_of::<Row<T>>(), mem::size_of::<usize>() * 4);

        let a = self.compute_index(a);
        let b = self.compute_index(b);

        unsafe {
            // Cast to a qword array to opt out of copy restrictions and avoid
            // drop hazards. Byte array is no good here since for whatever
            // reason LLVM won't optimized it.
            let a_ptr = self.inner.as_mut_ptr().add(a) as *mut MaybeUninit<usize>;
            let b_ptr = self.inner.as_mut_ptr().add(b) as *mut MaybeUninit<usize>;

            // Copy 1 qword at a time.
            //
            // The optimizer unrolls this loop and vectorizes it.
            let mut tmp: MaybeUninit<usize>;
            for i in 0..4 {
                tmp = *a_ptr.offset(i);
                *a_ptr.offset(i) = *b_ptr.offset(i);
                *b_ptr.offset(i) = tmp;
            }
        }

        // (inner[i], compressed[i])는 항상 쌍으로 움직인다.
        if !self.compressed.is_empty() {
            self.compressed.swap(a, b);
        }
    }

    /// Rotate the grid, moving all lines up/down in history.
    #[inline]
    pub fn rotate(&mut self, count: isize) {
        debug_assert!(count.unsigned_abs() <= self.inner.len());

        let len = self.inner.len();
        self.zero = (self.zero as isize + count + len as isize) as usize % len;
    }

    /// Rotate all existing lines down in history.
    ///
    /// This is a faster, specialized version of [`rotate_left`].
    ///
    /// [`rotate_left`]: https://doc.rust-lang.org/std/vec/struct.Vec.html#method.rotate_left
    #[inline]
    pub fn rotate_down(&mut self, count: usize) {
        self.zero = (self.zero + count) % self.inner.len();
    }

    /// Update the raw storage buffer.
    #[inline]
    pub fn replace_inner(&mut self, vec: Vec<Row<T>>) {
        self.len = vec.len();
        self.inner = vec;
        self.zero = 0;
        // 새 inner는 압축 이력이 없다(resize/reflow는 복원된 행을 넘겨준다).
        self.compressed.clear();
    }

    /// Remove all rows from storage.
    #[inline]
    pub fn take_all(&mut self) -> Vec<Row<T>> {
        self.truncate();

        let mut buffer = Vec::new();

        mem::swap(&mut buffer, &mut self.inner);
        self.len = 0;
        self.compressed.clear();

        buffer
    }

    /// Compute actual index in underlying storage given the requested index.
    #[inline]
    fn compute_index(&self, requested: Line) -> usize {
        debug_assert!(requested.0 < self.visible_lines as i32);

        let positive = -(requested - self.visible_lines).0 as usize - 1;

        debug_assert!(positive < self.len);

        let zeroed = self.zero + positive;

        // Use if/else instead of remainder here to improve performance.
        //
        // Requires `zeroed` to be smaller than `self.inner.len() * 2`,
        // but both `self.zero` and `requested` are always smaller than `self.inner.len()`.
        if zeroed >= self.inner.len() { zeroed - self.inner.len() } else { zeroed }
    }

    /// Rotate the ringbuffer to reset `self.zero` back to index `0`.
    #[inline]
    fn rezero(&mut self) {
        if self.zero == 0 {
            return;
        }

        // 곁가지도 동일하게 회전시켜 물리 슬롯 쌍을 보존한다.
        if !self.compressed.is_empty() {
            self.compressed.rotate_left(self.zero);
        }
        self.inner.rotate_left(self.zero);
        self.zero = 0;
    }
}

impl Storage<Cell> {
    /// `line`을 압축한다. 이미 압축됐거나 빈(placeholder) 행이면 `None`(=이 슬롯은
    /// 압축 frontier), 새로 압축했으면 `Some(회수 추정 힙 바이트)`. 회수량이 0이어도
    /// (거의 꽉 찬 incompressible 행) 상태는 바뀌었으므로 `Some(0)`이지 `None`이 아니다
    /// — 호출자(compress_history)의 frontier 판정이 어긋나지 않게 하기 위함.
    ///
    /// 압축 후 `inner[idx]`는 힙 0인 placeholder가 되고, 실제 내용은 곁가지에 남는다.
    /// 이후 이 행을 읽으려면 반드시 [`read_line`](Self::read_line)을 거쳐야 한다
    /// (원시 `Index<Line>`은 placeholder를 돌려준다).
    pub(crate) fn compress_line(&mut self, line: Line, columns: usize) -> Option<usize> {
        let idx = self.compute_index(line);
        if self.compressed.get(idx).is_some_and(Option::is_some) {
            return None;
        }
        let raw_bytes = self.inner[idx].len() * mem::size_of::<Cell>();
        if raw_bytes == 0 {
            return None;
        }
        let compressed = CompressedRow::encode(&self.inner[idx], columns);
        let saved = raw_bytes.saturating_sub(compressed.heap_bytes());

        // 첫 압축에서만 곁가지를 inner와 같은 길이로 실체화(lazy).
        if self.compressed.is_empty() {
            self.compressed.resize_with(self.inner.len(), || None);
        }
        self.compressed[idx] = Some(compressed);
        // 셀 배열의 힙을 즉시 반납(용량까지 드롭).
        self.inner[idx] = Row::from_vec(Vec::new(), 0);
        Some(saved)
    }

    /// `line`을 읽는다. 압축돼 있으면 `scratch`로 복원해 그 참조를, 아니면 원시
    /// `inner` 행 참조를 돌려준다. 어느 쪽이든 호출자는 동일한 `&Row<Cell>`을 본다.
    pub(crate) fn read_line<'a>(
        &'a self,
        line: Line,
        columns: usize,
        scratch: &'a mut Row<Cell>,
    ) -> &'a Row<Cell> {
        let idx = self.compute_index(line);
        match self.compressed.get(idx).and_then(Option::as_ref) {
            Some(compressed) => {
                *scratch = compressed.decode(columns);
                scratch
            },
            None => &self.inner[idx],
        }
    }

    /// 현재 압축 곁가지가 점유하는 힙 바이트 추정 — RSS 예산/실측용.
    pub(crate) fn compressed_heap_bytes(&self) -> usize {
        self.compressed
            .iter()
            .filter_map(Option::as_ref)
            .map(CompressedRow::heap_bytes)
            .sum()
    }

    /// 현재 압축된(placeholder) 슬롯 수. footprint에서 raw 히스토리 행 수를
    /// `history - 이 값`으로 정확히 산정하는 데 쓴다(클래스 모델의 과소/이중계상 제거).
    pub(crate) fn compressed_row_count(&self) -> usize {
        self.compressed.iter().filter(|slot| slot.is_some()).count()
    }

    /// 모든 압축 슬롯을 복원해 stock 상태로 되돌린다(곁가지 비움). resize/reflow처럼
    /// 히스토리 전체를 원시 인덱싱으로 훑는 연산 직전에 호출한다.
    pub(crate) fn inflate_all(&mut self, columns: usize) {
        if self.compressed.is_empty() {
            return;
        }
        for idx in 0..self.inner.len() {
            if let Some(compressed) = self.compressed[idx].take() {
                self.inner[idx] = compressed.decode(columns);
            }
        }
        self.compressed.clear();
    }
}

impl<T> Index<Line> for Storage<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: Line) -> &Self::Output {
        let index = self.compute_index(index);
        &self.inner[index]
    }
}

impl<T> IndexMut<Line> for Storage<T> {
    #[inline]
    fn index_mut(&mut self, index: Line) -> &mut Self::Output {
        let index = self.compute_index(index);
        &mut self.inner[index]
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::GridCell;
    use crate::grid::row::Row;
    use crate::grid::storage::{MAX_CACHE_SIZE, Storage};
    use crate::index::{Column, Line};
    use crate::term::cell::Flags;

    impl GridCell for char {
        fn is_empty(&self) -> bool {
            *self == ' ' || *self == '\t'
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

    #[test]
    fn with_capacity() {
        let storage = Storage::<char>::with_capacity(3, 1);

        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.len, 3);
        assert_eq!(storage.zero, 0);
        assert_eq!(storage.visible_lines, 3);
    }

    #[test]
    fn indexing() {
        let mut storage = Storage::<char>::with_capacity(3, 1);

        storage[Line(0)] = filled_row('0');
        storage[Line(1)] = filled_row('1');
        storage[Line(2)] = filled_row('2');

        storage.zero += 1;

        assert_eq!(storage[Line(0)], filled_row('2'));
        assert_eq!(storage[Line(1)], filled_row('0'));
        assert_eq!(storage[Line(2)], filled_row('1'));
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn indexing_above_inner_len() {
        let storage = Storage::<char>::with_capacity(1, 1);
        let _ = &storage[Line(-1)];
    }

    #[test]
    fn rotate() {
        let mut storage = Storage::<char>::with_capacity(3, 1);
        storage.rotate(2);
        assert_eq!(storage.zero, 2);
        storage.shrink_lines(2);
        assert_eq!(storage.len, 1);
        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.zero, 2);
    }

    /// Grow the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   MAX_CACHE_SIZE: \0
    #[test]
    fn grow_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 3,
            len: 3,
            compressed: Vec::new(),
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 4,
            len: 4,
            compressed: Vec::new(),
        };
        expected.inner.append(&mut vec![filled_row('\0'); MAX_CACHE_SIZE]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Grow the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: -
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   MAX_CACHE_SIZE: \0
    #[test]
    fn grow_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('-'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 3,
            len: 3,
            compressed: Vec::new(),
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            zero: 0,
            visible_lines: 4,
            len: 4,
            compressed: Vec::new(),
        };
        expected.inner.append(&mut vec![filled_row('\0'); MAX_CACHE_SIZE]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: 2
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 2 <- Hidden
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn shrink_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 3,
            len: 3,
            compressed: Vec::new(),
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            zero: 1,
            visible_lines: 2,
            len: 2,
            compressed: Vec::new(),
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2 <- Hidden
    #[test]
    fn shrink_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            zero: 0,
            visible_lines: 3,
            len: 3,
            compressed: Vec::new(),
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            zero: 0,
            visible_lines: 2,
            len: 2,
            compressed: Vec::new(),
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer at the start and end of the buffer.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    #[test]
    fn shrink_before_and_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 6,
            len: 6,
            compressed: Vec::new(),
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 2,
            len: 2,
            compressed: Vec::new(),
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Check that when truncating all hidden lines are removed from the raw buffer.
    ///
    /// Before:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn truncate_invisible_lines() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 1,
            len: 2,
            compressed: Vec::new(),
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1')],
            zero: 0,
            visible_lines: 1,
            len: 2,
            compressed: Vec::new(),
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Truncate buffer only at the beginning.
    ///
    /// Before:
    ///   0: 1
    ///   1: 2 <- Hidden
    ///   2: 0 <- Zero
    /// After:
    ///   0: 1
    ///   0: 0 <- Zero
    #[test]
    fn truncate_invisible_lines_beginning() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('1'), filled_row('2'), filled_row('0')],
            zero: 2,
            visible_lines: 1,
            len: 2,
            compressed: Vec::new(),
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1')],
            zero: 0,
            visible_lines: 1,
            len: 2,
            compressed: Vec::new(),
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// First shrink the buffer and then grow it again.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After Shrinking:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3 <- Hidden
    /// After Growing:
    ///   0: 4
    ///   1: 5
    ///   2: -
    ///   3: 0 <- Zero
    ///   4: 1
    ///   5: 2
    ///   6: 3
    #[test]
    fn shrink_then_grow() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 6,
            compressed: Vec::new(),
        };

        // Shrink buffer.
        storage.shrink_lines(3);

        // Make sure the result after shrinking is correct.
        let shrinking_expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 3,
            compressed: Vec::new(),
        };
        assert_eq!(storage.inner, shrinking_expected.inner);
        assert_eq!(storage.zero, shrinking_expected.zero);
        assert_eq!(storage.len, shrinking_expected.len);

        // Grow buffer.
        storage.initialize(1, 1);

        // Make sure the previously freed elements are reused.
        let growing_expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 4,
            compressed: Vec::new(),
        };

        assert_eq!(storage.inner, growing_expected.inner);
        assert_eq!(storage.zero, growing_expected.zero);
        assert_eq!(storage.len, growing_expected.len);
    }

    #[test]
    fn initialize() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            zero: 2,
            visible_lines: 0,
            len: 6,
            compressed: Vec::new(),
        };

        // Initialize additional lines.
        let init_size = 3;
        storage.initialize(init_size, 1);

        // Generate expected grid.
        let mut expected_inner = vec![
            filled_row('0'),
            filled_row('1'),
            filled_row('2'),
            filled_row('3'),
            filled_row('4'),
            filled_row('5'),
        ];
        let expected_init_size = std::cmp::max(init_size, MAX_CACHE_SIZE);
        expected_inner.append(&mut vec![filled_row('\0'); expected_init_size]);
        let expected_storage =
            Storage { inner: expected_inner, zero: 0, visible_lines: 0, len: 9, compressed: Vec::new() };

        assert_eq!(storage.len, expected_storage.len);
        assert_eq!(storage.zero, expected_storage.zero);
        assert_eq!(storage.inner, expected_storage.inner);
    }

    #[test]
    fn rotate_wrap_zero() {
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('-'), filled_row('-'), filled_row('-')],
            zero: 2,
            visible_lines: 0,
            len: 3,
            compressed: Vec::new(),
        };

        storage.rotate(2);

        assert!(storage.zero < storage.inner.len());
    }

    fn filled_row(content: char) -> Row<char> {
        let mut row = Row::new(1);
        row[Column(0)] = content;
        row
    }
}
