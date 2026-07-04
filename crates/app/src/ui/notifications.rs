//! 내부 알림 센터 (설계문서 PR-13). SessionStatusChanged를 받아
//! waiting/needs-approval/error/done을 알림으로 만들고, OS 알림도 띄운다.
//! 항목 클릭 시 해당 세션의 pane으로 focus 이동 (완료 기준: session focus).

use runtime::{SessionId, SessionStatus};

pub struct NotificationsUi {
    open: bool,
    items: Vec<NotificationItem>,
}

struct NotificationItem {
    /// 어느 workspace의 세션인지 — 워커마다 SessionId가 리셋돼 충돌하므로 함께 키로 쓴다.
    workspace_id: String,
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
    pub fn on_status(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        status: SessionStatus,
        title: &str,
    ) {
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
            workspace_id: workspace_id.to_owned(),
            session,
            status,
            title: title.to_owned(),
            // 생성 시엔 항상 안 읽음. on_status는 창이 숨겨져(minimized/occluded) ui()가
            // 스킵돼도 logic()에서 호출되므로, 여기서 self.open으로 읽음 처리하면 사용자가
            // 보지 못한 background 알림이 읽음이 돼 unread 신호를 잃는다. 실제 읽음은
            // show()(=가시일 때만 호출)가 처리한다.
            read: false,
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
    pub fn on_exit(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        exit_code: Option<u32>,
        title: &str,
    ) {
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
            .find(|item| item.workspace_id == workspace_id && item.session == session)
            .is_some_and(|item| item.status == status);
        if !dup {
            self.on_status(workspace_id, session, status, title);
        }
    }

    /// workspace가 삭제되면 그 workspace의 모든 알림을 제거한다.
    pub fn prune_workspace(&mut self, workspace_id: &str) {
        self.items.retain(|item| item.workspace_id != workspace_id);
    }

    /// workspace가 Suspended(축출)되면 그 workspace의 진행형(Waiting/승인) 알림을 제거한다 —
    /// 워커가 죽어 더는 조치 불가하므로. 결과(Done/Error)는 기록이라 유지한다.
    pub fn prune_transient(&mut self, workspace_id: &str) {
        self.items.retain(|item| {
            item.workspace_id != workspace_id
                || matches!(item.status, SessionStatus::Done | SessionStatus::Error)
        });
    }

    /// 활성 workspace의 세션이 사라지면 그 workspace의 진행형 알림을 정리한다.
    /// 결과 상태(Done/Error)는 기록이라 유지하고, 다른 workspace(warm 등)의 진행형은
    /// alive를 알 수 없어 유지한다(총량은 on_status의 100개 cap으로 유계).
    pub fn retain_sessions(&mut self, active_workspace_id: &str, alive: &[SessionId]) {
        self.items.retain(|item| {
            matches!(item.status, SessionStatus::Done | SessionStatus::Error)
                || item.workspace_id != active_workspace_id
                || alive.contains(&item.session)
        });
    }

    /// 알림 센터를 그린다. 클릭한 항목의 (workspace_id, session)을 돌려준다 —
    /// 호출측(App)이 활성 workspace면 pane focus, 아니면 그 workspace로 전환한다.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<(String, SessionId)> {
        if !self.open {
            return None;
        }
        // show()는 ui()에서만 호출되고 ui()는 창이 가시일 때만 실행된다 — 즉 여기 도달했다는
        // 건 센터가 열려 있고 화면에 보인다는 뜻이므로 표시된 항목을 읽음 처리한다. (센터가
        // 열린 채 새 알림이 도착한 경우에도 다음 렌더에서 읽음 처리됨.)
        let had_unread = self.items.iter().any(|item| !item.read);
        for item in &mut self.items {
            item.read = true;
        }
        // unread 뱃지는 이 프레임 앞서(상단바) 이미 그려졌다 — 방금 읽음으로 바꿨으면
        // 다음 프레임에 뱃지가 갱신되도록 repaint를 요청한다(안 그러면 무관한 UI 활동
        // 전까지 "알림 (1)"이 남는다).
        if had_unread {
            ctx.request_repaint();
        }
        let mut open = true;
        let mut clicked = None;
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
                // 최신 항목이 위로
                for item in self.items.iter().rev() {
                    let icon = status_icon(item.status);
                    if ui
                        .button(format!("{icon} {}", item.title))
                        .on_hover_text("클릭하면 해당 workspace/세션으로 이동")
                        .clicked()
                    {
                        clicked = Some((item.workspace_id.clone(), item.session));
                    }
                }
            });
        self.open = open;
        clicked
    }
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

    const WS: &str = "ws-1";

    #[test]
    fn running_복귀는_알림_아님() {
        let mut n = NotificationsUi::new();
        n.on_status(WS, SessionId(1), SessionStatus::Running, "t");
        assert_eq!(n.unread(), 0);
        n.on_status(WS, SessionId(1), SessionStatus::Error, "t");
        assert_eq!(n.unread(), 1);
    }

    #[test]
    fn 다른_workspace의_같은_세션id는_별개_알림() {
        let mut n = NotificationsUi::new();
        // 워커마다 SessionId가 리셋되므로 (ws, session)로 구분돼야 한다
        n.on_status("ws-a", SessionId(1), SessionStatus::Done, "A작업");
        n.on_status("ws-b", SessionId(1), SessionStatus::Done, "B작업");
        assert_eq!(n.items.len(), 2);
        // 활성(ws-a) 기준 retain: ws-a의 진행형만 정리, ws-b(다른 workspace)는 유지
        n.on_status("ws-a", SessionId(2), SessionStatus::Waiting, "A대기");
        n.on_status("ws-b", SessionId(2), SessionStatus::Waiting, "B대기");
        n.retain_sessions("ws-a", &[]); // ws-a에 alive 세션 없음
        // ws-a의 Waiting(진행형)은 정리, Done(결과)은 유지, ws-b는 전부 유지
        let has = |ws: &str, sess: u64, st: SessionStatus| {
            n.items
                .iter()
                .any(|i| i.workspace_id == ws && i.session == SessionId(sess) && i.status == st)
        };
        assert!(has("ws-a", 1, SessionStatus::Done));
        assert!(!has("ws-a", 2, SessionStatus::Waiting));
        assert!(has("ws-b", 1, SessionStatus::Done));
        assert!(has("ws-b", 2, SessionStatus::Waiting));
    }

    #[test]
    fn 센터가_열려있어도_생성시엔_안읽음() {
        // 숨김 중 logic()에서 on_status가 호출될 수 있으므로 self.open으로 읽음 처리하면
        // 안 된다 — 사용자가 본 시점(show=가시)에만 읽음. 생성 시엔 항상 unread.
        let mut n = NotificationsUi::new();
        n.toggle(); // open = true
        n.on_status(WS, SessionId(1), SessionStatus::Done, "완료");
        assert_eq!(n.unread(), 1);
    }

    #[test]
    fn retain은_결과상태_유지하고_진행형만_정리() {
        let mut n = NotificationsUi::new();
        n.on_status(WS, SessionId(1), SessionStatus::Done, "a"); // 결과 → 유지
        n.on_status(WS, SessionId(2), SessionStatus::NeedsApproval, "b"); // 진행형 → 정리
        n.on_status(WS, SessionId(3), SessionStatus::Error, "c"); // 결과 → 유지
        // 1·2 사라짐(닫힘/archive). Done(1)·Error(3)는 기록이라 유지, 승인(2)만 정리
        n.retain_sessions(WS, &[SessionId(3)]);
        let sessions: Vec<_> = n.items.iter().map(|i| i.session).collect();
        assert!(sessions.contains(&SessionId(1)));
        assert!(sessions.contains(&SessionId(3)));
        assert!(!sessions.contains(&SessionId(2)));
    }

    #[test]
    fn exit_알림과_status_중복_방지() {
        let mut n = NotificationsUi::new();
        // status detector가 먼저 Done 감지 → 이후 exit(0)는 중복 발화 안 함
        n.on_status(WS, SessionId(1), SessionStatus::Done, "a");
        n.on_exit(WS, SessionId(1), Some(0), "a");
        assert_eq!(n.items.len(), 1);
        // regex 없는 agent: status 없이 exit만 → Done 알림 생성
        n.on_exit(WS, SessionId(2), Some(0), "b");
        assert_eq!(
            n.items.iter().filter(|i| i.session == SessionId(2)).count(),
            1
        );
        // 비정상 종료 → Error
        n.on_exit(WS, SessionId(3), Some(1), "c");
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
        n.on_status(WS, SessionId(1), SessionStatus::Done, "old"); // 읽을 항목
        n.toggle(); // 열기 → 모두 읽음
        n.toggle(); // 닫기
        assert_eq!(n.unread(), 0);
        n.on_status(WS, SessionId(2), SessionStatus::Waiting, "new"); // 진행형, 안 읽음 1
        assert_eq!(n.unread(), 1);
        // session 2(진행형)가 사라짐 → 정리되어 unread 0, 옛 읽은 Done(1)은 남아도 unread 0
        n.retain_sessions(WS, &[SessionId(1)]);
        assert_eq!(n.unread(), 0);
    }
}
