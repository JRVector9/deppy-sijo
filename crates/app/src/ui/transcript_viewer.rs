//! 작업 이력 카드의 「원문 보기」가 여는 대화 원문 뷰어 (2026-08-15 스펙 §2-4).
//! `agent_transcript::read_conversation`이 읽어 온 결과를 그대로 그린다 — 아무것도
//! 저장하지 않는다(볼 때만 읽고 닫으면 버린다). leaf는 IO를 하지 않는다: 파일을
//! 읽거나 경로를 해석하지 않고, App이 `set_conversation`으로 결과를 넣어 준다.

/// 가상화 행 높이 추정에 쓰는 한 줄당 문자 수. 실제 소프트 랩 폭은 레이아웃
/// 전에는 알 수 없어 근사치를 쓴다 — 추정이 빗나가도 스크롤 위치가 살짝
/// 흔들릴 뿐, "화면 밖 메시지는 프레임마다 다시 레이아웃하지 않는다"는
/// 가상화의 목적(프레임 비용 유계) 자체는 항상 지켜진다.
const ROW_CHARS_ESTIMATE: usize = 56;
/// 메시지 한 개가 차지하는 여백 추정치 — `render_message`의 `inner_margin`
/// 상하(4+4)와 다음 메시지 앞의 `add_space(4.0)`을 합친 값.
const MESSAGE_ROW_EXTRA: f32 = 12.0;

/// leaf 상태. IO도 파일 경로 해석도 하지 않는다 — App이 `agent_detect::transcript_path_for`로
/// 찾은 경로를 `agent_transcript::read_conversation`으로 읽어 그 결과만 넣어 준다.
#[derive(Default)]
pub struct TranscriptViewerUi {
    loading: bool,
    conversation:
        Option<Result<crate::agent_transcript::TranscriptConversation, crate::agent_transcript::TranscriptViewError>>,
    /// 새 대화가 열릴 때마다 올라간다 — ScrollArea의 `id_salt`에 섞어, 대화가
    /// 바뀌면 이전 스크롤 위치(스티키 하단 포함)를 이어받지 않고 새로 시작하게 한다.
    generation: u64,
    /// `focus_offset`이 가리키는 턴의 메시지 인덱스 범위(스펙 §6-3). `set_conversation`에서
    /// 한 번만 계산해 두고 렌더 때마다 다시 찾지 않는다.
    focus_range: Option<std::ops::Range<usize>>,
    /// `focus_offset`은 있었는데 스냅샷 창(꼬리 4MB) 안에서 그 턴을 못 찾은 경우. 조용히
    /// 최신을 보여주면 사용자는 그게 그 턴인 줄 알기 때문에 상단에 안내를 띄운다.
    focus_missing: bool,
    /// 이번 대화가 열린 뒤 아직 적용하지 않은 스크롤 목표(강조 시작 인덱스). `render`가
    /// 한 프레임 소비하면 비운다 — 이후 프레임은 사용자가 스크롤해도 되돌리지 않는다.
    pending_scroll_to: Option<usize>,
    /// `messages` 전체를 `estimate_message_lines`로 훑어 얻는 평균 줄 수(`average_lines`).
    /// 대화는 `set_conversation`에서만 바뀌므로 거기서 한 번만 계산해 캐싱한다 — `render`가
    /// 매 프레임 최대 200개·1MB 메시지를 다시 훑지 않게 한다. `render`는 이 값에 그
    /// 프레임의 `line_height`만 곱해 쓴다(O(1)).
    cached_avg_lines: f32,
}

impl TranscriptViewerUi {
    /// App이 원문 IO를 시작할 때 부른다(2026-08-15, Task 10이 배선한다).
    pub fn set_loading(&mut self) {
        self.loading = true;
    }

