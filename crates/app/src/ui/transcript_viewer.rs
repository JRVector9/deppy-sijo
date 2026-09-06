//! 작업 이력 카드의 「원문 보기」가 여는 대화 원문 뷰어 (2026-08-15 스펙 §2-4).
//! `agent_transcript::read_conversation`이 읽어 온 결과를 그대로 그린다 — 아무것도
//! 저장하지 않는다(볼 때만 읽고 닫으면 버린다). leaf는 IO를 하지 않는다: 파일을
//! 읽거나 경로를 해석하지 않고, App이 `set_conversation`으로 결과를 넣어 준다.

/// leaf 상태. IO도 파일 경로 해석도 하지 않는다 — App이 `agent_detect::transcript_path_for`로
/// 찾은 경로를 `agent_transcript::read_conversation`으로 읽어 그 결과만 넣어 준다.
#[derive(Default)]
pub struct TranscriptViewerUi {
    loading: bool,
    conversation: Option<
        Result<
            crate::agent_transcript::TranscriptConversation,
            crate::agent_transcript::TranscriptViewError,
        >,
    >,
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
    /// 메시지별로 실제로 배치해서 잰 프레임 높이(간격·add_space 제외, 순수 프레임
    /// 높이만). 아직 한 번도 배치 안 된 메시지는 `None`이다. `set_conversation`에서
    /// 대화가 바뀌면 통째로 비운다 — 옛 대화의 높이가 남으면 스크롤이 통째로 어긋난다.
    heights: Vec<Option<f32>>,
    /// 이 대화를 연 뒤 아직 한 번도 "전부 배치"를 하지 않았다. 첫 프레임은 가상화
    /// 없이 지금까지 해오던 대로 전부 배치한다 — 모든 메시지의 높이를 실측하는
    /// 동시에, stick_to_bottom과 focus scroll_to_rect를 **실제로 배치된 rect**로
    /// 정확히 착지시킨다(2026-08-18 사용자가 겪은 깜빡임은 착지가 어긋나서
    /// 생겼다 — 여기서 타협하지 않는다). 두 번째 프레임부터는 이미 모든 높이를
    /// 실측해 뒀으므로 가상화해도 흔들리지 않는다.
    initial_pass_done: bool,
    /// 보조 검색 캐시 — 질의가 바뀔 때만 다시 채운다(diff_viewer.rs `SearchCache`와
    /// 같은 관례, 2026-08-18 계획 Task 5: 매 프레임 200개 전수 스캔 회피).
    search_cache: SearchCache,
    /// 직전 프레임에 스크롤을 트리거한 (질의, 활성 인덱스) — 같은 값이 반복되는
    /// 프레임엔 다시 스크롤하지 않는다(diff_viewer.rs `search_scroll_key`와 같은
    /// 관례). 검색이 꺼지면(빈 질의) `None`으로 돌아간다.
    search_scroll_key: Option<(String, usize)>,
    /// `search_scroll_key`가 이번에 바뀌어 스크롤이 필요하다고 표시한 메시지
    /// 인덱스. 부트스트랩 프레임(`initial_pass_done == false`, 모든 메시지를 배치해
    /// 실측하는 중)이면 여기서 소비하지 않고 다음 프레임으로 넘긴다 — 아직 실측
    /// 전인 잠정 높이로 오프셋을 계산하면 다음 프레임에 다시 어긋나 튄다(위
    /// `initial_pass_done` 문서가 설명하는 깜빡임과 같은 종류의 문제). 턴 초점
    /// (`pending_scroll_to`)과 별개 필드로 둔 이유도 같다 — 초점은 항상 부트스트랩
    /// 프레임에서만 소비되는 rect 기반 스크롤이라 이 필드와 소비 시점이 다르다.
    pending_search_scroll_to: Option<usize>,
}

/// 질의별 검색 캐시 — [`TranscriptViewerUi::render`]가 질의가 바뀔 때만
/// [`build_search_cache`]로 다시 채운다(대화가 길어도 매 프레임 전수 스캔 회피).
#[derive(Default)]
struct SearchCache {
    query: String,
    /// 메시지 i에서 시작하는 전역(대화 전체 기준) 일치 인덱스. 길이는 항상
    /// `messages.len() + 1`(마지막 원소는 총계 `total`인 sentinel) — 메시지 i의
    /// 일치 개수는 `message_match_start[i + 1] - message_match_start[i]`로 구한다.
    message_match_start: Vec<usize>,
    // `total`·`truncated`는 `TranscriptViewerUi::search_summary`로만 읽는다(App의
    // `{active}/{total}` 카운터, diff_viewer.rs `SearchCache`와 같은 관례).
    /// 대화 전체 일치 수([`crate::ui::aux_search::MAX_AUX_MATCHES`]에서 멈췄으면 그 이하).
    total: usize,
    truncated: bool,
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
        self.heights.clear();
        self.initial_pass_done = false;
        // 검색 캐시는 이 대화의 메시지 인덱스를 전제로 한다 — 대화가 바뀌면 그
        // 전제가 깨지므로 함께 비운다(2026-08-18 계획 Task 5).
        self.search_cache = SearchCache::default();
        self.search_scroll_key = None;
        self.pending_search_scroll_to = None;
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

    /// 대화 전체 일치 수·잘림 여부 — App이 `{active}/{total}` 카운터를 그릴 때 쓴다.
    /// [`Self::render`] 호출 시 질의가 바뀔 때만 갱신되는 캐시를 그대로 돌려준다(매
    /// 프레임 전수 스캔 없음). App은 이 순서를 지켜야 한다: 이 값을 읽어 검색 바를
    /// 그린 뒤, (바뀐 질의가 있으면 반영한) `render`를 호출한다 — 그래야 검색 바가
    /// 그 프레임에 보여주는 카운트가 `render`가 방금 그린 강조와 같은 질의 기준이다
    /// (질의가 막 바뀐 프레임에는 `render` 호출 전이라 직전 질의의 값이 잠깐 보일
    /// 수 있다 — 1프레임 지연, diff_viewer.rs `search_summary`와 같은 계약).
    pub fn search_summary(&self) -> (usize, bool) {
        (self.search_cache.total, self.search_cache.truncated)
    }

