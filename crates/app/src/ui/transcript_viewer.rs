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
}

impl TranscriptViewerUi {
    /// App이 원문 IO를 시작할 때 부른다(2026-08-15, Task 10이 배선한다).
    pub fn set_loading(&mut self) {
        self.loading = true;
    }

    /// App이 원문 IO를 끝내면 부른다. 성공이든 실패든 결과를 그대로 담는다 —
    /// 카드 하나의 대화가 이상해도 뷰어 자체는 죽지 않는다(fail-soft).
    pub fn set_conversation(
        &mut self,
        result: Result<
            crate::agent_transcript::TranscriptConversation,
            crate::agent_transcript::TranscriptViewError,
        >,
    ) {
        self.loading = false;
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

        let tokens = crate::ui::designall::tokens(ui.visuals());
        let line_height = ui.text_style_height(&egui::TextStyle::Body);
        let messages = &conversation.messages;
        let row_height = average_row_height(messages, line_height);

        // 스티키 하단: 처음 열 때(그리고 새 대화로 바뀔 때, generation을 salt로
        // 섞어 이전 스크롤 상태를 이어받지 않는다) 최신 메시지가 보이는 맨
        // 아래에서 시작한다. hunk 점프처럼 특정 행으로 미리 계산해 스크롤하는
        // diff_viewer.rs와 달리, "항상 최신이 보이는 하단"은 뷰포트 높이를
        // 몰라도 egui가 알아서 맞춰 주는 `stick_to_bottom`이 정확하다.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .id_salt(("transcript-viewer-scroll", self.generation))
            .show_rows(ui, row_height, messages.len(), |ui, range| {
                for index in range {
                    render_message(ui, &messages[index], tokens, catalog);
                }
            });
    }
}

fn render_message(
    ui: &mut egui::Ui,
    message: &crate::agent_transcript::ConversationMessage,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) {
    let bg = role_background(tokens, message.role);
    egui::Frame::NONE.fill(bg).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        let label = match message.role {
            crate::agent_transcript::ConversationRole::User => catalog.t("history.role.user", &[]),
            crate::agent_transcript::ConversationRole::Assistant => catalog.t("history.role.agent", &[]),
        };
        ui.label(egui::RichText::new(label).small().weak());
        ui.add(egui::Label::new(message.text.as_str()).wrap().selectable(true));
    });
    ui.add_space(4.0);
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

/// 메시지 하나가 차지할 표시 줄 수 추정 — 역할 라벨 한 줄 + 본문 줄들(명시적
/// 개행 기준, [`ROW_CHARS_ESTIMATE`]로 소프트 랩까지 근사).
fn estimate_message_lines(text: &str) -> usize {
    let body_lines: usize = text
        .lines()
        .map(|line| line.chars().count().div_ceil(ROW_CHARS_ESTIMATE).max(1))
        .sum();
    1 + body_lines.max(1)
}

/// `show_rows`에 넘길 단일 행 높이 — 대화 전체 메시지의 평균 추정 줄 수.
/// `show_rows`는 모든 행에 같은 높이를 가정하므로(egui 0.35 API), 메시지마다
/// 실제 높이가 달라도 평균으로 근사한다 — 가상화의 통상 트레이드오프다.
fn average_row_height(messages: &[crate::agent_transcript::ConversationMessage], line_height: f32) -> f32 {
    if messages.is_empty() {
        return line_height + MESSAGE_ROW_EXTRA;
    }
    let total_lines: usize = messages.iter().map(|m| estimate_message_lines(&m.text)).sum();
    let avg_lines = total_lines as f32 / messages.len() as f32;
    avg_lines * line_height + MESSAGE_ROW_EXTRA
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 빈_뷰어는_안내_문구를_보여준다() {
        let mut ui = TranscriptViewerUi::default();
        assert!(ui.is_empty());
        ui.set_conversation(Ok(crate::agent_transcript::TranscriptConversation::default()));
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
    ) -> crate::agent_transcript::ConversationMessage {
        crate::agent_transcript::ConversationMessage { role, text: text.to_owned(), at: None }
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
        viewer.set_conversation(Err(crate::agent_transcript::TranscriptViewError::NotFound));
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.not_found", &[]));
    }

    #[test]
    fn kittest_읽기_실패는_에러_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(Err(crate::agent_transcript::TranscriptViewError::ReadFailed));
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.error", &[]));
    }

    #[test]
    fn kittest_잘린_대화는_상단에_표시를_남긴다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(Ok(crate::agent_transcript::TranscriptConversation {
            messages: vec![message(crate::agent_transcript::ConversationRole::User, "안녕")],
            truncated: true,
        }));
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.truncated", &[]));
    }

    #[test]
    fn kittest_메시지는_역할_라벨과_함께_그려진다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(Ok(crate::agent_transcript::TranscriptConversation {
            messages: vec![
                message(crate::agent_transcript::ConversationRole::User, "질문입니다"),
                message(crate::agent_transcript::ConversationRole::Assistant, "답변입니다"),
            ],
            truncated: false,
        }));
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
            .map(|index| message(crate::agent_transcript::ConversationRole::Assistant, &format!("메시지 {index}")))
            .collect();
        viewer.set_conversation(Ok(crate::agent_transcript::TranscriptConversation {
            messages,
            truncated: false,
        }));
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
        assert_eq!(average_row_height(&[], 20.0), 20.0 + MESSAGE_ROW_EXTRA);
    }

    #[test]
    fn 평균_행_높이는_긴_메시지가_섞이면_커진다() {
        let short = message(crate::agent_transcript::ConversationRole::User, "짧다");
        let long = message(
            crate::agent_transcript::ConversationRole::Assistant,
            &"가".repeat(ROW_CHARS_ESTIMATE * 10),
        );
        let short_only = average_row_height(std::slice::from_ref(&short), 20.0);
        let mixed = average_row_height(&[short, long], 20.0);
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
}