    /// App이 원문 IO를 끝내면 부른다. 성공이든 실패든 결과를 그대로 담는다 —
    /// 카드 하나의 대화가 이상해도 뷰어 자체는 죽지 않는다(fail-soft).
    ///
    /// `focus_offset`은 카드가 가리키는 턴을 연 레코드 줄의 절대 파일 오프셋(스펙
    /// §6-1) — `focus_range`로 강조·스크롤할 인덱스 범위를 여기서 한 번만 계산해
    /// 둔다. `None`이면(다른 진입점) 강조 없이 지금처럼 맨 아래에서 시작한다.
    pub fn set_conversation(
        &mut self,
        result: Result<
            crate::agent_transcript::TranscriptConversation,
            crate::agent_transcript::TranscriptViewError,
        >,
        focus_offset: Option<u64>,
    ) {
        self.loading = false;
        self.focus_range = None;
        self.focus_missing = false;
        self.pending_scroll_to = None;
        self.cached_avg_lines = result.as_ref().map_or(1.0, |conversation| average_lines(&conversation.messages));
        if let (Some(offset), Ok(conversation)) = (focus_offset, result.as_ref()) {
            match focus_range(&conversation.messages, conversation.truncated, offset) {
                Some(range) => {
                    self.pending_scroll_to = Some(range.start);
                    self.focus_range = Some(range);
                }
                None => self.focus_missing = true,
            }
        }
        self.conversation = Some(result);
        self.generation += 1;
    }

    /// 아직 아무것도 열지 않은 초기 상태인가. App이 이걸로 좌측 목록에서 아무
    /// 카드도 고르지 않은 상태인지 물을 수 있다. 대화가 0건이어도 한 번
    /// 열렸으면(로딩 포함) false다 — "열림"과 "내용 있음"은 별개다.
    pub fn is_empty(&self) -> bool {
        !self.loading && self.conversation.is_none()
    }

    pub fn render(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        if self.loading {
            ui.weak(catalog.t("history.transcript.loading", &[]));
            return;
        }
        let Some(result) = self.conversation.as_ref() else {
            ui.weak(catalog.t("history.transcript.empty", &[]));
            return;
        };
        let conversation = match result {
            Err(crate::agent_transcript::TranscriptViewError::NotFound) => {
                ui.weak(catalog.t("history.transcript.not_found", &[]));
                return;
            }
            Err(crate::agent_transcript::TranscriptViewError::ReadFailed) => {
                ui.weak(catalog.t("history.transcript.error", &[]));
                return;
            }
            Ok(conversation) => conversation,
        };
        if conversation.truncated {
            ui.weak(catalog.t("history.transcript.truncated", &[]));
        }
        if self.focus_missing {
            // 같은 자리·같은 모양으로 truncated 안내 바로 아래에 쌓인다 — 조용히
            // 최신을 보여주면 사용자는 그게 그 턴인 줄 알기 때문에 반드시 말해 준다.
            ui.weak(catalog.t("history.transcript.focus_missing", &[]));
        }

        let tokens = crate::ui::designall::tokens(ui.visuals());
        let line_height = ui.text_style_height(&egui::TextStyle::Body);
        let messages = &conversation.messages;
        let row_height = average_row_height(self.cached_avg_lines, line_height);

        // 스티키 하단: 강조할 턴이 없을 때(처음 열 때, 또는 그 턴을 못 찾았을 때)
        // 최신 메시지가 보이는 맨 아래에서 시작한다. 강조할 턴이 있으면 그 시작
        // 인덱스로 직접 스크롤하므로 하단에 붙지 않는다.
        let stick_to_bottom = self.focus_range.is_none();
        // 새 대화(제너레이션이 바뀐 프레임)에서만, 그리고 그 프레임 단 한 번만
        // 강조 시작 인덱스로 스크롤한다 — hunk 점프하는 diff_viewer.rs와 같은
        // `.take()` 관례. `show_rows`는 행 높이가 균일하다고 가정하므로 egui가
        // 내부적으로 쓰는 `row_height + item_spacing.y`(scroll_area.rs의
        // `row_height_with_spacing`)를 그대로 곱해야 실제 레이아웃과 스크롤
        // 목표가 어긋나지 않는다.
        let mut scroll = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(stick_to_bottom)
            .id_salt(("transcript-viewer-scroll", self.generation));
        if let Some(start) = self.pending_scroll_to.take() {
            let row_height_with_spacing = row_height + ui.spacing().item_spacing.y;
            scroll = scroll.vertical_scroll_offset(start as f32 * row_height_with_spacing);
        }
        let highlight_range = self.focus_range.clone();
        scroll.show_rows(ui, row_height, messages.len(), |ui, range| {
            for index in range {
                let highlighted = highlight_range.as_ref().is_some_and(|r| r.contains(&index));
                render_message(ui, &messages[index], tokens, catalog, highlighted);
            }
        });
    }
}

