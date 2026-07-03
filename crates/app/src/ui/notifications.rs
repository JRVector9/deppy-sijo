//! 내부 알림 센터 (설계문서 PR-13). SessionStatusChanged를 받아
//! waiting/needs-approval/error/done을 알림으로 만들고, OS 알림도 띄운다.
//! 항목 클릭 시 해당 세션의 pane으로 focus 이동 (완료 기준: session focus).

use runtime::{MuxSnapshot, RuntimeClient, RuntimeCommand, SessionId, SessionStatus};

pub struct NotificationsUi {
    open: bool,
    items: Vec<NotificationItem>,
}

struct NotificationItem {
    session: SessionId,
    status: SessionStatus,
    title: String,
    read: bool,
}

impl NotificationsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            items: Vec::new(),
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            // 센터를 열면 모두 읽음 처리
            for item in &mut self.items {
                item.read = true;
            }
        }
    }

    /// 안 읽은 항목 수 — 항목별 read 플래그에서 파생 (pruning에 자동 정합).
    pub fn unread(&self) -> usize {
        self.items.iter().filter(|item| !item.read).count()
    }

    /// 상태 변경을 알림으로 만든다. Running 복귀는 알리지 않는다.
    /// `title`은 tab/세션 제목 (mux 스냅샷에서 조회).
    pub fn on_status(&mut self, session: SessionId, status: SessionStatus, title: &str) {
        let label = match status {
            SessionStatus::Waiting => "입력 대기",
            SessionStatus::NeedsApproval => "승인 필요",
            SessionStatus::Error => "오류",
            SessionStatus::Done => "완료",
            SessionStatus::Running => return, // 진행 재개는 알림 아님
        };
        #[cfg(not(test))] // 테스트에서 실제 OS 알림을 띄우지 않는다
        platform::notify(&format!("{label}: {title}"), title);
        let _ = label;
        self.items.push(NotificationItem {
            session,
            status,
            title: title.to_owned(),
            read: self.open, // 센터가 열려 있으면 바로 읽음
        });
        // 최근 100개만 유지
        if self.items.len() > 100 {
            let cut = self.items.len() - 100;
            self.items.drain(..cut);
        }
    }

    /// 세션 종료 알림 (완료 기준: done/error). exit code로 Done/Error 결정.
    /// status detector가 방금(직전 항목으로) 같은 결과를 냈으면 중복 발화하지 않되,
    /// 그 사이에 다른 항목(Waiting/재개 등)이 끼었으면 exit은 새 알림으로 낸다.
    pub fn on_exit(&mut self, session: SessionId, exit_code: Option<u32>, title: &str) {
        let status = if exit_code == Some(0) {
            SessionStatus::Done
        } else {
            SessionStatus::Error
        };
        // 이 세션의 "마지막 항목"이 같은 결과 상태일 때만 중복으로 본다 —
        // 다른 세션의 알림이 사이에 끼어도 판정이 흔들리지 않게 세션 기준으로 찾는다.
        // (Running은 알림 항목이 아니므로 애초에 items에 없다 — 중간 재개는 다른
        //  상태 항목으로 남고, 그 뒤 exit은 정상적으로 새 알림이 된다)
        let dup = self
            .items
            .iter()
            .rev()
            .find(|item| item.session == session)
            .is_some_and(|item| item.status == status);
        if !dup {
            self.on_status(session, status, title);
        }
    }

    /// 세션이 사라지면 관련 알림 정리 (unread는 items에서 파생되어 자동 정합).
    pub fn retain_sessions(&mut self, alive: &[SessionId]) {
        self.items.retain(|item| alive.contains(&item.session));
    }

    /// 알림 센터를 그린다. 세션 focus 명령을 보냈으면 true (호출측이 repaint 예약).
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        client: &dyn RuntimeClient,
        mux: Option<&MuxSnapshot>,
    ) -> bool {
        if !self.open {
            return false;
        }
        let mut open = true;
        let mut focused = false;
        egui::Window::new("알림")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                if self.items.is_empty() {
                    ui.label("알림이 없습니다.");
                }
                if !self.items.is_empty() && ui.button("모두 지우기").clicked() {
                    self.items.clear();
                }
                let mut focus_session = None;
                // 최신 항목이 위로
                for item in self.items.iter().rev() {
                    let icon = status_icon(item.status);
                    if ui
                        .button(format!("{icon} {}", item.title))
                        .on_hover_text("클릭하면 해당 세션으로 이동")
                        .clicked()
                    {
                        focus_session = Some(item.session);
                    }
                }
                // 클릭한 세션의 pane을 찾아 focus (완료 기준: session focus)
                if let Some(session) = focus_session
                    && let Some(pane) = mux.and_then(|mux| pane_of_session(mux, session))
                {
                    match client.send_command(RuntimeCommand::FocusPane { pane }) {
                        Ok(()) => focused = true,
                        Err(e) => tracing::warn!("알림 focus 이동 실패: {e:#}"),
                    }
                }
            });
        self.open = open;
        focused
    }
}

