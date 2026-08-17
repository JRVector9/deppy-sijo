//! 보조 본문 검색 — 상태·매칭·검색 바 (2026-08-18 스펙:
//! docs/superpowers/specs/2026-08-18-aux-search-design.md, 계획:
//! docs/superpowers/plans/2026-08-18-aux-search.md).
//!
//! leaf라 IO를 하지 않고 뷰를 직접 바꾸지 않는다 — `search_bar`는 intent만 반환하고,
//! 실제 좌측 목록 필터·우측 강조·이동은 소비자(work_history·git_panel·transcript_viewer·
//! diff_viewer)가 이 모듈의 순수 함수(`find_matches`/`contains_match`/`highlighted_job`)로
//! 직접 그린다. `AuxSearchState`는 App이 소유한다(탭 전환·워크스페이스 전환 시 `reset()`).

/// 본문당 세는 일치 상한 — 거대 diff에서 매 프레임 전수 스캔하지 않는다. 넘으면
/// [`Matches::truncated`]만 세우고 더 찾지 않는다(스펙 "매칭 규칙").
pub const MAX_AUX_MATCHES: usize = 500;

/// 보조 검색 상태. App이 소유하고(탭/워크스페이스 전환 시 [`AuxSearchState::reset`]),
/// leaf들은 이 값을 읽기만 한다.
#[derive(Default, Clone)]
pub struct AuxSearchState {
    pub query: String,
    pub open: bool,
    /// 0-based. 일치가 없으면 무시된다(표시 시점에 클램프한다).
    pub active: usize,
}

impl AuxSearchState {
    /// 검색 버튼·⌘F — 열려 있으면 닫고, 닫혀 있으면 연다. `query`/`active`는 건드리지
    /// 않는다 — 다시 열면 하던 검색이 그대로 이어진다(완전 초기화는 [`Self::reset`] 몫).
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
        }
    }

    /// 검색 바를 닫는다. `query`는 남겨 둔다 — 다시 토글해 열면 이어서 검색할 수 있게.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// 탭이 바뀌거나 워크스페이스가 바뀌면 완전히 비운다 — 이력에서 찾던 문구가 Git
    /// 목록에 남아 있으면 "왜 안 보이지"가 된다(스펙).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 질의가 비었으면 검색하지 않는 것과 같다. **닫혀 있을 때도 거짓** — 검색 바가
    /// 안 보이는데 목록/본문이 조용히 걸러져 있으면 더 헷갈린다(닫아도 `query`는
    /// 남는데, 그 값이 필터에 다시 새지 않게 하는 게 이 조건의 목적).
    pub fn is_active(&self) -> bool {
        self.open && !self.query.is_empty()
    }
}

/// 대소문자 무시 부분 문자열 일치들의 **바이트 범위**(항상 `haystack`의 char 경계).
pub struct Matches {
    pub ranges: Vec<std::ops::Range<usize>>,
    /// [`MAX_AUX_MATCHES`]에서 멈췄으면 참 — 그 시점 이후는 더 찾지 않는다.
    pub truncated: bool,
}

/// `needle`을 대소문자 무시로 접은 char 시퀀스. 빈 needle이면 `None`(호출부가
/// "검색 안 함"으로 처리).
fn folded_needle(needle: &str) -> Option<Vec<char>> {
    if needle.is_empty() {
        return None;
    }
    Some(needle.chars().flat_map(char::to_lowercase).collect())
}

/// `haystack[at..]`이 `needle_lower`(이미 소문자로 접힌 문자열)로 시작하는지 본다.
/// **원문 문자 단위**로 소비하며 각 문자를 그때그때 `to_lowercase()`로 접어 비교한다 —
/// `to_lowercase()`가 문자 하나를 여러 문자로 늘릴 수 있어서다(예: 'İ' → "i" + 결합
/// 점 두 글자, 대문자 1글자가 소문자 2글자가 된다). 통짜 소문자 사본을 미리 만들고
/// 그 안에서 찾은 바이트 위치를 원문 인덱스로 되돌리는 방식은 이 경우 원문·사본의
/// 바이트 길이가 어긋나 되돌림이 틀어질 수 있다 — 그래서 원문을 절대 통짜로 접지
/// 않고, `chars()`가 내주는 원문 char 경계만으로 끝 오프셋을 누적한다. 반환값이
/// 있으면 항상 `at` 기준 원문 char 경계 오프셋이라 패닉 없이 슬라이스할 수 있다.
fn match_len_at(haystack: &str, at: usize, needle_lower: &[char]) -> Option<usize> {
    let mut chars = haystack[at..].chars();
    let mut needle_idx = 0usize;
    let mut end = at;
    while needle_idx < needle_lower.len() {
        let c = chars.next()?;
        end += c.len_utf8();
        for folded in c.to_lowercase() {
            if needle_idx >= needle_lower.len() || folded != needle_lower[needle_idx] {
                return None;
            }
            needle_idx += 1;
        }
    }
    Some(end)
}

