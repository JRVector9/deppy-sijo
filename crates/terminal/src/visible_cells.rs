//! Immutable local rows, with the original flat sequence on the wire.
use std::ops::{
    Deref, Index, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo, RangeToInclusive,
};
use std::sync::{Arc, OnceLock};

use crate::TerminalCell;

const ARC_HEADER_BYTES: usize = 2 * std::mem::size_of::<usize>();

#[derive(Debug)]
enum Storage {
    Flat(Arc<[TerminalCell]>),
    Rows {
        cols: usize,
        rows: Arc<[Arc<[TerminalCell]>]>,
        flat: OnceLock<Arc<[TerminalCell]>>,
    },
}

/// The cell sequence can share unchanged rows without copying a complete viewport.
#[derive(Debug, Clone)]
pub struct VisibleCells(Arc<Storage>);

impl VisibleCells {
    pub(crate) fn from_rows(cols: usize, rows: Vec<Arc<[TerminalCell]>>) -> Self {
        assert!(cols > 0 && rows.iter().all(|row| row.len() == cols));
        Self(Arc::new(Storage::Rows {
            cols,
            rows: rows.into(),
            flat: OnceLock::new(),
        }))
    }

    pub fn len(&self) -> usize {
        match &*self.0 {
            Storage::Flat(cells) => cells.len(),
            Storage::Rows { cols, rows, .. } => cols * rows.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get<I: VisibleCellIndex>(&self, index: I) -> Option<&I::Output> {
        index.get(self)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &TerminalCell> {
        let (flat, rows) = match &*self.0 {
            Storage::Flat(cells) => (Some(&**cells), None),
            Storage::Rows { rows, .. } => (None, Some(&**rows)),
        };
        flat.into_iter().flat_map(|cells| cells.iter()).chain(
            rows.into_iter()
                .flat_map(|rows| rows.iter().flat_map(|row| row.iter())),
        )
    }
    pub fn to_vec(&self) -> Vec<TerminalCell> {
        let mut cells = Vec::with_capacity(self.len());
        cells.extend(self.iter().copied());
        cells
    }
    pub fn chunks(
        &self,
        chunk_size: usize,
    ) -> impl DoubleEndedIterator<Item = &[TerminalCell]> + ExactSizeIterator {
        assert!(chunk_size > 0, "chunk size must be positive");
        (0..self.len().div_ceil(chunk_size)).map(move |chunk| {
            let start = chunk * chunk_size;
            self.range(start..(start + chunk_size).min(self.len()))
                .unwrap()
        })
    }

    pub(crate) fn shared_rows(&self) -> Option<&[Arc<[TerminalCell]>]> {
        match &*self.0 {
            Storage::Rows { rows, .. } => Some(rows),
            _ => None,
        }
    }

    /// Resident requested heap, excluding allocator overhead; reads never scan history/rows.
    pub(crate) fn heap_bytes(&self) -> usize {
        let storage = ARC_HEADER_BYTES + std::mem::size_of::<Storage>();
        match &*self.0 {
            Storage::Flat(cells) => storage + ARC_HEADER_BYTES + std::mem::size_of_val(&**cells),
            Storage::Rows { cols, rows, flat } => {
                storage
                    + ARC_HEADER_BYTES
                    + std::mem::size_of_val(&**rows)
                    + rows.len() * (ARC_HEADER_BYTES + cols * std::mem::size_of::<TerminalCell>())
                    + flat.get().map_or(0, |cells| {
                        ARC_HEADER_BYTES + std::mem::size_of_val(&**cells)
                    })
            }
        }
    }

    fn flat(&self) -> &[TerminalCell] {
        match &*self.0 {
            Storage::Flat(cells) => cells,
            Storage::Rows { rows, flat, .. } => flat.get_or_init(|| {
                let mut cells = Vec::with_capacity(self.len());
                cells.extend(rows.iter().flat_map(|row| row.iter().copied()));
                cells.into()
            }),
        }
    }

    fn range(&self, range: Range<usize>) -> Option<&[TerminalCell]> {
        if range.start > range.end || range.end > self.len() {
            return None;
        }
        if range.is_empty() {
            return Some(&[]);
        }
        match &*self.0 {
            Storage::Flat(cells) => cells.get(range),
            Storage::Rows { cols, rows, .. } if range.start / cols == (range.end - 1) / cols => {
                let row = range.start / cols;
                rows[row].get(range.start - row * cols..range.end - row * cols)
            }
            _ => self.flat().get(range),
        }
    }
}

impl From<Vec<TerminalCell>> for VisibleCells {
    fn from(cells: Vec<TerminalCell>) -> Self {
        Self::from(Arc::<[TerminalCell]>::from(cells))
    }
}
impl From<Arc<[TerminalCell]>> for VisibleCells {
    fn from(cells: Arc<[TerminalCell]>) -> Self {
        Self(Arc::new(Storage::Flat(cells)))
    }
}
impl Default for VisibleCells {
    fn default() -> Self {
        static EMPTY: OnceLock<VisibleCells> = OnceLock::new();
        EMPTY.get_or_init(|| Vec::new().into()).clone()
    }
}
impl PartialEq for VisibleCells {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.len() == other.len() && self.iter().eq(other.iter()))
    }
}
impl Deref for VisibleCells {
    type Target = [TerminalCell];
    fn deref(&self) -> &Self::Target {
        self.flat()
    }
}
impl AsRef<[TerminalCell]> for VisibleCells {
    fn as_ref(&self) -> &[TerminalCell] {
        self.flat()
    }
}

/// Slice-style indexing that borrows a single row directly instead of flattening it.
pub trait VisibleCellIndex {
    type Output: ?Sized;
    fn get(self, cells: &VisibleCells) -> Option<&Self::Output>;
}
impl VisibleCellIndex for usize {
    type Output = TerminalCell;
    fn get(self, cells: &VisibleCells) -> Option<&TerminalCell> {
        match &*cells.0 {
            Storage::Flat(flat) => flat.get(self),
            Storage::Rows { cols, rows, .. } => rows.get(self / cols)?.get(self % cols),
        }
    }
}
impl VisibleCellIndex for Range<usize> {
    type Output = [TerminalCell];
    fn get(self, cells: &VisibleCells) -> Option<&[TerminalCell]> {
        cells.range(self)
    }
}
macro_rules! range_index {
    ($ty:ty, $convert:expr) => {
        impl VisibleCellIndex for $ty {
            type Output = [TerminalCell];
            fn get(self, cells: &VisibleCells) -> Option<&[TerminalCell]> {
                let range: Option<Range<usize>> = ($convert)(self, cells.len());
                cells.range(range?)
            }
        }
    };
}
range_index!(RangeTo<usize>, |r: RangeTo<usize>, _| Some(0..r.end));
range_index!(RangeFrom<usize>, |r: RangeFrom<usize>, len| Some(
    r.start..len
));
range_index!(RangeFull, |_: RangeFull, len| Some(0..len));
range_index!(RangeInclusive<usize>, |r: RangeInclusive<usize>, _| r
    .end()
    .checked_add(1)
    .map(|end| *r.start()..end));
range_index!(RangeToInclusive<usize>, |r: RangeToInclusive<usize>, _| r
    .end
    .checked_add(1)
    .map(|end| 0..end));
impl<I: VisibleCellIndex> Index<I> for VisibleCells {
    type Output = I::Output;
    fn index(&self, index: I) -> &Self::Output {
        self.get(index).expect("visible cell index out of bounds")
    }
}
impl serde::Serialize for VisibleCells {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for cell in self.iter() {
            sequence.serialize_element(cell)?;
        }
        sequence.end()
    }
}
impl<'de> serde::Deserialize<'de> for VisibleCells {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<TerminalCell> as serde::Deserialize>::deserialize(deserializer).map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rows() -> VisibleCells {
        VisibleCells::from_rows(
            2,
            ['a', 'b', 'c']
                .into_iter()
                .map(|c| {
                    vec![
                        TerminalCell::new(
                            c,
                            [1, 2, 3],
                            [4, 5, 6],
                            false,
                            false,
                            crate::CellAttrs::empty()
                        );
                        2
                    ]
                    .into()
                })
                .collect(),
        )
    }
    #[test]
    fn row_index_and_iteration_do_not_materialize_flat_storage() {
        let cells = rows();
        let original = cells.heap_bytes();
        assert_eq!(cells.get(2).unwrap().c, 'b');
        assert_eq!(cells.get(2..4).unwrap().len(), 2);
        assert_eq!(cells[..2][0].c, 'a');
        assert_eq!(cells.iter().map(|c| c.c).collect::<String>(), "aabbcc");
        assert_eq!(
            cells.iter().rev().map(|c| c.c).collect::<String>(),
            "ccbbaa"
        );
        assert_eq!(cells.to_vec().len(), 6);
        assert_eq!(cells.heap_bytes(), original);
        assert!(cells.get(6).is_none());
        assert!(cells.get(4..7).is_none());
        assert!(cells.get(4..3).is_none());
        assert!(cells.get(6..6).unwrap().is_empty());
        assert!(cells.get(..=usize::MAX).is_none());
        assert_eq!(cells[1..5].len(), 4);
        assert_eq!(
            cells.heap_bytes() - original,
            ARC_HEADER_BYTES + 6 * std::mem::size_of::<TerminalCell>()
        );
        assert_eq!(cells[1..5].len(), 4);
        assert_eq!(cells[..].len(), 6);
    }
    #[test]
    fn wire_serialization_is_flat_without_flat_cache_allocation() {
        let cells = rows();
        let original = cells.heap_bytes();
        let flat: Arc<[TerminalCell]> = cells.to_vec().into();
        assert_eq!(
            serde_json::to_vec(&cells).unwrap(),
            serde_json::to_vec(&flat).unwrap()
        );
        let roundtrip: VisibleCells =
            serde_json::from_slice(&serde_json::to_vec(&cells).unwrap()).unwrap();
        assert_eq!(cells, roundtrip);
        assert_eq!(
            postcard::to_allocvec(&cells).unwrap(),
            postcard::to_allocvec(&flat).unwrap()
        );
        let roundtrip: VisibleCells =
            postcard::from_bytes(&postcard::to_allocvec(&cells).unwrap()).unwrap();
        assert_eq!(cells, roundtrip);
        assert_eq!(cells.chunks(2).count(), 3);
        assert_eq!(cells.heap_bytes(), original);
    }
}
