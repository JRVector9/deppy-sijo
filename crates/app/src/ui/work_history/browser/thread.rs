//! Read-only thread projection; durable turn identities are never rewritten.
use super::*;

fn owner<'a>(row: &WorkHistoryRow<'a>) -> (&'a str, &'a str, &'a str, &'a str) {
    (
        row.workspace_id,
        row.pane_id,
        row.kind,
        row.agent_session_id,
    )
}

fn order<'a>(row: &WorkHistoryRow<'a>) -> (i64, u64, &'a str) {
    (started_at(row), row.source_offset, row.turn_key)
}

pub(super) fn groups(rows: &[WorkHistoryRow<'_>], indices: &[usize]) -> Vec<Vec<usize>> {
    let mut positions = std::collections::HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &i in indices {
        let position = *positions.entry(owner(&rows[i])).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[position].push(i);
    }
    groups
}

pub(super) fn continuation(text: &str) -> bool {
    let text = text.trim().trim_end_matches(['.', '!', '?', '。']);
    let lower = text.to_lowercase();
    matches!(
        lower.as_str(),
        "계속" | "한글로" | "응" | "응 그렇게 해" | "continue" | "go on" | "proceed"
    ) || (text.chars().count() < 32
        && ["이어서", "계속 ", "남은것", "남은 것"]
            .iter()
            .any(|prefix| lower.starts_with(prefix)))
}

pub(super) fn title<'a>(rows: &[WorkHistoryRow<'a>], anchor: &WorkHistoryRow<'a>) -> &'a str {
    rows.iter()
        .filter(|row| owner(row) == owner(anchor) && order(row) <= order(anchor))
        .filter(|row| {
            row.instruction.trim().chars().count() >= 20 && !continuation(row.instruction)
        })
        .max_by_key(|row| order(row))
        .map(|row| collapsed_summary_line(row.instruction))
        .unwrap_or_else(|| collapsed_summary_line(anchor.instruction))
}

pub(super) fn previous(rows: &[WorkHistoryRow<'_>], anchor: &WorkHistoryRow<'_>) -> Vec<usize> {
    let mut indices: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| owner(row) == owner(anchor) && order(row) < order(anchor))
        .map(|(index, _)| index)
        .collect();
    indices.sort_by_key(|&index| std::cmp::Reverse(order(&rows[index])));
    indices.truncate(3);
    indices
}

/// Sparse continuation records can omit metadata. Borrow only saved context
/// from the same owner at/before this turn; identities and live proof stay exact.
pub(super) fn context<'a>(
    rows: &[WorkHistoryRow<'a>],
    anchor: &WorkHistoryRow<'a>,
) -> WorkHistoryRow<'a> {
    let mut result = *anchor;
    let mut past: Vec<_> = rows
        .iter()
        .filter(|row| owner(row) == owner(anchor) && order(row) <= order(anchor))
        .collect();
    // Include anchor last for callers that supply a sparse row not in rows.
    past.push(anchor);
    past.sort_by_key(|row| order(row));
    let present = |value: Option<&'a str>| value.filter(|text| !text.trim().is_empty());
    let (mut cwd, mut branch, mut model, mut effort) = (None, None, None, None);
    for row in past {
        if let Some(path) = present(row.cwd) {
            if cwd != Some(path) {
                branch = None;
            }
            cwd = Some(path);
        }
        if let Some(name) = present(row.model) {
            if model != Some(name) {
                effort = None;
            }
            model = Some(name);
        }
        if let Some(value) = present(row.branch) {
            branch = Some(value);
        }
        if let Some(value) = present(row.effort) {
            effort = Some(value);
        }
    }
    result.cwd = cwd;
    result.branch = branch;
    result.model = model;
    result.effort = effort;
    result
}

pub(super) fn project<'a>(row: &WorkHistoryRow<'a>) -> Option<&'a str> {
    row.cwd
        .filter(|cwd| !cwd.trim().is_empty())
        .and_then(|cwd| cwd.trim_end_matches('/').rsplit('/').next())
        .filter(|name| !name.is_empty())
}