fn render_message(
    ui: &mut egui::Ui,
    message: &crate::agent_transcript::ConversationMessage,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
    highlighted: bool,
) {
    let mut frame =
        egui::Frame::NONE.fill(message_background(tokens, message.role, highlighted)).inner_margin(egui::Margin::symmetric(8, 4));
    if highlighted {
        // 배경 섞기만으로는 라이트 테마에서 `content_canvas`와 `selected_background`가
        // 원래 가깝다(두 표면 다 "물러난 면"이라 채도 차이가 작다) — 그 자리는
        // work_history.rs의 펼친 카드가 이미 쓰는 관례(선택 배경 + accent 테두리)로
        // 메운다. 그러면 라이트/다크 어느 쪽이든 테두리가 강조를 확실히 보여준다.
        frame = frame.stroke(egui::Stroke::new(1.0, tokens.accent));
    }
    frame.show(ui, |ui| {
        let label = match message.role {
            crate::agent_transcript::ConversationRole::User => catalog.t("history.role.user", &[]),
            crate::agent_transcript::ConversationRole::Assistant => catalog.t("history.role.agent", &[]),
        };
        ui.label(egui::RichText::new(label).small().weak());
        ui.add(egui::Label::new(message.text.as_str()).wrap().selectable(true));
    });
    ui.add_space(4.0);
}

/// 강조 여부까지 반영한 메시지 배경. 강조되면 역할별 배경을 `selected_background`
/// 쪽으로 섞는다 — 역할 구분(사용자 액센트 / 에이전트 페이지 바닥)은 완전히
/// 지우지 않으면서 "이 턴이다"라는 신호를 더한다. 시각적 확정은
/// [`render_message`]가 함께 붙이는 accent 테두리가 맡는다.
fn message_background(
    tokens: crate::ui::designall::Tokens,
    role: crate::agent_transcript::ConversationRole,
    highlighted: bool,
) -> egui::Color32 {
    let bg = role_background(tokens, role);
    if highlighted {
        crate::ui::designall::mix(bg, tokens.selected_background, 0.6)
    } else {
        bg
    }
}

/// 사용자 발화는 액센트를 옅게 섞은 면, 에이전트 발화는 페이지 바닥
/// (`content_canvas`)으로 구분한다 — 둘 다 `designall::Tokens`에서 고른 값이라
/// 라이트/다크 모두 자동으로 성립한다(하드코딩 색 없음).
fn role_background(
    tokens: crate::ui::designall::Tokens,
    role: crate::agent_transcript::ConversationRole,
) -> egui::Color32 {
    match role {
        crate::agent_transcript::ConversationRole::User => {
            crate::ui::designall::mix(tokens.app_background, tokens.accent, 0.12)
        }
        crate::agent_transcript::ConversationRole::Assistant => tokens.content_canvas,
    }
}

/// 강조할 메시지 인덱스 범위(스펙 §6-3). 찾지 못하면 `None`.
///
/// `truncated`(스냅샷이 파일 시작을 못 담았다)이고 `focus_offset`이 창의 **첫**
/// 메시지 offset보다 앞이면 그 턴은 창 밖(더 앞)으로 밀려난 것이다 — 이때
/// `offset >= focus_offset`을 그대로 적용하면 가장 오래된(하지만 엉뚱한) 메시지에
/// 걸려 버리므로 먼저 `None`으로 끊는다. 잘리지 않았다면(파일 전체가 창 안) 이
/// 관용을 적용하지 않는다.
///
/// 그 관문을 통과하면 시작은 `offset >= focus_offset`인 **첫** 메시지 — 정확히
/// 일치하는 것이 정상이지만, 턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있어
/// 부등호로 잡는다. 끝은 시작 다음에 오는 **첫 User 메시지 직전**(그게 다음
/// 턴의 시작이다) — 없으면 대화 끝까지.
fn focus_range(
    messages: &[crate::agent_transcript::ConversationMessage],
    truncated: bool,
    focus_offset: u64,
) -> Option<std::ops::Range<usize>> {
    if truncated && messages.first().is_some_and(|first| focus_offset < first.offset) {
        return None;
    }
    let start = messages.iter().position(|m| m.offset >= focus_offset)?;
    let end = messages[start + 1..]
        .iter()
        .position(|m| m.role == crate::agent_transcript::ConversationRole::User)
        .map_or(messages.len(), |relative| start + 1 + relative);
    Some(start..end)
}