/// `start`(char 경계) 이후 첫 일치를 찾는다. 후보 시작점마다 [`match_len_at`]을
/// 시도하되, 시작점 자체는 `haystack[start..].char_indices()`가 내주는 원문 char
/// 경계만 쓴다.
fn find_one(haystack: &str, start: usize, needle_lower: &[char]) -> Option<std::ops::Range<usize>> {
    for (offset, _) in haystack[start..].char_indices() {
        if let Some(end) = match_len_at(haystack, start + offset, needle_lower) {
            return Some(start + offset..end);
        }
    }
    None
}

/// 대소문자 무시 부분 문자열 — `haystack` 안 일치들의 바이트 범위. [`MAX_AUX_MATCHES`]에서
/// 멈추고(더 찾지 않는다), 멈췄으면 `truncated = true`. 빈 `needle`은 일치 없음.
pub fn find_matches(haystack: &str, needle: &str) -> Matches {
    let Some(needle_lower) = folded_needle(needle) else {
        return Matches { ranges: Vec::new(), truncated: false };
    };
    let mut ranges = Vec::new();
    let mut truncated = false;
    let mut pos = 0usize;
    while pos < haystack.len() {
        if ranges.len() >= MAX_AUX_MATCHES {
            truncated = true;
            break;
        }
        match find_one(haystack, pos, &needle_lower) {
            Some(range) => {
                pos = range.end;
                ranges.push(range);
            }
            None => break,
        }
    }
    Matches { ranges, truncated }
}

/// 목록 필터용 — 하나라도 걸리는지만 본다(전체 범위는 필요 없어 첫 일치에서 멈춘다).
pub fn contains_match(haystack: &str, needle: &str) -> bool {
    let Some(needle_lower) = folded_needle(needle) else {
        return false;
    };
    find_one(haystack, 0, &needle_lower).is_some()
}

/// 일치 구간만 배경을 달리한 [`egui::text::LayoutJob`]. `matches`는 반드시 이
/// `text`에서 뽑은 것이어야 한다(범위가 다른 문자열 기준이면 패닉한다). `active`는
/// 그 본문 기준 활성 일치 인덱스이며, 해당 구간만 `active_bg`로 칠하고 나머지
/// 일치는 `match_bg`로 칠한다.
pub fn highlighted_job(
    text: &str,
    matches: &Matches,
    active: Option<usize>,
    base: egui::TextFormat,
    match_bg: egui::Color32,
    active_bg: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if matches.ranges.is_empty() {
        job.append(text, 0.0, base);
        return job;
    }
    let mut cursor = 0usize;
    for (idx, range) in matches.ranges.iter().enumerate() {
        if range.start > cursor {
            job.append(&text[cursor..range.start], 0.0, base.clone());
        }
        let mut format = base.clone();
        format.background = if active == Some(idx) { active_bg } else { match_bg };
        job.append(&text[range.clone()], 0.0, format);
        cursor = range.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base);
    }
    job
}

/// 검색 입력 위젯 id — 포커스 요청·판정이 같은 값을 써야 한다(notes.rs `text_id`와
/// 같은 관례).
fn search_input_id() -> egui::Id {
    egui::Id::new("aux_search_input")
}

/// "이번에 열린 뒤 이미 자동 포커스를 줬는지" 표시 키. `search_bar`는 App이 소유한
/// `AuxSearchState`를 불변 참조로만 받는 순수 함수라 이 플래그를 둘 구조체 필드가
/// 없다 — work_history.rs `copy_feedback_id`와 같은 관례로 `ctx().data`에 심는다.
fn focused_once_id() -> egui::Id {
    egui::Id::new("aux_search_focused_once")
}