    /// `search`는 (질의, 대화 전체 기준 활성 일치 인덱스) — App이 소유한
    /// `AuxSearchState`에서 뽑아 넘긴다. `None`이면 보조 검색이
    /// 꺼져 있거나 이 원문 뷰어가 대상이 아니라는 뜻이라 강조 없이 예전처럼 그린다.
    /// diff_viewer.rs `render`의 `search` 인자와 같은 형태다(2026-08-18 계획 Task 4·5
    /// — 소비자 둘 다 같은 모양을 쓰기로 함).
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        search: Option<(&str, usize)>,
    ) {
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
        let messages = &conversation.messages;
        // 대화 길이와 캐시 길이가 안 맞으면(주로 방금 set_conversation 직후) 새로
        // 비운 크기로 맞춘다 — set_conversation은 이미 `heights.clear()`로 비웠지만,
        // 여기서 실제 메시지 수만큼 `None`으로 채워야 아래 인덱싱이 안전하다.
        if self.heights.len() != messages.len() {
            self.heights = vec![None; messages.len()];
        }

        // ── 보조 검색: 질의가 바뀔 때만 캐시를 다시 채운다(diff_viewer.rs와 같은
        // 관례, 매 프레임 전수 스캔 회피). 빈 질의는 검색 안 함과 같다(aux_search
        // 관례) — `active_search`로 한 번에 접어 이후 코드가 "검색 없음"과 "빈
        // 질의"를 따로 취급하지 않는다.
        let active_search = search.filter(|(query, _)| !query.is_empty());
        let query = active_search.map_or("", |(query, _)| query);
        let active = active_search.map(|(_, active)| active);
        if self.search_cache.query != query {
            self.search_cache = build_search_cache(messages, query);
        }
        // 전역 활성 인덱스가 속한 메시지 — 있으면 스크롤 대상이자 stick_to_bottom을
        // 끄는 근거다(아래). 캐시 조회라 매 프레임 다시 계산해도 전수 스캔이 아니다.
        let search_target_message = active.and_then(|active| {
            message_for_active_match(&self.search_cache.message_match_start, active)
        });
        // 활성 일치가 바뀌었거나(또는 검색이 새로 열렸거나 닫혔거나) 질의가 바뀐
        // 프레임에만 스크롤 대상을 세팅한다 — 매 프레임 세팅하면 검색이 열린 동안
        // 사용자가 본문을 자유롭게 스크롤할 수 없다(hunk 이동·턴 초점과 같은
        // 문제의식).
        let scroll_key = active.map(|active| (query.to_owned(), active));
        if scroll_key != self.search_scroll_key {
            self.search_scroll_key = scroll_key;
            self.pending_search_scroll_to = search_target_message;
        }

        // 스티키 하단: 강조할 턴도 없고 스크롤할 활성 검색 일치도 없을 때만(처음 열
        // 때, 또는 그 턴/일치를 못 찾았을 때) 최신 메시지가 보이는 맨 아래에서
        // 시작한다. 둘 중 하나라도 있으면 그 위치로 직접 스크롤하므로 하단에 붙지
        // 않는다 — 여기서 검색 쪽을 빼먹으면 stick_to_bottom이 매 프레임 검색
        // 스크롤 오프셋을 도로 덮어써 버린다(2026-08-18 kittest로 실제로 잡힌 회귀).
        let stick_to_bottom = self.focus_range.is_none() && search_target_message.is_none();
        let highlight_range = self.focus_range.clone();
        let scroll_to = self.pending_scroll_to.take();

        // 검색 스크롤은 실측 높이가 이미 다 있을 때만 `slot_offsets` 기반 오프셋이
        // 정확하다. 아직 부트스트랩 전이면(대화가 막 열렸는데 검색이 이미 켜져
        // 있던 경우 — 카드를 바꿔도 검색은 안 닫힌다, 스펙) 여기서 소비하지 않고
        // 다음 프레임(부트스트랩이 실측을 끝낸 뒤)으로 넘긴다 — 안 그러면 미실측
        // 잠정 높이로 계산한 오프셋이 실제 높이가 드러나는 대로 다시 어긋나 튄다
        // (위 `initial_pass_done` 문서가 설명하는 깜빡임과 같은 종류의 문제라 여기서
        // 도 타협하지 않는다). 그 한 프레임을 놓치지 않도록 명시적으로 다시 그리길
        // 요청한다.
        let search_scroll_offset = if !self.initial_pass_done {
            if self.pending_search_scroll_to.is_some() {
                ui.ctx().request_repaint();
            }
            None
        } else if let Some(target) = self.pending_search_scroll_to.take() {
            let row_fallback = ui.text_style_height(&egui::TextStyle::Body);
            let provisional = provisional_height(&self.heights, row_fallback);
            let extra_per_message = ui.spacing().item_spacing.y + MESSAGE_GAP;
            slot_offsets(&self.heights, provisional, extra_per_message)
                .get(target)
                .copied()
        } else {
            None
        };

        let heights = &mut self.heights;
        let initial_pass_done = &mut self.initial_pass_done;
        let message_match_start = &self.search_cache.message_match_start;

        let mut scroll_area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(stick_to_bottom)
            .id_salt(("transcript-viewer-scroll", self.generation));
        if let Some(offset) = search_scroll_offset {
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }
        scroll_area.show_viewport(ui, |ui, viewport| {
            if !*initial_pass_done {
                // 이 대화를 연 뒤 첫 프레임: **가상화하지 않고 지금까지 해오던
                // 대로 전부 배치한다.** 높이를 하나도 안 잰 상태에서 잠정 평균으로
                // stick_to_bottom·focus scroll_to_rect의 착지 지점을 계산하면,
                // 실제 높이가 드러나는 대로 착지가 여러 프레임에 걸쳐 움직이는
                // 꼴이 된다 — 그게 바로 사용자가 겪은 깜빡임이다(2026-08-18
                // 보고). 여기서는 타협하지 않는다: 착지는 항상 실제로 배치된
                // rect로만 한다. 대신 그 대가로 모든 메시지 높이를 이 한
                // 프레임에서 실측해 캐시에 채워 두고, 다음 프레임부터는 그
                // 캐시가 이미 정확하므로 가상화해도 흔들리지 않는다.
                for (index, message) in messages.iter().enumerate() {
                    let highlighted = highlight_range.as_ref().is_some_and(|r| r.contains(&index));
                    let local_active = local_active_in_message(message_match_start, index, active);
                    let rect = render_message(
                        ui,
                        message,
                        tokens,
                        catalog,
                        highlighted,
                        query,
                        local_active,
                    );
                    heights[index] = Some(rect.height());
                    if scroll_to == Some(index) {
                        ui.scroll_to_rect(rect, Some(egui::Align::TOP));
                    }
                }
                *initial_pass_done = true;
                return;
            }

            // 두 번째 프레임부터: 캐시된 높이의 누적합으로 뷰포트에 걸치는
            // 구간만 배치한다(+ overscan). 잠정 높이(미측정 평균)는 방어적으로
            // 남겨 둘 뿐 — 첫 프레임에서 이미 전부 실측했으므로 실전에서는
            // 거의 쓰이지 않는다.
            let row_fallback = ui.text_style_height(&egui::TextStyle::Body);
            let provisional = provisional_height(heights, row_fallback);
            let extra_per_message = ui.spacing().item_spacing.y + MESSAGE_GAP;
            let offsets = slot_offsets(heights, provisional, extra_per_message);
            let total_height = offsets.last().copied().unwrap_or(0.0);
            ui.set_height(total_height);

            let range = visible_range(&offsets, viewport.min.y..viewport.max.y, OVERSCAN_MESSAGES);
            if range.is_empty() {
                return;
            }

            let y_min = ui.max_rect().top() + offsets[range.start];
            let y_max = ui.max_rect().top() + offsets[range.end];
            let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y_min..=y_max);

            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |viewport_ui| {
                // 배치 안 하는 앞뒤 메시지도 "widget이 있었다"고 셈해야 스크롤
                // 도중 같은 메시지의 auto id가 프레임마다 안 바뀐다(show_rows와
                // 같은 관례) — 선택 가능 텍스트의 커서/선택 상태가 그 위에 걸려
                // 있다.
                viewport_ui.skip_ahead_auto_ids(range.start);
                for index in range.clone() {
                    let message = &messages[index];
                    let highlighted = highlight_range.as_ref().is_some_and(|r| r.contains(&index));
                    let local_active = local_active_in_message(message_match_start, index, active);
                    let measured = render_message(
                        viewport_ui,
                        message,
                        tokens,
                        catalog,
                        highlighted,
                        query,
                        local_active,
                    )
                    .height();
                    if heights[index] != Some(measured) {
                        heights[index] = Some(measured);
                        // 값이 바뀌었으니(예: 폭이 바뀌어 줄바꿈이 달라졌다) 다음
                        // 프레임에 새 누적합을 반영한다.
                        viewport_ui.ctx().request_repaint();
                    }
                }
            });
        });
    }
}

