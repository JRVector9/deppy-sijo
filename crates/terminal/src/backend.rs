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

/// 터미널 텍스트 검색 매치 하나 (T3). 좌표는 화면 최하단 기준으로 매겨 UI가
/// display_offset(스크롤) 좌표계와 직접 대응시켜 스크롤·하이라이트에 바로 쓴다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrollbackMatch {
    /// 화면 최하단(0)부터 위(과거)로 센 라인 번호. 뷰포트 행 = scroll_offset + rows-1 - 이 값.
    pub line_from_bottom: u32,
    /// 매치가 차지하는 grid 열 시작(포함).
    pub col_start: u16,
    /// 매치가 차지하는 grid 열 끝(제외). wide char의 spacer 열까지 포함한다.
    pub col_end: u16,
}

/// scrollback+화면 검색 결과 (T3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrollbackSearchResult {
    /// 화면 최하단에 가까운 순서(line_from_bottom 오름차순, 같은 라인은 좌→우).
    pub matches: Vec<ScrollbackMatch>,
    /// 검색 시점의 전체 라인 수(history + 화면). UI의 스크롤 목표 클램프에 쓴다.
    pub total_lines: u32,
    /// 매치 수 상한(max_matches)에 도달해 결과가 잘렸는지.
    pub capped: bool,
}

impl ScrollbackSearchResult {
    pub fn empty() -> Self {
        Self {
            matches: Vec::new(),
            total_lines: 0,
            capped: false,
        }
    }
}

/// char 하나를 소문자로 폴딩한다(대소문자 무시 검색용). 다중 char로 분해되는 드문
/// 경우(İ 등)는 첫 char만 취해 grid 열과의 1:1 대응을 유지한다 — 열 매핑 안정성 우선.
pub fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// 한 라인(이미 [`fold_char`]로 폴딩된 char 슬라이스)에서 needle의 겹치지 않는 모든
/// 부분 문자열 매치를 좌→우 순서로 찾아 (시작, 끝) char 인덱스를 돌려준다.
/// needle도 폴딩되어 있다고 가정한다.
pub fn substring_matches(haystack: &[char], needle: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    let last = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last {
        if haystack[i..i + needle.len()] == *needle {
            out.push((i, i + needle.len()));
            i += needle.len(); // 겹치지 않는 매치만
        } else {
            i += 1;
        }
    }
    out
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

    /// 스크롤백에서 맨 아래(라이브 화면)로 복귀한다. 기본 구현은 큰 음수 delta
    /// (양수 = 과거 방향 관례의 역) — 정확한 백엔드는 재정의한다(alacritty Scroll::Bottom).
    fn scroll_to_bottom(&mut self) {
        self.scroll(i32::MIN / 2);
    }

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

    /// 메모리 압박 하에서 스크롤백을 클래스 예산 **아래로** 강제 축소한다 — 가장 오래된
    /// 히스토리를 `max_lines`까지 드롭하고 남은 것을 전부 압축한다. 전역 예산이 exited
    /// 아카이브만으로 안 맞을 때 live 세션을 예산 안으로 넣는 경로. 이미 `max_lines`
    /// 이하면 `None`. 기본 구현은 no-op(미지원 백엔드).
    fn trim_scrollback(&mut self, _max_lines: usize) -> Option<TerminalCacheEvent> {
        None
    }

    fn cache_class(&self) -> TerminalCacheClass;

    fn cache_footprint(&self) -> TerminalCacheFootprint;

    fn bracketed_paste(&self) -> bool;

    /// 현재 화면(스크롤 무시, 실제 grid)의 텍스트 — status detector용 경량 조회.
    /// TerminalViewportSnapshot을 만들지 않는다 (설계문서 PR-12: hidden session 규칙).
    fn screen_text(&self) -> String;

    /// scrollback+화면 전체를 스타일 보존 ANSI 바이트로 직렬화한다 —
    /// exited 백엔드 압축 아카이브용 (§14.3 확장). 새 백엔드에 feed하면 복원된다.
    /// 미지원 백엔드는 None (아카이브 대신 기존 drop 동작).
    fn serialize_scrollback(&self) -> Option<Vec<u8>> {
        None
    }

    /// scrollback+화면 전체에서 query를 부분 문자열로(대소문자 무시) 찾는다 (T3).
    /// scrollback에는 이미 라인 수 cap이 있어 탐색 비용은 유계다. 미지원 백엔드는 빈 결과.
    fn search_scrollback(&self, query: &str, max_matches: usize) -> ScrollbackSearchResult {
        let _ = (query, max_matches);
        ScrollbackSearchResult::empty()
    }

    /// 커서가 속한 논리 라인에서 `back` **논리 라인**(soft wrap 행 병합) 위의 텍스트 —
    /// 마지막 출력 추출용 (셸 통합 2단계). LF로 생긴 라인만 세므로 세션의 LF 카운터
    /// 좌표와 일치한다. 셀 해석(wide spacer 스킵, conceal→공백)은 search_scrollback과
    /// 동일하고 trailing 공백은 라인 끝에서만 잘라낸다. 화면이 아직 안 찬 프레시 셸에서도
    /// 정확하도록 grid 최하단이 아니라 **커서**가 기준이다.
    /// 범위 밖(스크롤백 트림)이나 미지원 백엔드는 None.
    fn logical_line_back_from_cursor(&self, back: usize) -> Option<String> {
        let _ = back;
        None
    }
}

#[cfg(test)]
mod search_tests {
    use super::{fold_char, substring_matches};

    fn fold(s: &str) -> Vec<char> {
        s.chars().map(fold_char).collect()
    }

    #[test]
    fn 대소문자_무시_부분_문자열_매치() {
        let hay = fold("Error: File Not Found");
        assert_eq!(substring_matches(&hay, &fold("error")), vec![(0, 5)]);
        assert_eq!(substring_matches(&hay, &fold("NOT")), vec![(12, 15)]);
    }

    #[test]
    fn 한_라인_다중_매치는_좌에서_우_순서() {
        let hay = fold("foo bar foo baz foo");
        assert_eq!(
            substring_matches(&hay, &fold("foo")),
            vec![(0, 3), (8, 11), (16, 19)]
        );
    }

    #[test]
    fn 겹치는_매치는_비겹침으로만_센다() {
        let hay = fold("aaaa");
        // "aa"는 (0,2),(2,4)만 — (1,3)은 겹쳐서 제외
        assert_eq!(substring_matches(&hay, &fold("aa")), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn 빈_쿼리나_긴_쿼리는_매치_없음() {
        let hay = fold("abc");
        assert!(substring_matches(&hay, &fold("")).is_empty());
        assert!(substring_matches(&hay, &fold("abcd")).is_empty());
    }

    #[test]
    fn 유니코드_대소문자_폴딩() {
        let hay = fold("Grüße HÄLLO");
        assert_eq!(substring_matches(&hay, &fold("grüße")), vec![(0, 5)]);
        assert_eq!(substring_matches(&hay, &fold("hällo")), vec![(6, 11)]);
    }
}