fn pane_of_session(mux: &MuxSnapshot, session: SessionId) -> Option<runtime::MuxPaneId> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .map(|pane| pane.id.clone())
}

fn status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Waiting => "⏳",
        SessionStatus::NeedsApproval => "✋",
        SessionStatus::Error => "❌",
        SessionStatus::Done => "✅",
        SessionStatus::Running => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxSnapshot, PaneSnapshot, TabSnapshot};

    fn mux_with(session: SessionId, pane: runtime::MuxPaneId) -> MuxSnapshot {
        MuxSnapshot {
            tabs: vec![TabSnapshot {
                id: runtime::MuxTabId::new(),
                title: "탭".into(),
                layout: runtime::LayoutNode::Pane(pane.clone()),
                panes: vec![PaneSnapshot {
                    id: pane,
                    session_id: Some(session),
                    title: "에이전트 1".into(),
                }],
            }],
            active_tab: None,
            focused_pane: None,
        }
    }

    #[test]
    fn 세션의_pane_조회() {
        let session = SessionId(3);
        let pane = runtime::MuxPaneId::new();
        let mux = mux_with(session, pane.clone());
        assert_eq!(pane_of_session(&mux, session), Some(pane));
        assert_eq!(pane_of_session(&mux, SessionId(99)), None);
    }

    #[test]
    fn running_복귀는_알림_아님() {
        let mut n = NotificationsUi::new();
        n.on_status(SessionId(1), SessionStatus::Running, "t");
        assert_eq!(n.unread(), 0);
        n.on_status(SessionId(1), SessionStatus::Error, "t");
        assert_eq!(n.unread(), 1);
    }

    #[test]
    fn 죽은_세션_알림_정리() {
        let mut n = NotificationsUi::new();
        n.on_status(SessionId(1), SessionStatus::Done, "a");
        n.on_status(SessionId(2), SessionStatus::Done, "b");
        assert_eq!(n.unread(), 2);
        n.retain_sessions(&[SessionId(2)]);
        assert_eq!(n.items.len(), 1);
        assert_eq!(n.items[0].session, SessionId(2));
        assert_eq!(n.unread(), 1); // items에서 파생
    }

    #[test]
    fn exit_알림과_status_중복_방지() {
        let mut n = NotificationsUi::new();
        // status detector가 먼저 Done 감지 → 이후 exit(0)는 중복 발화 안 함
        n.on_status(SessionId(1), SessionStatus::Done, "a");
        n.on_exit(SessionId(1), Some(0), "a");
        assert_eq!(n.items.len(), 1);
        // regex 없는 agent: status 없이 exit만 → Done 알림 생성
        n.on_exit(SessionId(2), Some(0), "b");
        assert_eq!(
            n.items.iter().filter(|i| i.session == SessionId(2)).count(),
            1
        );
        // 비정상 종료 → Error
        n.on_exit(SessionId(3), Some(1), "c");
        assert!(matches!(
            n.items
                .iter()
                .find(|i| i.session == SessionId(3))
                .unwrap()
                .status,
            SessionStatus::Error
        ));
    }

    #[test]
    fn unread는_읽은_옛항목과_무관하게_pruning에_정합() {
        let mut n = NotificationsUi::new();
        n.on_status(SessionId(1), SessionStatus::Done, "old"); // 읽을 항목
        n.toggle(); // 열기 → 모두 읽음
        n.toggle(); // 닫기
        assert_eq!(n.unread(), 0);
        n.on_status(SessionId(2), SessionStatus::Error, "new"); // 안 읽음 1
        assert_eq!(n.unread(), 1);
        // session 2가 사라짐 → 안 읽은 항목 제거, 옛 읽은 항목(session 1)은 남아도 unread 0
        n.retain_sessions(&[SessionId(1)]);
        assert_eq!(n.unread(), 0);
    }
}