/// 메시지 하나가 차지할 표시 줄 수 추정 — 역할 라벨 한 줄 + 본문 줄들(명시적
/// 개행 기준, [`ROW_CHARS_ESTIMATE`]로 소프트 랩까지 근사).
fn estimate_message_lines(text: &str) -> usize {
    let body_lines: usize = text
        .lines()
        .map(|line| line.chars().count().div_ceil(ROW_CHARS_ESTIMATE).max(1))
        .sum();
    1 + body_lines.max(1)
}

/// 대화 전체 메시지의 평균 추정 줄 수 — `messages`를 훑는 O(n) 비용은 여기에만
/// 있다. `set_conversation`이 대화가 바뀔 때 한 번만 불러 `cached_avg_lines`에
/// 담아 두고, `render`는 매 프레임 이 값을 다시 계산하지 않는다.
fn average_lines(messages: &[crate::agent_transcript::ConversationMessage]) -> f32 {
    if messages.is_empty() {
        return 1.0;
    }
    let total_lines: usize = messages.iter().map(|m| estimate_message_lines(&m.text)).sum();
    total_lines as f32 / messages.len() as f32
}

/// `show_rows`에 넘길 단일 행 높이 — 평균 추정 줄 수(`average_lines`)에 이번 프레임의
/// `line_height`를 곱한다. `show_rows`는 모든 행에 같은 높이를 가정하므로(egui 0.35
/// API), 메시지마다 실제 높이가 달라도 평균으로 근사한다 — 가상화의 통상 트레이드오프다.
fn average_row_height(avg_lines: f32, line_height: f32) -> f32 {
    avg_lines * line_height + MESSAGE_ROW_EXTRA
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 빈_뷰어는_안내_문구를_보여준다() {
        let mut ui = TranscriptViewerUi::default();
        assert!(ui.is_empty());
        ui.set_conversation(Ok(crate::agent_transcript::TranscriptConversation::default()), None);
        assert!(!ui.is_empty(), "열었으면 비어 있지 않다 — 대화가 0건이어도 상태는 열림이다");
    }

    #[test]
    fn 로딩_중이면_빈_상태가_아니다() {
        let mut ui = TranscriptViewerUi::default();
        ui.set_loading();
        assert!(!ui.is_empty(), "로딩도 열림이다 — 대화가 아직 안 왔을 뿐이다");
    }

    fn harness_for(viewer: TranscriptViewerUi) -> egui_kittest::Harness<'static, TranscriptViewerUi> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut TranscriptViewerUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.render(ui, &catalog);
            },
            viewer,
        )
    }

    fn message(
        role: crate::agent_transcript::ConversationRole,
        text: &str,
        offset: u64,
    ) -> crate::agent_transcript::ConversationMessage {
        crate::agent_transcript::ConversationMessage { role, text: text.to_owned(), at: None, offset }
    }

    #[test]
    fn kittest_열기_전에는_안내_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut harness = harness_for(TranscriptViewerUi::default());
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.empty", &[]));
    }

    #[test]
    fn kittest_로딩_중이면_로딩_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_loading();
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.loading", &[]));
    }

    #[test]
    fn kittest_찾지_못한_대화는_not_found_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(Err(crate::agent_transcript::TranscriptViewError::NotFound), None);
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.not_found", &[]));
    }

    #[test]
    fn kittest_읽기_실패는_에러_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(Err(crate::agent_transcript::TranscriptViewError::ReadFailed), None);
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.error", &[]));
    }

    #[test]
    fn kittest_잘린_대화는_상단에_표시를_남긴다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![message(crate::agent_transcript::ConversationRole::User, "안녕", 0)],
                truncated: true,
            }),
            None,
        );
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.truncated", &[]));
    }

    #[test]
    fn kittest_메시지는_역할_라벨과_함께_그려진다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![
                    message(crate::agent_transcript::ConversationRole::User, "질문입니다", 0),
                    message(crate::agent_transcript::ConversationRole::Assistant, "답변입니다", 100),
                ],
                truncated: false,
            }),
            None,
        );
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.role.user", &[]));
        harness.get_by_label(&catalog.t("history.role.agent", &[]));
        harness.get_by_label("질문입니다");
        harness.get_by_label("답변입니다");
    }

    #[test]
    fn kittest_가상화는_화면_밖_메시지를_그리지_않는다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        let messages = (0..200)
            .map(|index| {
                message(
                    crate::agent_transcript::ConversationRole::Assistant,
                    &format!("메시지 {index}"),
                    index as u64 * 100,
                )
            })
            .collect();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation { messages, truncated: false }),
            None,
        );
        let mut harness = harness_for(viewer);
        harness.run();

        assert!(
            harness.query_by_label("메시지 199").is_some(),
            "스티키 하단이라 가장 최근 메시지는 보여야 한다"
        );
        assert!(
            harness.query_by_label("메시지 0").is_none(),
            "가상화되면 맨 처음 메시지는 화면 밖이라 그려지지 않는다 — 200개를 매 프레임 \
             전부 그리면 프레임 비용이 메시지 수에 비례해 유계가 아니게 된다"
        );
    }

    #[test]
    fn 추정_줄_수는_개행과_긴_줄_모두_반영한다() {
        assert_eq!(estimate_message_lines("한 줄"), 2, "역할 라벨 1줄 + 본문 1줄");
        assert_eq!(estimate_message_lines("첫 줄\n둘째 줄\n셋째 줄"), 4, "라벨 1줄 + 본문 3줄");
        let long = "가".repeat(ROW_CHARS_ESTIMATE * 3);
        assert_eq!(estimate_message_lines(&long), 4, "개행 없이 길어도 추정 폭만큼 나눠 센다");
    }

    #[test]
    fn 평균_행_높이는_메시지가_없으면_한_줄_높이다() {
        assert_eq!(average_row_height(average_lines(&[]), 20.0), 20.0 + MESSAGE_ROW_EXTRA);
    }

    #[test]
    fn 평균_행_높이는_긴_메시지가_섞이면_커진다() {
        let short = message(crate::agent_transcript::ConversationRole::User, "짧다", 0);
        let long = message(
            crate::agent_transcript::ConversationRole::Assistant,
            &"가".repeat(ROW_CHARS_ESTIMATE * 10),
            100,
        );
        let short_only = average_row_height(average_lines(std::slice::from_ref(&short)), 20.0);
        let mixed = average_row_height(average_lines(&[short, long]), 20.0);
        assert!(mixed > short_only, "긴 메시지가 섞이면 평균 높이가 커져야 한다");
    }

    #[test]
    fn 역할별_배경은_서로_다르고_토큰에서만_고른다() {
        let tokens = crate::ui::designall::DARK;
        let user_bg = role_background(tokens, crate::agent_transcript::ConversationRole::User);
        let agent_bg = role_background(tokens, crate::agent_transcript::ConversationRole::Assistant);
        assert_ne!(user_bg, agent_bg);
        assert_eq!(agent_bg, tokens.content_canvas, "에이전트는 페이지 바닥 톤을 그대로 쓴다");
    }

    #[test]
    fn 강조_배경은_역할_배경과_구분되고_라이트_다크_모두_성립한다() {
        // designall::tokens에서만 고른 값이라 라이트/다크 둘 다 자동으로 성립해야
        // 한다 — 하드코딩 색이면 한쪽에서만 우연히 달라 보일 수 있다.
        for tokens in [crate::ui::designall::DARK, crate::ui::designall::LIGHT] {
            for role in [
                crate::agent_transcript::ConversationRole::User,
                crate::agent_transcript::ConversationRole::Assistant,
            ] {
                let base = message_background(tokens, role, false);
                let highlighted = message_background(tokens, role, true);
                assert_ne!(base, highlighted, "강조되면 배경이 눈에 띄게 달라져야 한다");
            }
        }
    }

    #[test]
    fn 초점_범위는_그_턴만_잡는다() {
        let messages = vec![
            message(crate::agent_transcript::ConversationRole::User, "턴1 지시", 0),
            message(crate::agent_transcript::ConversationRole::Assistant, "턴1 답", 100),
            message(crate::agent_transcript::ConversationRole::User, "턴2 지시", 200),
            message(crate::agent_transcript::ConversationRole::Assistant, "턴2 답", 300),
        ];
        assert_eq!(focus_range(&messages, false, 0), Some(0..2), "다음 User 직전까지");
        assert_eq!(focus_range(&messages, false, 200), Some(2..4), "마지막 턴은 끝까지");
    }

    #[test]
    fn 초점_범위는_정확히_일치하지_않아도_다음_메시지를_잡는다() {
        // 턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있다 — 부등호로 잡는다. 잘리지
        // 않았다면(파일 전체가 창 안) 이 관용을 적용하는 게 옳다.
        let messages = vec![
            message(crate::agent_transcript::ConversationRole::User, "턴1", 0),
            message(crate::agent_transcript::ConversationRole::User, "턴2", 200),
        ];
        assert_eq!(focus_range(&messages, false, 150), Some(1..2));
    }

    #[test]
    fn 창_밖의_턴은_초점을_잡지_못한다() {
        let messages = vec![message(crate::agent_transcript::ConversationRole::User, "최근", 900)];
        // 잘리지 않았다면(파일 전체가 창 안) `offset >= focus_offset`으로 뒤쪽
        // 메시지를 잡는 게 옳다 — 턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있어서다.
        assert_eq!(focus_range(&messages, false, 100), Some(0..1), "안 잘렸으면 뒤쪽을 잡는다");
        assert_eq!(focus_range(&messages, false, 1_000), None, "그보다 뒤는 없다");
        // 잘렸다면(스냅샷 창이 파일 시작을 못 담았다) `focus_offset`이 창의 첫
        // 메시지보다 앞이라는 건 그 턴이 창 밖(더 앞)으로 밀려났다는 뜻이다 — 엉뚱한
        // (더 최근) 메시지를 그 턴인 것처럼 강조하면 안 되므로 못 찾은 것으로 취급한다.
        assert_eq!(focus_range(&messages, true, 100), None, "잘렸으면 창 앞의 턴은 못 찾는다");
        assert_eq!(focus_range(&messages, true, 1_000), None, "그보다 뒤는 잘렸어도 여전히 없다");
    }

    #[test]
    fn kittest_초점을_못_찾으면_안내를_띄운다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        // 스냅샷 창(꼬리 4MB) 앞이라 이 오프셋을 가진 메시지가 없다 — 조용히 최신을
        // 보여주면 사용자는 그게 그 턴인 줄 안다.
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![message(crate::agent_transcript::ConversationRole::User, "최근", 900)],
                truncated: false,
            }),
            Some(1_000),
        );
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.focus_missing", &[]));
    }

    #[test]
    fn kittest_초점_범위는_강조_배경을_받는다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![
                    message(crate::agent_transcript::ConversationRole::User, "턴1 지시", 0),
                    message(crate::agent_transcript::ConversationRole::Assistant, "턴1 답", 100),
                    message(crate::agent_transcript::ConversationRole::User, "턴2 지시", 200),
                    message(crate::agent_transcript::ConversationRole::Assistant, "턴2 답", 300),
                ],
                truncated: false,
            }),
            Some(200),
        );
        let mut harness = harness_for(viewer);
        harness.run();

        // 실제 강조 색(배경 섞기 + accent 테두리)은 kittest가 픽셀을 읽지 못해
        // 직접 못 잰다 — 그건 위 `강조_배경은_역할_배경과_구분되고…` 순수 함수
        // 테스트가 맡는다. 여기서는 그 턴을 찾았을 때(안내문 없이) 강조 로직이
        // 범위 안 메시지를 가리거나 죽이지 않고 그대로 보여주는지 확인한다.
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        assert!(
            harness.query_by_label(&catalog.t("history.transcript.focus_missing", &[])).is_none(),
            "찾은 턴이니 안내가 없어야 한다"
        );
        harness.get_by_label("턴2 지시");
        harness.get_by_label("턴2 답");
    }
}
