use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::TerminalViewportSnapshot;

pub const TERMINAL_GLOBAL_CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCacheClass {
    Visible,
    Hidden,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCacheBudget {
    pub max_scrollback_lines: usize,
    pub max_bytes: usize,
}

impl TerminalCacheBudget {
    pub const VISIBLE: Self = Self {
        max_scrollback_lines: 10_000,
        max_bytes: 16 * 1024 * 1024,
    };
    pub const HIDDEN: Self = Self {
        max_scrollback_lines: 1_000,
        max_bytes: 2 * 1024 * 1024,
    };
    pub const EXITED: Self = Self {
        max_scrollback_lines: 1_000,
        max_bytes: 2 * 1024 * 1024,
    };

    pub fn for_class(class: TerminalCacheClass) -> Self {
        match class {
            TerminalCacheClass::Visible => Self::VISIBLE,
            TerminalCacheClass::Hidden => Self::HIDDEN,
            TerminalCacheClass::Exited => Self::EXITED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCacheFootprint {
    pub class: TerminalCacheClass,
    pub scrollback_limit_lines: usize,
    pub history_lines: usize,
    pub screen_lines: usize,
    pub columns: usize,
    pub bytes_per_line: usize,
    pub estimated_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCacheEventKind {
    ScrollbackLimitApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCacheEvent {
    pub kind: TerminalCacheEventKind,
    pub class: TerminalCacheClass,
    pub budget: TerminalCacheBudget,
    pub before: TerminalCacheFootprint,
    pub after: TerminalCacheFootprint,
}

impl TerminalCacheEvent {
    pub fn dropped_history_lines(self) -> usize {
        self.before
            .history_lines
            .saturating_sub(self.after.history_lines)
    }

    pub fn freed_estimated_bytes(self) -> usize {
        self.before
            .estimated_bytes
            .saturating_sub(self.after.estimated_bytes)
    }
}

/// 설계문서 4.2 TerminalRenderModel.
pub enum TerminalRenderModel {
    CellGrid,
    /// LibGhosttyBackend Mode B용 (v1.x) — 현재 구현체 없음
    ExternalSurface,
}

/// ExternalSurface 렌더 모델의 surface 핸들 (설계문서 4.2).
/// v0에서는 사용처가 없다 — LibGhosttyBackend Mode B에서 구체화.
pub struct TerminalExternalSurfaceHandle;

/// 설계문서 4.2 TerminalBackend trait.
/// `bracketed_paste`는 설계 trait에 없지만 PR-05 완료 기준(bracketed paste)이
/// 입력 경로에서 모드 조회를 요구해 추가했다.
pub trait TerminalBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet>;
    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()>;
    fn render_model(&self) -> TerminalRenderModel;

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot>;
    fn external_surface(&self) -> Option<TerminalExternalSurfaceHandle>;

    fn scroll(&mut self, delta: i32);
    fn reset(&mut self);

    /// 가시성에 따라 scrollback 상한을 조정한다 (설계문서 §14.3).
    /// 실제 제한은 [`TerminalCacheBudget`]의 line/byte budget을 같이 적용한다.
    /// **전이 시에만** 호출할 것 — 내부적으로 title 이벤트를 유발할 수 있다.
    fn set_visible(&mut self, visible: bool) -> Option<TerminalCacheEvent> {
        let class = if visible {
            TerminalCacheClass::Visible
        } else {
            TerminalCacheClass::Hidden
        };
        self.set_cache_class(class)
    }

    fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent>;

    fn cache_class(&self) -> TerminalCacheClass;

    fn cache_footprint(&self) -> TerminalCacheFootprint;

    fn bracketed_paste(&self) -> bool;

    /// 현재 화면(스크롤 무시, 실제 grid)의 텍스트 — status detector용 경량 조회.
    /// TerminalViewportSnapshot을 만들지 않는다 (설계문서 PR-12: hidden session 규칙).
    fn screen_text(&self) -> String;
}
