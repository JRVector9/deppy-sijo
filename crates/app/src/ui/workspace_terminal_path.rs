//! Bounded path extraction from visible terminal cells, without terminal or host I/O.

const MAX_PATH_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum PathCell<'a> {
    Glyph(char, Option<&'a str>),
    Continuation,
    WrapFiller,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PathCandidate {
    /// Exact visible token. Only the host can distinguish filename punctuation from prose.
    pub word: String,
    pub start: usize,
    pub end: usize,
}

pub(crate) fn candidate_at<'a>(
    cols: usize,
    cells: usize,
    index: usize,
    cell_at: impl Fn(usize) -> PathCell<'a>,
) -> Option<PathCandidate> {
    if cols == 0 || index >= cells || matches!(cell_at(index), PathCell::WrapFiller) {
        return None;
    }
    let content = |i| match cell_at(i) {
        PathCell::Glyph(c, _) => !c.is_whitespace() && !c.is_control(),
        PathCell::Continuation => i % cols != 0,
        PathCell::WrapFiller => i % cols == cols - 1,
    };
    if !content(index) {
        return None;
    }
    let mut start = index;
    let mut end = index;
    while start > 0 && content(start - 1) {
        start -= 1;
        if index - start >= MAX_PATH_BYTES * 2 {
            return None;
        }
    }
    while end + 1 < cells && content(end + 1) {
        end += 1;
        if end - start >= MAX_PATH_BYTES * 2 {
            return None;
        }
    }

    // The snapshot has no wrap flags. Join only contiguous nonblank edge cells and let the
    // existing host resolver confirm the complete candidate before presenting it as a link.
    let mut word = String::new();
    let mut glyphs = Vec::new();
    for i in start..=end {
        if let PathCell::Glyph(c, grapheme) = cell_at(i) {
            let byte = word.len();
            if let Some(text) = grapheme {
                if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return None;
                }
                word.push_str(text);
            } else {
                word.push(c);
            }
            if word.len() > MAX_PATH_BYTES {
                return None;
            }
            glyphs.push((byte, i));
        }
    }
    let trimmed = word.trim_start_matches(|c: char| "\"'`([{<".contains(c));
    let first_byte = word.len() - trimmed.len();
    let mut last_byte = word.len();
    let mut balance = [0_i32; 4];
    for c in trimmed.chars() {
        match c {
            '(' => balance[0] += 1,
            ')' => balance[0] -= 1,
            '[' => balance[1] += 1,
            ']' => balance[1] -= 1,
            '{' => balance[2] += 1,
            '}' => balance[2] -= 1,
            '<' => balance[3] += 1,
            '>' => balance[3] -= 1,
            _ => {}
        }
    }
    loop {
        let text = &word[first_byte..last_byte];
        let last = text.chars().next_back()?;
        let closer = match last {
            ')' => Some(0),
            ']' => Some(1),
            '}' => Some(2),
            '>' => Some(3),
            _ => None,
        };
        if ".,;:!?\"'`".contains(last) || closer.is_some_and(|i| balance[i] < 0) {
            last_byte -= last.len_utf8();
            if let Some(i) = closer {
                balance[i] += 1;
            }
        } else {
            break;
        }
    }
    let text = &word[first_byte..last_byte];
    if !(text.starts_with('/')
        || text.starts_with("~/")
        || text.starts_with("./")
        || text.starts_with("../"))
    {
        return None;
    }
    let first = glyphs.iter().find(|(byte, _)| *byte == first_byte)?.1;
    let mut last = glyphs.iter().rfind(|(byte, _)| *byte < last_byte)?.1;
    if last + 1 < cells && matches!(cell_at(last + 1), PathCell::Continuation) {
        last += 1;
    }
    if !(first..=last).contains(&index) {
        return None;
    }
    Some(PathCandidate {
        word,
        start: first,
        end: last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(cols: usize, text: &str, index: usize) -> Option<PathCandidate> {
        let cells: Vec<_> = text.chars().collect();
        candidate_at(cols, cells.len(), index, |i| {
            PathCell::Glyph(cells[i], None)
        })
    }

    #[test]
    fn terminal_path_helper_extracts_long_absolute_path_from_every_visible_row() {
        let path = "/Users/jr/Desktop/projects/colon35/release-candidate-20261016/frontend/docs/superpowers/specs/2026-10-23-claude-per-ho-design.md";
        let text = format!("See `{path}`. ");
        for index in [5, 35, 70, 105, path.len()] {
            assert_eq!(
                ascii(32, &text, index),
                Some(PathCandidate {
                    word: format!("`{path}`."),
                    start: 5,
                    end: path.len() + 4,
                }),
                "the entire visible path is one target at cell {index}"
            );
        }
    }

    #[test]
    fn terminal_path_helper_keeps_balanced_filename_parentheses_and_line_suffix() {
        let path = "/tmp/notes(final)[v2].md:12:3";
        let text = format!("({path}),");
        assert_eq!(ascii(80, &text, 12).unwrap().word, text);
        assert!(ascii(80, &text, 0).is_none());
        assert!(ascii(80, &text, text.len() - 1).is_none());
    }

    #[test]
    fn terminal_path_helper_skips_korean_tail_and_preserves_sparse_graphemes() {
        let cells = [
            PathCell::Glyph('/', None),
            PathCell::Glyph('한', None),
            PathCell::Continuation,
            PathCell::Glyph('글', Some("글\u{301}")),
            PathCell::Continuation,
            PathCell::Glyph('.', None),
            PathCell::Glyph('m', None),
            PathCell::Glyph('d', None),
            PathCell::Glyph(' ', None),
        ];
        for index in 0..8 {
            assert_eq!(
                candidate_at(10, cells.len(), index, |i| cells[i])
                    .unwrap()
                    .word,
                "/한글\u{301}.md"
            );
        }
    }

    #[test]
    fn terminal_path_helper_skips_only_a_row_end_wide_wrap_filler() {
        let cells = [
            PathCell::Glyph('/', None),
            PathCell::Glyph('a', None),
            PathCell::Glyph('b', None),
            PathCell::WrapFiller,
            PathCell::Glyph('한', None),
            PathCell::Continuation,
            PathCell::Glyph('.', None),
            PathCell::Glyph('m', None),
            PathCell::Glyph('d', None),
        ];
        assert_eq!(
            candidate_at(4, cells.len(), 4, |i| cells[i]).unwrap().word,
            "/ab한.md"
        );
        assert!(candidate_at(4, cells.len(), 3, |i| cells[i]).is_none());
    }

    #[test]
    fn terminal_path_helper_rejects_urls_prose_controls_and_outside_cells() {
        for text in ["https://example.com/a.md", "file:///tmp/a.md", "plain.md"] {
            assert!(ascii(80, text, 6).is_none(), "{text}");
        }
        assert!(ascii(80, "/tmp/a.md text", 10).is_none());
        assert!(ascii(80, "/tmp/a.md", 20).is_none());
        assert!(ascii(0, "/tmp/a.md", 1).is_none());
        assert!(ascii(80, "/tmp/\u{1b}oops.md", 7).is_none());
    }

    #[test]
    fn terminal_path_helper_does_not_join_across_padding_or_invent_missing_prefixes() {
        assert_eq!(
            ascii(12, "/tmp/a.md   next/path.md ", 5).unwrap().word,
            "/tmp/a.md"
        );
        assert!(ascii(12, "continuation/name.md", 4).is_none());
    }

    #[test]
    fn terminal_path_helper_bounds_scan_and_retained_bytes() {
        let text = format!("/{}.md", "x".repeat(MAX_PATH_BYTES));
        assert!(ascii(80, &text, 12).is_none());
    }

    #[test]
    fn terminal_path_helper_preserves_raw_lookup_for_punctuated_file_names() {
        for text in [
            "/tmp/README",
            "/tmp/README)",
            "/tmp/README,",
            "(/tmp/README)",
        ] {
            assert_eq!(
                ascii(80, text, 6).unwrap().word,
                text,
                "the host must distinguish the raw filename before trying prose punctuation fallbacks"
            );
        }
    }
}