/// A bounded plain preview of saved output, not a generated/translated result.
pub(super) fn preview(summary: &str) -> String {
    let mut fence = None;
    let line = summary
        .lines()
        .map(str::trim)
        .find(|line| {
            if line.starts_with("```") || line.starts_with("~~~") {
                let marker = &line[..3];
                if fence == Some(marker) {
                    fence = None;
                } else if fence.is_none() {
                    fence = Some(marker);
                }
                return false;
            }
            fence.is_none() && !line.is_empty() && !line.starts_with('#')
        })
        .unwrap_or_default();
    let line = line.trim_start_matches(['-', '*', '>', ' ']);
    let mut result = String::new();
    let mut link_target = false;
    let mut after_bracket = false;
    let mut count = 0;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if link_target {
            if ch == ')' {
                link_target = false;
            }
            continue;
        }
        if ch == '(' && after_bracket {
            link_target = true;
            after_bracket = false;
            continue;
        }
        after_bracket = ch == ']';
        if matches!(ch, '*' | '`' | '[' | ']') {
            continue;
        }
        if count == 160 {
            result.push('…');
            break;
        }
        result.push(ch);
        count += 1;
        if matches!(ch, '.' | '!' | '?' | '。')
            && chars.peek().is_none_or(|next| next.is_whitespace())
        {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(at: i64, prompt: &'static str) -> WorkHistoryRow<'static> {
        WorkHistoryRow {
            pane_id: "a",
            pane_title: None,
            cwd: Some("/projects/nomorevibe/"),
            occurred_at: Some(at),
            current_state: None,
            workspace_id: "w",
            kind: "claude",
            agent_session_id: "native",
            turn_key: prompt,
            source_offset: at as u64,
            instruction: prompt,
            agent_summary: None,
            messages_json: None,
            model: None,
            effort: None,
            branch: None,
            git_change_count: None,
            state: WorkHistoryState::Completed,
            updated_at: at,
        }
    }

    #[test]
    fn thread_projection_preserves_owners_and_past_context() {
        let old = row(
            1,
            "관리자 화면에서 마지막 확인 기간을 설정할 수 있게 구현해",
        );
        let short = row(2, "남은것 진행하자.");
        let future = row(
            3,
            "새로운 모바일 터미널의 화면 크기를 조절하는 기능을 구현해",
        );
        let mut other = row(1, "다른 세션의 작업은 이 세션과 서로 독립적으로 유지해");
        other.pane_id = "b";
        let mut workspace = other;
        workspace.workspace_id = "other";
        let mut provider = other;
        provider.kind = "codex";
        let rows = [old, short, future, other, workspace, provider];
        assert_eq!(title(&rows, &short), old.instruction);
        assert_eq!(previous(&rows, &short), [0]);
        assert_eq!(
            groups(&rows, &[2, 1, 0, 3, 4, 5]),
            [vec![2, 1, 0], vec![3], vec![4], vec![5]]
        );
        assert_eq!(project(&old), Some("nomorevibe"));
    }

    #[test]
    fn thread_context_only_inherits_past_owner_and_matching_project() {
        let mut old = row(1, "original");
        old.branch = Some("main");
        old.model = Some("model-a");
        old.effort = Some("high");
        let mut short = row(2, "계속");
        short.cwd = None;
        let mut future = row(3, "future");
        future.cwd = Some("/new/project");
        future.branch = Some("future-branch");
        let mut other = row(2, "other");
        other.agent_session_id = "different";
        other.branch = Some("other-branch");
        let rows = [old, short, future, other];
        let filled = context(&rows, &short);
        assert_eq!(filled.cwd, old.cwd);
        assert_eq!(filled.branch, old.branch);
        assert_eq!(filled.model, old.model);
        assert_eq!(filled.effort, old.effort);
        assert_eq!(
            WorkTurnIdentity::from(&filled),
            WorkTurnIdentity::from(&short)
        );
        assert_eq!(filled.current_state, short.current_state);
        let mut new_project = short;
        new_project.cwd = Some("/another/project");
        new_project.model = Some("model-b");
        let filled = context(&rows, &new_project);
        assert_eq!(filled.branch, None);
        assert_eq!(filled.effort, None);
        // Sparse metadata from an earlier project cannot cross a path change.
        let mut sparse_branch = row(2, "branch-only");
        sparse_branch.cwd = None;
        sparse_branch.branch = Some("older-project-branch");
        let mut moved = row(3, "moved");
        moved.cwd = Some("/moved/project");
        let mut continuation = row(4, "이어서");
        continuation.cwd = None;
        let filled = context(&[old, sparse_branch, moved, continuation], &continuation);
        assert_eq!(filled.cwd, moved.cwd);
        assert_eq!(filled.branch, None);
    }

    #[test]
    fn thread_preview_is_bounded_and_uses_saved_text() {
        assert_eq!(
            preview("## Result\n**Fixed** [PR #34](https://example.test/a). More details."),
            "Fixed PR #34."
        );
        assert_eq!(
            preview("```rust\nlet source = true;\n```\n검증 완료했습니다. 다음 작업입니다."),
            "검증 완료했습니다."
        );
        assert!(preview(&"한".repeat(1000)).chars().count() <= 161);
        assert_eq!(preview("\n"), "");
        assert_eq!(preview("```rust\nlet source = true;"), "");
    }
}