/// 메시지 프레임과 다음 메시지 사이에 두는 여백. `render_message`의 `ui.add_space`
/// 호출과 누적합 계산이 이 상수를 공유해야 어긋나지 않는다.
const MESSAGE_GAP: f32 = 4.0;

/// 뷰포트 앞뒤로 이만큼 메시지를 더 배치해 스크롤 중 빈칸이 보이지 않게 한다.
const OVERSCAN_MESSAGES: usize = 4;

/// 아직 안 잰 메시지의 잠정 높이 — 이미 측정된 높이들의 평균, 하나도 없으면
/// `fallback`(한 줄 높이). 측정되는 대로 이 평균도 실측값 쪽으로 수렴한다.
fn provisional_height(heights: &[Option<f32>], fallback: f32) -> f32 {
    let (sum, count) = heights
        .iter()
        .flatten()
        .fold((0.0_f32, 0usize), |(sum, count), height| {
            (sum + height, count + 1)
        });
    if count == 0 {
        fallback
    } else {
        sum / count as f32
    }
}

/// 메시지별 "슬롯 높이"(프레임 높이 + 항목 간격 + [`MESSAGE_GAP`])의 누적합. 길이는
/// `heights.len() + 1`이고 `offsets[k]`는 메시지 0..k를 배치했을 때 소비하는 전체
/// 높이(egui의 자동 item_spacing까지 포함) — 이 값으로 각 메시지의 절대 y 오프셋과
/// 전체 콘텐츠 높이를 정확히 맞춘다.
fn slot_offsets(heights: &[Option<f32>], provisional: f32, extra_per_message: f32) -> Vec<f32> {
    let mut offsets = Vec::with_capacity(heights.len() + 1);
    offsets.push(0.0);
    let mut acc = 0.0;
    for height in heights {
        acc += height.unwrap_or(provisional) + extra_per_message;
        offsets.push(acc);
    }
    offsets
}

