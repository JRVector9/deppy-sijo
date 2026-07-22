//! LayoutNode ↔ `mux_layouts.layout_json` 매핑 (설계문서 §11.4).
//! mux 타입에는 serde를 두지 않으므로 (10장 의존 방향: mux는 영속 무의존)
//! 여기서 거울 타입으로 수동 매핑한다. MuxPaneId는 String UUID — 무변환.

use std::collections::HashSet;

use anyhow::{Context, bail};
use deppy_core::MuxPaneId;
use mux::{LayoutNode, SplitDirection};
use serde::{Deserialize, Serialize};

/// LayoutNode의 JSON 거울 타입. 예:
/// `{"type":"split","direction":"horizontal","ratio":0.5,"first":{...},"second":{...}}`
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum NodeJson {
    Pane {
        pane_id: String,
    },
    Split {
        direction: DirectionJson,
        ratio: f32,
        first: Box<NodeJson>,
        second: Box<NodeJson>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum DirectionJson {
    Horizontal,
    Vertical,
}

pub fn layout_to_json(layout: &LayoutNode) -> anyhow::Result<String> {
    let node = to_node(layout)?;
    serde_json::to_string(&node).context("layout_json 직렬화 실패")
}

pub fn layout_from_json(json: &str) -> anyhow::Result<LayoutNode> {
    let node: NodeJson = serde_json::from_str(json).context("layout_json 파싱 실패")?;
    from_node(node)
}

/// Untrusted restore input의 재귀·pane fan-out을 JSON 역직렬화 전에 제한한다.
/// 오류 문구는 영속 데이터나 serde 세부 내용을 포함하지 않는 고정 문자열이다.
pub(crate) fn layout_from_json_bounded(
    json: &str,
    max_depth: usize,
    max_panes: usize,
    max_id_bytes: usize,
) -> anyhow::Result<LayoutNode> {
    if max_depth == 0 || max_panes == 0 || !json_nesting_within(json, max_depth) {
        bail!("workspace_restore_layout_invalid");
    }
    let node: NodeJson = serde_json::from_str(json)
        .map_err(|_| anyhow::anyhow!("workspace_restore_layout_invalid"))?;
    let mut pane_count = 0usize;
    let mut pane_ids = HashSet::new();
    from_node_bounded(
        node,
        1,
        max_depth,
        max_panes,
        max_id_bytes,
        &mut pane_count,
        &mut pane_ids,
    )
}

/// Layout JSON은 object만 중첩한다. 문자열 내부 delimiter와 escape를 제외하고
/// 구조 깊이를 먼저 세어 serde_json이 제한보다 깊은 입력을 재귀 처리하지 않게 한다.
fn json_nesting_within(json: &str, max_depth: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in json.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                let Some(next) = depth.checked_add(1) else {
                    return false;
                };
                depth = next;
                if depth > max_depth {
                    return false;
                }
            }
            b'}' | b']' => {
                let Some(next) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next;
            }
            _ => {}
        }
    }
    !in_string && !escaped && depth == 0
}

#[allow(clippy::too_many_arguments)]
fn from_node_bounded(
    node: NodeJson,
    depth: usize,
    max_depth: usize,
    max_panes: usize,
    max_id_bytes: usize,
    pane_count: &mut usize,
    pane_ids: &mut HashSet<String>,
) -> anyhow::Result<LayoutNode> {
    if depth > max_depth {
        bail!("workspace_restore_layout_invalid");
    }
    match node {
        NodeJson::Pane { pane_id } => {
            if pane_id.is_empty()
                || pane_id.len() > max_id_bytes
                || !pane_ids.insert(pane_id.clone())
            {
                bail!("workspace_restore_layout_invalid");
            }
            *pane_count = pane_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("workspace_restore_layout_invalid"))?;
            if *pane_count > max_panes {
                bail!("workspace_restore_layout_invalid");
            }
            Ok(LayoutNode::Pane(MuxPaneId(pane_id)))
        }
        NodeJson::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                bail!("workspace_restore_layout_invalid");
            }
            let next_depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("workspace_restore_layout_invalid"))?;
            Ok(LayoutNode::Split {
                direction: match direction {
                    DirectionJson::Horizontal => SplitDirection::Horizontal,
                    DirectionJson::Vertical => SplitDirection::Vertical,
                },
                ratio,
                first: Box::new(from_node_bounded(
                    *first,
                    next_depth,
                    max_depth,
                    max_panes,
                    max_id_bytes,
                    pane_count,
                    pane_ids,
                )?),
                second: Box::new(from_node_bounded(
                    *second,
                    next_depth,
                    max_depth,
                    max_panes,
                    max_id_bytes,
                    pane_count,
                    pane_ids,
                )?),
            })
        }
    }
}

