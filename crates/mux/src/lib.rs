//! Mux Runtime 객체 모델 (설계문서 5장 / 9장).
//! layout의 source of truth — pane과 session은 분리된다 (PR-07 완료 기준).
//! UI 의존 없음 (10장). 영속화는 PR-14, UI 소비(tabs/panes)는 PR-10.

mod focus;
mod layout_tree;
mod pane;
mod snapshot;
mod tab;
mod window;
mod workspace;

pub use focus::FocusManager;
pub use layout_tree::{LayoutNode, RemovePane, SplitDirection};
pub use pane::{MuxPane, PaneKind};
pub use snapshot::{MuxSnapshot, PaneSnapshot, TabSnapshot};
pub use tab::{ClosePane, MuxTab};
pub use window::MuxWindow;
pub use workspace::MuxWorkspace;