/// 뷰포트 y범위(overscan 포함)와 겹치는 메시지 인덱스 구간을 누적합에서 찾는다.
/// `offsets[i]..offsets[i+1]`이 메시지 i가 차지하는 구간이라는 전제로 이분 탐색한다.
fn visible_range(
    offsets: &[f32],
    viewport: std::ops::Range<f32>,
    overscan: usize,
) -> std::ops::Range<usize> {
    let total = offsets.len().saturating_sub(1);
    if total == 0 || viewport.end <= viewport.start {
        return 0..0;
    }
    let tight_start = offsets
        .partition_point(|offset| *offset <= viewport.start)
        .saturating_sub(1)
        .min(total - 1);
    let tight_end = offsets
        .partition_point(|offset| *offset < viewport.end)
        .clamp(tight_start + 1, total);
    let start = tight_start.saturating_sub(overscan);
    let end = (tight_end + overscan).min(total);
    start..end
}

/// `query`로 `messages` 전체를 한 번 훑어 메시지별 일치 시작 인덱스·총 일치 수·
/// 잘림 여부를 센다. [`crate::ui::aux_search::MAX_AUX_MATCHES`]에 닿으면 그 뒤
/// 메시지는 더 스캔하지 않는다(스펙 "매칭 규칙", diff_viewer.rs `build_search_cache`와
/// 같은 관례). 빈 질의는 일치 없음과 같다(`find_matches`가 그렇게 처리해 여기서
/// 따로 분기하지 않는다).
fn build_search_cache(
    messages: &[crate::agent_transcript::ConversationMessage],
    query: &str,
) -> SearchCache {
    let mut message_match_start = Vec::with_capacity(messages.len() + 1);
    let mut total = 0usize;
    let mut truncated = false;
    for message in messages {
        message_match_start.push(total);
        if truncated {
            continue;
        }
        let m = crate::ui::aux_search::find_matches(&message.text, query);
        let remaining = crate::ui::aux_search::MAX_AUX_MATCHES - total;
        total += m.ranges.len().min(remaining);
        if total >= crate::ui::aux_search::MAX_AUX_MATCHES {
            truncated = true;
        }
    }
    message_match_start.push(total);
    SearchCache {
        query: query.to_owned(),
        message_match_start,
        total,
        truncated,
    }
}

/// `message_match_start`(길이 `messages.len() + 1`, [`build_search_cache`] 산출물)에서
/// 전역 일치 인덱스 `active`를 담은 메시지를 찾는다. `active`가 총 일치 수 밖이면
/// `None`(예: 캐시가 아직 새 활성 인덱스를 못 따라온 경계 프레임).
fn message_for_active_match(message_match_start: &[usize], active: usize) -> Option<usize> {
    let &total = message_match_start.last()?;
    if active >= total {
        return None;
    }
    let starts = &message_match_start[..message_match_start.len() - 1];
    let idx = starts.partition_point(|&start| start <= active);
    Some(idx.saturating_sub(1))
}

/// 메시지 `index` 안에서 전역 활성 일치 `active`가 로컬로 몇 번째 일치인지. 그
/// 메시지 안에 없으면(다른 메시지 소관이거나 애초에 일치가 없으면) `None` —
/// [`highlighted_job`](crate::ui::aux_search::highlighted_job)의 `active` 인자에
/// 그대로 넘긴다. 부트스트랩·가상화 두 렌더 경로가 같은 계산을 반복하므로 여기
/// 하나로 뽑아 뒀다(diff_viewer.rs는 렌더 경로가 하나뿐이라 인라인으로 충분했다).
fn local_active_in_message(
    message_match_start: &[usize],
    index: usize,
    active: Option<usize>,
) -> Option<usize> {
    let start = message_match_start.get(index).copied()?;
    let end = message_match_start.get(index + 1).copied().unwrap_or(start);
    active
        .filter(|a| *a >= start && *a < end)
        .map(|a| a - start)
}

/// 검색 일치 배경 — designall::tokens에서만 고른다(하드코딩 금지, 라이트/다크 둘 다
/// 성립). diff_viewer.rs의 같은 이름 함수와 같은 공식이다(warning·accent, 반투명
/// gamma_multiply) — 이력·Git 두 본문의 검색 강조가 같은 "느낌"이어야 한다.
/// 반투명이라 밑에 깔린 역할별 배경(`role_background`)·초점 강조
/// (`message_background`)가 그대로 비쳐 세 겹이 서로 지우지 않는다.
fn search_match_bg(tokens: crate::ui::designall::Tokens) -> egui::Color32 {
    tokens.warning.gamma_multiply(0.4)
}

/// 활성 일치 배경 — 나머지 일치([`search_match_bg`])와 다른 색이어야 ↑↓가 어디로
/// 갔는지 눈에 띈다(스펙 "우측 본문 강조·이동"). 불투명도를 더 높여 확실히
/// 두드러지게 — diff_viewer.rs와 같은 공식.
fn search_active_match_bg(tokens: crate::ui::designall::Tokens) -> egui::Color32 {
    tokens.accent.gamma_multiply(0.6)
}