fn to_node(layout: &LayoutNode) -> anyhow::Result<NodeJson> {
    Ok(match layout {
        LayoutNode::Pane(id) => NodeJson::Pane {
            pane_id: id.0.clone(),
        },
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            check_ratio(*ratio)?;
            NodeJson::Split {
                direction: match direction {
                    SplitDirection::Horizontal => DirectionJson::Horizontal,
                    SplitDirection::Vertical => DirectionJson::Vertical,
                },
                ratio: *ratio,
                first: Box::new(to_node(first)?),
                second: Box::new(to_node(second)?),
            }
        }
    })
}

fn from_node(node: NodeJson) -> anyhow::Result<LayoutNode> {
    Ok(match node {
        NodeJson::Pane { pane_id } => LayoutNode::Pane(MuxPaneId(pane_id)),
        NodeJson::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            check_ratio(ratio)?;
            LayoutNode::Split {
                direction: match direction {
                    DirectionJson::Horizontal => SplitDirection::Horizontal,
                    DirectionJson::Vertical => SplitDirection::Vertical,
                },
                ratio,
                first: Box::new(from_node(*first)?),
                second: Box::new(from_node(*second)?),
            }
        }
    })
}

/// NaN/무한대나 범위 밖 ratio가 저장·복원되는 것을 양방향에서 막는다.
fn check_ratio(ratio: f32) -> anyhow::Result<()> {
    if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
        bail!("split ratio 범위 밖: {ratio}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 중첩_split_round_trip() {
        let (a, b, c) = (MuxPaneId::new(), MuxPaneId::new(), MuxPaneId::new());
        let mut layout = LayoutNode::Pane(a.clone());
        layout.split_pane(&a, SplitDirection::Horizontal, b.clone());
        layout.split_pane(&b, SplitDirection::Vertical, c.clone());

        let json = layout_to_json(&layout).unwrap();
        let restored = layout_from_json(&json).unwrap();
        assert_eq!(restored, layout);
        // DFS pane 순서까지 보존
        assert_eq!(restored.panes(), vec![a, b, c]);
    }

    #[test]
    fn 단일_pane_round_trip() {
        let layout = LayoutNode::Pane(MuxPaneId::new());
        let restored = layout_from_json(&layout_to_json(&layout).unwrap()).unwrap();
        assert_eq!(restored, layout);
    }

    #[test]
    fn 손상_json_거부() {
        assert!(layout_from_json("not json").is_err());
        assert!(layout_from_json(r#"{"type":"circle","radius":1}"#).is_err());
        // ratio 범위 밖
        let bad = r#"{"type":"split","direction":"horizontal","ratio":1.5,
            "first":{"type":"pane","pane_id":"a"},"second":{"type":"pane","pane_id":"b"}}"#;
        assert!(layout_from_json(bad).is_err());
    }

    #[test]
    fn 비정상_ratio_저장_거부() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: f32::NAN,
            first: Box::new(LayoutNode::Pane(MuxPaneId::new())),
            second: Box::new(LayoutNode::Pane(MuxPaneId::new())),
        };
        assert!(layout_to_json(&layout).is_err());
    }

    fn nested_layout_json(depth: usize) -> String {
        let mut json = r#"{"type":"pane","pane_id":"pane-0"}"#.to_owned();
        for index in 1..depth {
            json = format!(
                r#"{{"type":"split","direction":"horizontal","ratio":0.5,"first":{json},"second":{{"type":"pane","pane_id":"pane-{index}"}}}}"#
            );
        }
        json
    }

    #[test]
    fn bounded_layout은_깊이_exact와_plus_one을_구분한다() {
        let exact = nested_layout_json(64);
        assert!(layout_from_json_bounded(&exact, 64, 256, 1024).is_ok());

        let too_deep = nested_layout_json(65);
        assert_eq!(
            layout_from_json_bounded(&too_deep, 64, 256, 1024)
                .unwrap_err()
                .to_string(),
            "workspace_restore_layout_invalid"
        );
    }

    #[test]
    fn bounded_layout은_중복_pane_id를_거부한다() {
        let duplicate = r#"{"type":"split","direction":"horizontal","ratio":0.5,
            "first":{"type":"pane","pane_id":"same"},
            "second":{"type":"pane","pane_id":"same"}}"#;
        assert!(layout_from_json_bounded(duplicate, 64, 256, 1024).is_err());
    }
}