/// 검색 바 한 줄. 반환 intent는 App이 소비한다(질의 반영·이전/다음 이동·닫기).
/// `state.open`이 거짓이면 아무것도 그리지 않고 `None`을 돌려준다 — 진입(검색 버튼/⌘F)은
/// App이 소유하고, 이 함수는 열려 있을 때의 한 줄만 책임진다.
pub fn search_bar(
    ui: &mut egui::Ui,
    state: &AuxSearchState,
    total: usize,
    truncated: bool,
    catalog: &i18n::Catalog,
) -> Option<AuxSearchAction> {
    if !state.open {
        // 닫힌 동안 플래그를 지워야 다음에 열릴 때 다시 자동 포커스된다.
        ui.ctx().data_mut(|data| data.remove::<bool>(focused_once_id()));
        return None;
    }

    let mut action = None;
    ui.horizontal(|ui| {
        let mut query = state.query.clone();
        let response = ui.add(
            egui::TextEdit::singleline(&mut query)
                .id(search_input_id())
                .hint_text(catalog.t("search.placeholder", &[]))
                .desired_width(200.0),
        );
        if response.changed() {
            action = Some(AuxSearchAction::QueryChanged(query));
        }

        // 열린 뒤 첫 렌더에서만 포커스를 준다 — 매 프레임 강제하면 사용자가 본문을
        // 클릭해도 포커스가 도로 끌려온다(notes.rs `focus_pending`과 같은 문제의식).
        let already_focused = ui
            .ctx()
            .data(|data| data.get_temp::<bool>(focused_once_id()))
            .unwrap_or(false);
        if !already_focused {
            response.request_focus();
            ui.ctx()
                .data_mut(|data| data.insert_temp(focused_once_id(), true));
        }

        let shown_active = if total == 0 { 0 } else { state.active.min(total - 1) + 1 };
        let active_text = shown_active.to_string();
        let total_text = total.to_string();
        ui.label(catalog.t(
            "search.count",
            &[("active", active_text.as_str()), ("total", total_text.as_str())],
        ));
        if truncated {
            ui.label(catalog.t("search.truncated", &[]));
        }

        if ui
            .button("↑")
            .on_hover_text(catalog.t("search.prev", &[]))
            .clicked()
        {
            action = Some(AuxSearchAction::Prev);
        }
        if ui
            .button("↓")
            .on_hover_text(catalog.t("search.next", &[]))
            .clicked()
        {
            action = Some(AuxSearchAction::Next);
        }
        if ui
            .button("✕")
            .on_hover_text(catalog.t("search.close", &[]))
            .clicked()
        {
            action = Some(AuxSearchAction::Close);
        }

        // Enter=다음, Shift+Enter=이전, Esc=닫기 — 입력창이 포커스일 때만 소비한다
        // (workspace.rs 터미널 검색 바와 같은 관례: 키가 본문/터미널로 새지 않게).
        if response.has_focus() {
            let (enter, esc, shift) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::Escape),
                    i.modifiers.shift,
                )
            });
            if esc {
                action = Some(AuxSearchAction::Close);
            } else if enter {
                action = Some(if shift { AuxSearchAction::Prev } else { AuxSearchAction::Next });
            }
        }
    });
    action
}

/// `search_bar`가 App에 돌려주는 intent. 실제 상태 변경(질의 반영·이동·닫기)은
/// App이 `AuxSearchState`에 적용한다.
pub enum AuxSearchAction {
    QueryChanged(String),
    Prev,
    Next,
    Close,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 매칭은_대소문자를_무시한다() {
        let m = find_matches("Hello World", "world");
        assert_eq!(m.ranges, vec![6..11]);
        assert!(!m.truncated);
    }

    #[test]
    fn 빈_질의는_검색하지_않는다() {
        assert!(find_matches("아무거나", "").ranges.is_empty());
        assert!(!contains_match("아무거나", ""));
    }

    #[test]
    fn 매칭은_한글_경계를_지킨다() {
        let m = find_matches("가나다라", "나다");
        assert_eq!(m.ranges.len(), 1);
        let r = m.ranges[0].clone();
        assert_eq!(&"가나다라"[r], "나다", "char 경계에서 잘려야 한다");
    }

    #[test]
    fn 매칭은_상한에서_멈춘다() {
        let text = "a".repeat(MAX_AUX_MATCHES + 50);
        let m = find_matches(&text, "a");
        assert_eq!(m.ranges.len(), MAX_AUX_MATCHES);
        assert!(m.truncated);
    }

    #[test]
    fn 활성_일치만_다른_배경을_받는다() {
        let m = find_matches("aXaXa", "X");
        let job = highlighted_job(
            "aXaXa",
            &m,
            Some(1),
            egui::TextFormat::default(),
            egui::Color32::RED,
            egui::Color32::GREEN,
        );
        let bgs: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert!(bgs.contains(&egui::Color32::GREEN), "활성 일치가 있어야 한다");
        assert_eq!(
            bgs.iter().filter(|c| **c == egui::Color32::GREEN).count(),
            1
        );
    }