fn render_message(
    ui: &mut egui::Ui,
    message: &crate::agent_transcript::ConversationMessage,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
    highlighted: bool,
    search_query: &str,
    search_active_local: Option<usize>,
) -> egui::Rect {
    let mut frame = egui::Frame::NONE
        .fill(message_background(tokens, message.role, highlighted))
        .inner_margin(egui::Margin::symmetric(8, 4));
    if highlighted {
        // 배경 섞기만으로는 라이트 테마에서 `content_canvas`와 `selected_background`가
        // 원래 가깝다(두 표면 다 "물러난 면"이라 채도 차이가 작다) — 그 자리는
        // work_history.rs의 펼친 카드가 이미 쓰는 관례(선택 배경 + accent 테두리)로
        // 메운다. 그러면 라이트/다크 어느 쪽이든 테두리가 강조를 확실히 보여준다.
        frame = frame.stroke(egui::Stroke::new(1.0, tokens.accent));
    }
    let response = frame.show(ui, |ui| {
        let label = match message.role {
            crate::agent_transcript::ConversationRole::User => catalog.t("history.role.user", &[]),
            crate::agent_transcript::ConversationRole::Assistant => {
                catalog.t("history.role.agent", &[])
            }
        };
        ui.label(egui::RichText::new(label).small().weak());
        // 검색은 메시지 본문(message.text)에서만 찾는다 — 역할 라벨은 검색 대상이
        // 아니다. 빈 질의(검색 안 함)나 이 메시지에 일치가 없으면 예전처럼 그린다
        // (매 프레임 LayoutJob을 새로 짓지 않는다, diff_viewer.rs의 `row_matches`와
        // 같은 관례).
        let text_matches = (!search_query.is_empty())
            .then(|| crate::ui::aux_search::find_matches(&message.text, search_query))
            .filter(|m| !m.ranges.is_empty());
        match text_matches {
            None => {
                ui.add(
                    egui::Label::new(message.text.as_str())
                        .wrap()
                        .selectable(true),
                );
            }
            Some(matches) => {
                // color는 PLACEHOLDER — 위젯이 그릴 때 현재 텍스트 색으로
                // 바꿔치기한다(diff_viewer.rs `diff_row_job`과 같은 관례,
                // TextFormat::default().color는 GRAY라 그대로 두면 다른 라벨과
                // 색이 어긋난다).
                let base = egui::TextFormat {
                    font_id: egui::TextStyle::Body.resolve(ui.style()),
                    color: egui::Color32::PLACEHOLDER,
                    ..Default::default()
                };
                let job = crate::ui::aux_search::highlighted_job(
                    message.text.as_str(),
                    &matches,
                    search_active_local,
                    base,
                    search_match_bg(tokens),
                    search_active_match_bg(tokens),
                );
                ui.add(egui::Label::new(job).wrap().selectable(true));
            }
        }
    });
    ui.add_space(MESSAGE_GAP);
    response.response.rect
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
    if truncated
        && messages
            .first()
            .is_some_and(|first| focus_offset < first.offset)
    {
        return None;
    }
    let start = messages.iter().position(|m| m.offset >= focus_offset)?;
    let end = messages[start + 1..]
        .iter()
        .position(|m| m.role == crate::agent_transcript::ConversationRole::User)
        .map_or(messages.len(), |relative| start + 1 + relative);
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 빈_뷰어는_안내_문구를_보여준다() {
        let mut ui = TranscriptViewerUi::default();
        assert!(ui.is_empty());
        ui.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation::default()),
            None,
        );
        assert!(
            !ui.is_empty(),
            "열었으면 비어 있지 않다 — 대화가 0건이어도 상태는 열림이다"
        );
    }

    #[test]
    fn 로딩_중이면_빈_상태가_아니다() {
        let mut ui = TranscriptViewerUi::default();
        ui.set_loading();
        assert!(
            !ui.is_empty(),
            "로딩도 열림이다 — 대화가 아직 안 왔을 뿐이다"
        );
    }

    fn harness_for(
        viewer: TranscriptViewerUi,
    ) -> egui_kittest::Harness<'static, TranscriptViewerUi> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut TranscriptViewerUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.render(ui, &catalog, None);
            },
            viewer,
        )
    }

    /// 보조 검색이 켜진 채 렌더하는 하네스 — `search`는 (질의, 활성 인덱스).
    fn harness_with_search(
        viewer: TranscriptViewerUi,
        query: &'static str,
        active: usize,
    ) -> egui_kittest::Harness<'static, TranscriptViewerUi> {
        egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut TranscriptViewerUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.render(ui, &catalog, Some((query, active)));
            },
            viewer,
        )
    }

    fn message(
        role: crate::agent_transcript::ConversationRole,
        text: &str,
        offset: u64,
    ) -> crate::agent_transcript::ConversationMessage {
        crate::agent_transcript::ConversationMessage {
            role,
            text: text.to_owned(),
            at: None,
            offset,
        }
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
        viewer.set_conversation(
            Err(crate::agent_transcript::TranscriptViewError::NotFound),
            None,
        );
        let mut harness = harness_for(viewer);
        harness.run();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        harness.get_by_label(&catalog.t("history.transcript.not_found", &[]));
    }

    #[test]
    fn kittest_읽기_실패는_에러_문구를_보여준다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Err(crate::agent_transcript::TranscriptViewError::ReadFailed),
            None,
        );
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
                messages: vec![message(
                    crate::agent_transcript::ConversationRole::User,
                    "안녕",
                    0,
                )],
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
                    message(
                        crate::agent_transcript::ConversationRole::User,
                        "질문입니다",
                        0,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::Assistant,
                        "답변입니다",
                        100,
                    ),
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

    /// 예전에는 `show_rows` 가상화라 화면 밖 메시지를 아예 만들지 않았는데, 그건
    /// **모든 행이 같은 높이**라는 가정 위에서만 성립해서 깨졌다(2026-08-18 사용자
    /// 보고 — 스크롤이 깜빡이고 위로 바로 안 올라갔다). 그 다음 커밋은 정확성을 위해
    /// 가상화를 버리고 전부 배치했다. 이번 계약은 그 둘을 합친다: 측정 높이 캐시 +
    /// 누적합으로 뷰포트에 걸치는 구간만 배치하되(2프레임째부터), 값은 **실측**이라
    /// 어긋나지 않는다. 첫 프레임(부트스트랩)은 여전히 전부 배치해 모든 높이를 재고
    /// stick_to_bottom을 정확히 착지시키므로, 그 프레임에서 실행을 멈추면(harness를
    /// 한 번만 돌리면) 가상화가 시작되기 전 상태만 보게 된다 — 그래서 `run()`을 두 번
    /// 불러 가상화된 두 번째 프레임의 결과를 확인한다.
    #[test]
    fn kittest_뷰포트_밖_메시지는_배치되지_않고_최신_메시지는_보인다() {
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
            Ok(crate::agent_transcript::TranscriptConversation {
                messages,
                truncated: false,
            }),
            None,
        );
        let mut harness = harness_for(viewer);
        harness.run(); // 부트스트랩: 전부 배치해 높이를 재고 stick_to_bottom을 착지시킨다.
        harness.run(); // 가상화 프레임: 착지된 뷰포트 기준으로 걸치는 구간만 배치한다.

        assert!(
            harness.query_by_label("메시지 199").is_some(),
            "스티키 하단이라 가장 최근 메시지는 계속 보여야 한다"
        );
        assert!(
            harness.query_by_label("메시지 0").is_none(),
            "가상화되면 뷰포트에서 한참 벗어난(overscan 밖) 메시지는 배치되지 않는다 — \
             그래야 스크롤 프레임 비용이 대화 길이가 아니라 뷰포트 크기로 유계가 된다"
        );
    }

    /// 이 상한은 더 이상 "매 스크롤 프레임" 비용을 지키지 않는다 — 가상화가 그건
    /// 뷰포트+overscan으로 이미 유계로 만든다. 다만 대화를 처음 여는 **부트스트랩
    /// 프레임**(아직 높이를 하나도 못 잰 첫 프레임, `initial_pass_done == false`)은
    /// 착지 정확성을 위해 지금도 전부 배치한다 — 그 한 프레임의 비용은 여전히 이
    /// 상한에 걸려 있다.
    #[test]
    fn 메시지_수_상한이_부트스트랩_프레임_비용을_막는다() {
        assert_eq!(crate::agent_transcript::CONVERSATION_MESSAGES_MAX, 200);
    }

    #[test]
    fn 역할별_배경은_서로_다르고_토큰에서만_고른다() {
        let tokens = crate::ui::designall::DARK;
        let user_bg = role_background(tokens, crate::agent_transcript::ConversationRole::User);
        let agent_bg =
            role_background(tokens, crate::agent_transcript::ConversationRole::Assistant);
        assert_ne!(user_bg, agent_bg);
        assert_eq!(
            agent_bg, tokens.content_canvas,
            "에이전트는 페이지 바닥 톤을 그대로 쓴다"
        );
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
            message(
                crate::agent_transcript::ConversationRole::User,
                "턴1 지시",
                0,
            ),
            message(
                crate::agent_transcript::ConversationRole::Assistant,
                "턴1 답",
                100,
            ),
            message(
                crate::agent_transcript::ConversationRole::User,
                "턴2 지시",
                200,
            ),
            message(
                crate::agent_transcript::ConversationRole::Assistant,
                "턴2 답",
                300,
            ),
        ];
        assert_eq!(
            focus_range(&messages, false, 0),
            Some(0..2),
            "다음 User 직전까지"
        );
        assert_eq!(
            focus_range(&messages, false, 200),
            Some(2..4),
            "마지막 턴은 끝까지"
        );
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
        let messages = vec![message(
            crate::agent_transcript::ConversationRole::User,
            "최근",
            900,
        )];
        // 잘리지 않았다면(파일 전체가 창 안) `offset >= focus_offset`으로 뒤쪽
        // 메시지를 잡는 게 옳다 — 턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있어서다.
        assert_eq!(
            focus_range(&messages, false, 100),
            Some(0..1),
            "안 잘렸으면 뒤쪽을 잡는다"
        );
        assert_eq!(
            focus_range(&messages, false, 1_000),
            None,
            "그보다 뒤는 없다"
        );
        // 잘렸다면(스냅샷 창이 파일 시작을 못 담았다) `focus_offset`이 창의 첫
        // 메시지보다 앞이라는 건 그 턴이 창 밖(더 앞)으로 밀려났다는 뜻이다 — 엉뚱한
        // (더 최근) 메시지를 그 턴인 것처럼 강조하면 안 되므로 못 찾은 것으로 취급한다.
        assert_eq!(
            focus_range(&messages, true, 100),
            None,
            "잘렸으면 창 앞의 턴은 못 찾는다"
        );
        assert_eq!(
            focus_range(&messages, true, 1_000),
            None,
            "그보다 뒤는 잘렸어도 여전히 없다"
        );
    }

    #[test]
    fn kittest_초점을_못_찾으면_안내를_띄운다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        // 스냅샷 창(꼬리 4MB) 앞이라 이 오프셋을 가진 메시지가 없다 — 조용히 최신을
        // 보여주면 사용자는 그게 그 턴인 줄 안다.
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![message(
                    crate::agent_transcript::ConversationRole::User,
                    "최근",
                    900,
                )],
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
                    message(
                        crate::agent_transcript::ConversationRole::User,
                        "턴1 지시",
                        0,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::Assistant,
                        "턴1 답",
                        100,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::User,
                        "턴2 지시",
                        200,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::Assistant,
                        "턴2 답",
                        300,
                    ),
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
            harness
                .query_by_label(&catalog.t("history.transcript.focus_missing", &[]))
                .is_none(),
            "찾은 턴이니 안내가 없어야 한다"
        );
        harness.get_by_label("턴2 지시");
        harness.get_by_label("턴2 답");
    }

    #[test]
    fn 대화가_바뀌면_높이_캐시와_배치_상태가_초기화된다() {
        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation::default()),
            None,
        );
        // 이전 대화에서 이미 부트스트랩(전부 배치)을 마치고 높이를 재 뒀다고 가정한다.
        viewer.heights = vec![Some(10.0), Some(20.0)];
        viewer.initial_pass_done = true;

        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation::default()),
            None,
        );

        assert!(
            viewer.heights.is_empty(),
            "옛 대화의 높이가 남으면 새 대화에서 스크롤이 통째로 어긋난다"
        );
        assert!(
            !viewer.initial_pass_done,
            "새 대화는 다시 부트스트랩부터 시작해야 착지가 정확하다"
        );
    }

    #[test]
    fn 잠정_높이는_측정된_것들의_평균이고_없으면_한_줄_높이다() {
        assert_eq!(
            provisional_height(&[None, None], 12.0),
            12.0,
            "하나도 안 쟀으면 폴백"
        );
        assert_eq!(
            provisional_height(&[Some(10.0), None, Some(20.0)], 12.0),
            15.0,
            "측정된 것만 평균"
        );
        assert_eq!(provisional_height(&[], 12.0), 12.0, "메시지가 없어도 폴백");
    }

    #[test]
    fn 누적합은_측정_높이와_잠정_높이를_섞어_계산한다() {
        // 메시지 0·2는 실측(10, 20), 메시지 1은 미측정(잠정 5) — extra_per_message=1.
        let offsets = slot_offsets(&[Some(10.0), None, Some(20.0)], 5.0, 1.0);
        assert_eq!(offsets, vec![0.0, 11.0, 17.0, 38.0]);
    }

    #[test]
    fn 뷰포트에_걸치는_구간만_찾는다() {
        // 메시지 5개, 각각 높이 10 (간격 없음): 경계가 0,10,20,30,40,50.
        let offsets: Vec<f32> = (0u16..=5).map(|i| f32::from(i) * 10.0).collect();
        // 뷰포트 [12,28)은 메시지1([10,20))과 메시지2([20,30))에 걸친다.
        assert_eq!(visible_range(&offsets, 12.0..28.0, 0), 1..3);
    }

    #[test]
    fn overscan은_앞뒤로_더_배치하되_경계를_벗어나지_않는다() {
        let offsets: Vec<f32> = (0u16..=5).map(|i| f32::from(i) * 10.0).collect();
        assert_eq!(
            visible_range(&offsets, 12.0..28.0, 1),
            0..4,
            "overscan 1이면 앞뒤로 하나씩 더"
        );
        assert_eq!(
            visible_range(&offsets, 12.0..28.0, 10),
            0..5,
            "overscan이 아무리 커도 [0, 메시지_수)를 못 벗어난다"
        );
    }

    #[test]
    fn 미측정_메시지가_섞여도_배치_구간을_정확히_찾는다() {
        // 메시지 0·2는 실측 20, 메시지 1은 미측정(잠정 10) — offsets = [0,20,30,50].
        let offsets = slot_offsets(&[Some(20.0), None, Some(20.0)], 10.0, 0.0);
        assert_eq!(offsets, vec![0.0, 20.0, 30.0, 50.0]);
        // 뷰포트가 잠정 높이 구간([20,30))만 걸치면 그 메시지 하나만 잡혀야 한다.
        assert_eq!(visible_range(&offsets, 22.0..28.0, 0), 1..2);
    }

    #[test]
    fn 빈_대화의_배치_구간은_비어_있다() {
        let offsets = slot_offsets(&[], 12.0, 4.0);
        assert_eq!(visible_range(&offsets, 0.0..100.0, 4), 0..0);
    }

    #[test]
    fn 메시지가_하나면_구간은_항상_그_메시지_하나다() {
        let offsets = slot_offsets(&[Some(10.0)], 12.0, 4.0);
        assert_eq!(visible_range(&offsets, 0.0..5.0, 0), 0..1);
        assert_eq!(
            visible_range(&offsets, 200.0..300.0, 0),
            0..1,
            "뷰포트가 콘텐츠보다 훨씬 아래라도 마지막(=유일한) 메시지로 클램프된다"
        );
    }

    // ── 보조 검색(2026-08-18 계획 Task 5) ──────────────────────────────────

    #[test]
    fn 검색_캐시는_메시지별_누적_일치수를_센다() {
        let messages = vec![
            message(
                crate::agent_transcript::ConversationRole::User,
                "cab cab",
                0,
            ),
            message(
                crate::agent_transcript::ConversationRole::Assistant,
                "no match",
                100,
            ),
            message(
                crate::agent_transcript::ConversationRole::User,
                "cab cab cab",
                200,
            ),
        ];
        let cache = build_search_cache(&messages, "cab");
        assert_eq!(
            cache.message_match_start,
            vec![0, 2, 2, 5],
            "2 + 0 + 3, sentinel 포함"
        );
        assert_eq!(cache.total, 5);
        assert!(!cache.truncated);
    }

    #[test]
    fn 검색_캐시는_상한에서_멈추고_잘림을_표시한다() {
        let messages = vec![
            message(
                crate::agent_transcript::ConversationRole::Assistant,
                &"a".repeat(400),
                0,
            ),
            message(
                crate::agent_transcript::ConversationRole::Assistant,
                &"a".repeat(400),
                100,
            ),
        ];
        let cache = build_search_cache(&messages, "a");
        // 메시지 0에서 400개를 다 세고, 메시지 1은 남은 100개(500 - 400)만 세고 멈춘다.
        assert_eq!(cache.message_match_start, vec![0, 400, 500]);
        assert_eq!(cache.total, crate::ui::aux_search::MAX_AUX_MATCHES);
        assert!(cache.truncated);
    }

    #[test]
    fn 전역_활성_인덱스로_메시지를_찾는다() {
        let starts = vec![0, 2, 2, 5];
        assert_eq!(message_for_active_match(&starts, 0), Some(0));
        assert_eq!(message_for_active_match(&starts, 1), Some(0));
        assert_eq!(
            message_for_active_match(&starts, 2),
            Some(2),
            "메시지 1은 일치가 없어 건너뛴다"
        );
        assert_eq!(message_for_active_match(&starts, 4), Some(2));
        assert_eq!(
            message_for_active_match(&starts, 5),
            None,
            "총계 밖은 못 찾는다(캐시가 못 따라온 경계 프레임)"
        );
        assert_eq!(message_for_active_match(&starts, 100), None);
    }

    #[test]
    fn 메시지_안의_로컬_활성_인덱스를_계산한다() {
        let starts = vec![0, 2, 2, 5];
        assert_eq!(local_active_in_message(&starts, 0, Some(0)), Some(0));
        assert_eq!(local_active_in_message(&starts, 0, Some(1)), Some(1));
        assert_eq!(
            local_active_in_message(&starts, 0, Some(2)),
            None,
            "전역 인덱스 2는 메시지 2 소관이라 메시지 0에서는 없다"
        );
        assert_eq!(local_active_in_message(&starts, 2, Some(2)), Some(0));
        assert_eq!(local_active_in_message(&starts, 2, Some(4)), Some(2));
        assert_eq!(
            local_active_in_message(&starts, 0, None),
            None,
            "활성 일치 자체가 없으면 없다"
        );
        assert_eq!(
            local_active_in_message(&starts, 99, Some(0)),
            None,
            "범위 밖 메시지 인덱스"
        );
    }

    #[test]
    fn 검색_강조_배경은_서로_다르고_역할_배경과도_구분된다() {
        // designall::tokens에서만 고른 값이라 라이트/다크 둘 다 자동으로 성립해야
        // 한다(하드코딩 금지). 역할 배경·활성 일치 배경과도 구분돼야 세 겹이 겹칠
        // 때 뭉개지지 않는다.
        for tokens in [crate::ui::designall::DARK, crate::ui::designall::LIGHT] {
            let match_bg = search_match_bg(tokens);
            let active_bg = search_active_match_bg(tokens);
            assert_ne!(
                match_bg, active_bg,
                "활성 일치는 나머지와 다른 색이어야 ↑↓가 보인다"
            );
            for role in [
                crate::agent_transcript::ConversationRole::User,
                crate::agent_transcript::ConversationRole::Assistant,
            ] {
                let role_bg = role_background(tokens, role);
                assert_ne!(match_bg, role_bg, "검색 강조가 역할 배경에 묻히면 안 된다");
                assert_ne!(
                    active_bg, role_bg,
                    "활성 검색 강조가 역할 배경에 묻히면 안 된다"
                );
            }
        }
    }

    #[test]
    fn kittest_검색_총계는_render_이후_캐시에서_읽힌다() {
        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![
                    message(
                        crate::agent_transcript::ConversationRole::User,
                        "cab cab",
                        0,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::Assistant,
                        "no match",
                        100,
                    ),
                    message(
                        crate::agent_transcript::ConversationRole::User,
                        "cab cab cab",
                        200,
                    ),
                ],
                truncated: false,
            }),
            None,
        );
        assert_eq!(
            viewer.search_summary(),
            (0, false),
            "render 전에는 아직 캐시가 없다"
        );

        let mut harness = harness_with_search(viewer, "cab", 0);
        harness.run();
        assert_eq!(
            harness.state().search_summary(),
            (5, false),
            "2 + 0 + 3 = 5, 안 잘림"
        );
    }

    #[test]
    fn kittest_검색_강조는_메시지_라벨을_그대로_유지한다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages: vec![message(
                    crate::agent_transcript::ConversationRole::Assistant,
                    "billing widget 안내",
                    0,
                )],
                truncated: false,
            }),
            None,
        );
        let mut harness = harness_with_search(viewer, "widget", 0);
        harness.run();

        // LayoutJob으로 강조를 그려도 접근성 라벨(=보이는 전체 글자)은 그대로다 —
        // 일치 구간만 배경이 다를 뿐 글자 자체는 안 바뀐다.
        harness.get_by_label("billing widget 안내");
    }

    /// 활성 검색 일치가 있는 메시지가 초기 뷰포트(stick_to_bottom이 착지시키는
    /// 맨 아래) 밖에 있으면, 그 메시지로 스크롤이 실제로 옮겨져야 한다. 이 검증은
    /// `slot_offsets` 기반 오프셋 스크롤(부트스트랩 다음 프레임에 적용)이 실제로
    /// 동작하는지 확인한다 — rect 기반 `scroll_to_rect`만으로는 아직 배치되지 않은
    /// 메시지를 못 겨냥한다(2026-08-18 계획 Task 5 지침).
    #[test]
    fn kittest_활성_검색_일치로_스크롤한다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = TranscriptViewerUi::default();
        let messages = (0..200)
            .map(|index| {
                let text = if index == 50 {
                    "고유표식".to_owned()
                } else {
                    format!("메시지 {index}")
                };
                message(
                    crate::agent_transcript::ConversationRole::Assistant,
                    &text,
                    index as u64 * 100,
                )
            })
            .collect();
        viewer.set_conversation(
            Ok(crate::agent_transcript::TranscriptConversation {
                messages,
                truncated: false,
            }),
            None,
        );
        let mut harness = harness_with_search(viewer, "고유표식", 0);
        harness.run(); // 부트스트랩: 전부 배치해 높이를 재고, 검색 스크롤 대상을
        // 계산해 둔다 — 이 프레임에서는 아직 소비하지 않는다(위
        // `pending_search_scroll_to` 문서 참고).
        harness.run(); // 가상화 프레임: 이제 슬롯 오프셋 기반으로 그 메시지까지
        // 스크롤한다.

        assert!(
            harness.query_by_label("고유표식").is_some(),
            "활성 검색 일치가 있는 메시지로 스크롤해야 한다"
        );
        assert!(
            harness.query_by_label("메시지 199").is_none(),
            "검색 스크롤이 stick_to_bottom 착지 위치를 밀어내고 그 메시지 쪽으로 옮겨야 한다"
        );
    }
}
