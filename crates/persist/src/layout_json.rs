//! LayoutNode ↔ `mux_layouts.layout_json` 매핑 (설계문서 §11.4).
//! mux 타입에는 serde를 두지 않으므로 (10장 의존 방향: mux는 영속 무의존)
//! 여기서 거울 타입으로 수동 매핑한다. MuxPaneId는 String UUID — 무변환.

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
}