    /// 유니코드 확장 사례 — 'İ'(U+0130)는 소문자로 접으면 "i" + 결합 점(U+0307) 두
    /// 글자가 된다(1글자→2글자). 통짜 소문자 사본을 만들고 위치를 되돌리는 방식이면
    /// 이런 문자에서 바이트 길이가 어긋나 위험한데, 이 구현은 원문을 통짜로 접지
    /// 않으므로 패닉 없이(그리고 반환 범위가 char 경계로) 동작해야 한다.
    #[test]
    fn 소문자_변환시_늘어나는_문자도_통짜_변환_없이_안전하다() {
        let haystack = "test İ end";
        // 패닉하지 않고 끝까지 스캔되는 것 자체가 회귀 방지 포인트다.
        let m = find_matches(haystack, "end");
        assert_eq!(m.ranges.len(), 1);
        assert_eq!(&haystack[m.ranges[0].clone()], "end");
        // 'İ'를 원문 그대로 담은 needle을 찾을 때도 char 경계를 지킨다(자기 자신과의
        // 대소문자 무시 비교).
        let m2 = find_matches(haystack, "İ");
        assert_eq!(m2.ranges.len(), 1);
        assert_eq!(&haystack[m2.ranges[0].clone()], "İ");
    }

    /// 이모지·결합 문자에서도 char 경계를 지키고 패닉하지 않는다.
    #[test]
    fn 매칭은_이모지_경계에서_패닉하지_않는다() {
        let haystack = "hello 👍 world";
        let m = find_matches(haystack, "world");
        assert_eq!(m.ranges.len(), 1);
        assert_eq!(&haystack[m.ranges[0].clone()], "world");
        // 이모지를 needle로 써도 정확히 그 문자 하나만 찾는다.
        let m2 = find_matches(haystack, "👍");
        assert_eq!(m2.ranges.len(), 1);
        assert_eq!(&haystack[m2.ranges[0].clone()], "👍");
    }

    #[test]
    fn toggle은_열림_상태를_뒤집고_질의는_건드리지_않는다() {
        let mut state = AuxSearchState { query: "abc".into(), ..Default::default() };
        assert!(!state.open);
        state.toggle();
        assert!(state.open);
        assert_eq!(state.query, "abc");
        state.toggle();
        assert!(!state.open);
        assert_eq!(state.query, "abc", "닫아도 질의는 남아야 다시 열 때 이어진다");
    }

    #[test]
    fn reset은_모든_필드를_기본값으로_되돌린다() {
        let mut state = AuxSearchState { query: "abc".into(), open: true, active: 3 };
        state.reset();
        assert!(!state.open);
        assert!(state.query.is_empty());
        assert_eq!(state.active, 0);
    }

    #[test]
    fn is_active는_열려있고_질의가_있을_때만_참이다() {
        let mut state = AuxSearchState::default();
        assert!(!state.is_active(), "닫혀 있고 질의도 없으면 거짓");
        state.query = "x".into();
        assert!(!state.is_active(), "질의가 있어도 닫혀 있으면 거짓");
        state.open = true;
        assert!(state.is_active());
        state.query.clear();
        assert!(!state.is_active(), "빈 질의면 열려 있어도 거짓");
    }

    #[test]
    fn kittest_검색바가_열리면_입력에_포커스가_잡힌다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let state = AuxSearchState { open: true, ..Default::default() };
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut AuxSearchState| {
                search_bar(ui, state, 0, false, &catalog);
            },
            state,
        );
        harness.run();
        assert!(
            harness.ctx.memory(|m| m.has_focus(search_input_id())),
            "열리면 입력에 자동 포커스가 잡혀야 한다"
        );
    }

    #[test]
    fn kittest_타이핑하면_querychanged가_올라간다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            search: AuxSearchState,
            action: Option<AuxSearchAction>,
        }

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut State| {
                if let Some(action) = search_bar(ui, &state.search, 0, false, &catalog) {
                    state.action = Some(action);
                }
            },
            State { search: AuxSearchState { open: true, ..Default::default() }, action: None },
        );
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("abc");
        harness.run();
        assert!(
            matches!(&harness.state().action, Some(AuxSearchAction::QueryChanged(q)) if q == "abc"),
            "타이핑은 QueryChanged를 올려야 한다"
        );
    }

    #[test]
    fn kittest_닫기_버튼은_close를_낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            search: AuxSearchState,
            action: Option<AuxSearchAction>,
        }

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut State| {
                if let Some(action) = search_bar(ui, &state.search, 0, false, &catalog) {
                    state.action = Some(action);
                }
            },
            State { search: AuxSearchState { open: true, ..Default::default() }, action: None },
        );
        harness.run();
        // 버튼의 접근성 라벨은 hover text가 아니라 보이는 글자("✕") 자체다
        // (work_history.rs "View Git changes" 관례와 동일 — 버튼 텍스트가 곧 label).
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "✕")
            .click();
        harness.run();
        assert!(matches!(harness.state().action, Some(AuxSearchAction::Close)));
    }

    #[test]
    fn kittest_닫힌_검색바는_아무것도_그리지_않고_none을_낸다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut Option<AuxSearchAction>| {
                *state = search_bar(ui, &AuxSearchState::default(), 0, false, &catalog);
            },
            None,
        );
        harness.run();
        assert!(harness.state().is_none());
    }
}
