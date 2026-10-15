use terminal::{AlacrittyBackend, TerminalBackend};
#[derive(Clone, Copy, PartialEq, Eq)] enum AgentPromptKind { Claude, Codex, Other }
#[derive(Debug, PartialEq, Eq)]
enum PromptRow {
    Empty,
    Draft,
    /// Native input hint; unlike an arbitrary unknown row, no user text is present.
    Placeholder,
    Unknown,
}

fn is_native_editor_border(cells: &[terminal::TerminalCell], prompt_fg: [u8; 3]) -> bool {
    let mut strokes = 0;
    let mut border_fg = None;
    for cell in cells {
        if cell.c.is_whitespace() {
            continue;
        }
        if !matches!(cell.c, '─' | '━' | '═') || cell.fg == prompt_fg {
            return false;
        }
        if border_fg.is_some_and(|fg| fg != cell.fg) {
            return false;
        }
        border_fg = Some(cell.fg);
        strokes += 1;
    }
    strokes >= cells.len().saturating_mul(3) / 4
}

fn prompt_row(
    snapshot: &terminal::TerminalViewportSnapshot,
    provider: AgentPromptKind,
) -> PromptRow {
    if snapshot.scroll_offset != 0 || !snapshot.cursor.visible || snapshot.cols == 0 {
        return PromptRow::Unknown;
    }
    let marker = match provider {
        AgentPromptKind::Claude => '❯',
        AgentPromptKind::Codex => '›',
        AgentPromptKind::Other => return PromptRow::Unknown,
    };
    let start = usize::from(snapshot.cursor.row) * usize::from(snapshot.cols);
    let Some(cells) = snapshot
        .visible_cells
        .get(start..start + usize::from(snapshot.cols))
    else {
        return PromptRow::Unknown;
    };
    let Some(index) = cells.iter().position(|cell| !cell.c.is_whitespace()) else {
        return PromptRow::Unknown;
    };
    if cells[index].c != marker {
        return PromptRow::Unknown;
    }
    let tail = &cells[index + 1..];
    if tail.iter().all(|cell| cell.c.is_whitespace()) {
        // Claude's multiline editor keeps its ❯ marker on the first row even
        // when Ctrl+U clears that row and later draft lines remain. The bottom
        // rule separates those lines from status/footer content.
        if provider == AgentPromptKind::Claude {
            for row in usize::from(snapshot.cursor.row) + 1..usize::from(snapshot.rows) {
                let start = row * usize::from(snapshot.cols);
                let Some(next) = snapshot
                    .visible_cells
                    .get(start..start + usize::from(snapshot.cols))
                else {
                    return PromptRow::Unknown;
                };
                if is_native_editor_border(next, cells[index].fg) {
                    break;
                }
                if next.iter().any(|cell| !cell.c.is_whitespace()) {
                    return PromptRow::Draft;
                }
            }
        }
        return PromptRow::Empty;
    }
    // A dim suggestion before the cursor has entered it is not a verified empty prompt.
    // Keep that positive native-input evidence separate from an arbitrary unknown row.
    // Explicit visible-session sends can proceed; automation must still fail closed.
    let first = index
        + 1
        + tail
            .iter()
            .position(|cell| !cell.c.is_whitespace())
            .unwrap();
    if usize::from(snapshot.cursor.col) <= first
        && cells[first..]
            .iter()
            .filter(|cell| !cell.c.is_whitespace())
            .all(|cell| cell.attrs().contains(terminal::CellAttrs::DIM))
    {
        return PromptRow::Placeholder;
    }
    PromptRow::Draft
}

fn choice_dialog(snapshot: &terminal::TerminalViewportSnapshot) -> bool {
    // Bounded current viewport only, never scrollback or transcript polling.
    let first_row = usize::from(snapshot.cursor.row).saturating_sub(8);
    let last_row = (usize::from(snapshot.cursor.row) + 3).min(usize::from(snapshot.rows));
    let first = first_row * usize::from(snapshot.cols);
    let last = last_row * usize::from(snapshot.cols);
    let Some(cells) = snapshot.visible_cells.get(first..last) else {
        return true;
    };
    let text: String = cells.iter().map(|cell| cell.c).collect();
    let text = text.to_ascii_lowercase();
    text.contains("enter to select")
        || text.contains("press enter to confirm")
        || text.contains("esc to cancel")
        || text.contains("escape to cancel")
}


fn main() {
    let mut backend = AlacrittyBackend::new(80, 24, 100);
    backend.feed("› \x1b[2m한글 힌트\x1b[0m\x1b[1;4H".as_bytes()).unwrap();
    let before = backend.viewport_snapshot().unwrap();
    let after = backend.input_snapshot().unwrap();
    let old_row = prompt_row(&before, AgentPromptKind::Codex);
    let new_row = prompt_row(&after, AgentPromptKind::Codex);
    println!("viewport_col={} input_col={} viewport_row={:?} input_row={:?}", before.cursor.col, after.cursor.col, old_row, new_row);
    assert_eq!(old_row, PromptRow::Placeholder);
    assert_eq!(new_row, old_row, "wide spacer must preserve native placeholder readiness");
}
