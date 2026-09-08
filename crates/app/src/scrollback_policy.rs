//! 런타임 수명별 스크롤백 정책 전달. 최신 요청 하나와 유계 재시도만 보관한다.
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u8 = 6;
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub generation: u64,
    pub requested: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    pub request: Request,
    pub applied: u16,
    pub unsupported: u16,
    pub trimmed: u64,
    pub effective_min: u32,
    pub durable: bool,
    pub restored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Applied,
    Partial,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub status: Status,
    /// 독립 복구 로그까지 과거 삭제를 보장하는가. 현재 runtime은 false다.
    pub durable: bool,
    pub restored: bool,
    pub applied: u32,
    pub unsupported: u32,
    pub trimmed: u64,
    pub effective_min: u32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            status: Status::Pending,
            durable: false,
            restored: false,
            applied: 0,
            unsupported: 0,
            trimmed: 0,
            effective_min: 0,
        }
    }
}

pub struct Delivery {
    runtime_instance: u64,
    request: Request,
    attempts: u8,
    next_attempt: Option<Instant>,
    result: Option<Applied>,
    failed: bool,
    restore_required: bool,
}

impl Delivery {
    pub fn new(runtime_instance: u64, requested: u32, now: Instant) -> Self {
        Self {
            runtime_instance,
            request: Request {
                generation: 1,
                requested,
            },
            attempts: 0,
            next_attempt: Some(now),
            result: None,
            failed: false,
            restore_required: false,
        }
    }
    /// RestoreWorkspace가 입장한 뒤에만 복원 완료 ACK를 요구한다. 명령 전 dotenv
    /// 대기는 기존 복원 lane이 소유하며 여기서 별도 타임아웃을 만들지 않는다.
    pub fn require_restore(&mut self, required: bool, now: Instant) {
        if self.restore_required == required {
            return;
        }
        self.restore_required = required;
        if let Some(result) = self.result {
            if required && !result.restored {
                self.restart(now);
            } else if !required {
                self.failed = false;
                self.next_attempt = None;
            }
        }
    }

    pub fn set_requested(&mut self, requested: u32, now: Instant) {
        if self.request.requested == requested {
            return;
        }
        let Some(generation) = self.request.generation.checked_add(1) else {
            // 같은 세대를 다른 값에 재사용하면 worker의 충돌 방어와 어긋난다.
            self.failed = true;
            self.next_attempt = None;
            return;
        };
        self.request = Request {
            generation,
            requested,
        };
        self.result = None;
        self.restart(now);
    }

    fn restart(&mut self, now: Instant) {
        self.attempts = 0;
        self.failed = false;
        self.next_attempt = Some(now);
    }

    fn backoff(&self) -> Duration {
        Duration::from_millis(16u64 << self.attempts.saturating_sub(1).min(5))
    }

    /// logic tick당 최대 한 번 전송한다. 반환한 시간에만 다음 tick을 예약한다.
    pub fn poll(
        &mut self,
        now: Instant,
        mut send: impl FnMut(Request) -> bool,
    ) -> Option<Duration> {
        let due = self.next_attempt?;
        if due > now {
            return Some(due.duration_since(now));
        }
        if self.attempts >= MAX_ATTEMPTS {
            self.failed = true;
            self.next_attempt = None;
            return None;
        }
        self.attempts += 1;
        let wait = if send(self.request) {
            ACK_TIMEOUT
        } else {
            self.backoff()
        };
        self.next_attempt = Some(now + wait);
        Some(wait)
    }

    pub fn observe(&mut self, runtime_instance: u64, result: Applied, now: Instant) -> bool {
        if runtime_instance != self.runtime_instance || result.request != self.request {
            return false;
        }
        // durable은 독립 복구 로그의 삭제 보장 범위이며 적용 성공/실패가 아니다.
        // 현재 실행 중인 backend의 실제 ACK이면 false여도 재시도 없이 완료한다.
        self.result = Some(result);
        if self.restore_required && !result.restored {
            // 초기 정책 ACK만 도착했으면 복원 최종 ACK를 유계로 재확인한다.
            if !self.failed && self.next_attempt.is_none() {
                self.next_attempt = Some(now + ACK_TIMEOUT);
            }
        } else {
            self.failed = false;
            self.next_attempt = None;
        }
        true
    }

    pub fn retry(&mut self, now: Instant) {
        if self.failed {
            self.restart(now);
        }
    }

    pub fn view_after_restore(&self, requested: u32, restore_required: bool) -> View {
        let mut view = self.view(requested);
        if restore_required && !view.restored && view.status != Status::Failed {
            view.status = Status::Pending;
        }
        view
    }

    pub fn view(&self, requested: u32) -> View {
        if self.failed {
            return View {
                status: Status::Failed,
                ..View::default()
            };
        }
        if requested != self.request.requested {
            return View::default();
        }
        let Some(result) = self.result else {
            return View::default();
        };
        View {
            status: if self.restore_required && !result.restored {
                Status::Pending
            } else if result.unsupported > 0 {
                Status::Partial
            } else {
                Status::Applied
            },
            durable: result.durable,
            restored: result.restored,
            applied: u32::from(result.applied),
            unsupported: u32::from(result.unsupported),
            trimmed: result.trimmed,
            effective_min: result.effective_min,
        }
    }
}

pub fn aggregate(views: impl IntoIterator<Item = View>) -> View {
    let mut output = View {
        status: Status::Applied,
        durable: true,
        restored: true,
        ..View::default()
    };
    let mut effective_min = None;
    for view in views {
        output.status = match (output.status, view.status) {
            (Status::Failed, _) | (_, Status::Failed) => Status::Failed,
            (Status::Pending, _) | (_, Status::Pending) => Status::Pending,
            (Status::Partial, _) | (_, Status::Partial) => Status::Partial,
            _ => Status::Applied,
        };
        output.durable &= view.durable;
        output.restored &= view.restored;
        output.applied = output.applied.saturating_add(view.applied);
        output.unsupported = output.unsupported.saturating_add(view.unsupported);
        output.trimmed = output.trimmed.saturating_add(view.trimmed);
        if view.applied > 0 {
            effective_min = Some(
                effective_min.map_or(view.effective_min, |old: u32| old.min(view.effective_min)),
            );
        }
    }
    output.effective_min = effective_min.unwrap_or(0);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(request: Request) -> Applied {
        Applied {
            request,
            applied: 1,
            unsupported: 0,
            trimmed: 12,
            effective_min: request.requested,
            durable: true,
            restored: true,
        }
    }

    #[test]
    fn 초기_정책과_ack_확인은_동일한_첫_세대를_쓴다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 5000, now);
        let mut sent = Vec::new();
        delivery.poll(now, |request| {
            sent.push(request);
            true
        });
        assert_eq!(
            sent,
            vec![Request {
                generation: 1,
                requested: 5000
            }]
        );
        assert_eq!(
            delivery.view(5000).status,
            Status::Pending,
            "큐 수락은 적용 완료가 아니다"
        );
        assert!(delivery.observe(7, ack(sent[0]), now));
        assert_eq!(delivery.view(5000).status, Status::Applied);
    }

    #[test]
    fn 다른_runtime_이전_세대_같은_세대의_다른_값_ack은_무시한다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        let old = delivery.request;
        delivery.set_requested(5000, now);
        let current = delivery.request;
        assert_ne!(current.generation, old.generation);
        assert!(!delivery.observe(8, ack(current), now));
        assert!(!delivery.observe(7, ack(old), now));
        assert!(!delivery.observe(
            7,
            ack(Request {
                requested: 100,
                ..current
            }),
            now
        ));
        assert_eq!(delivery.view(5000).status, Status::Pending);
        assert!(delivery.observe(7, ack(current), now));
        assert_eq!(delivery.view(5000).status, Status::Applied);
    }

    #[test]
    fn 연속_편집은_대기중인_최신값_하나만_전송한다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        delivery.poll(now, |_| false);
        for requested in [200, 300, 5000] {
            delivery.set_requested(requested, now);
        }
        let mut sent = Vec::new();
        delivery.poll(now, |request| {
            sent.push(request);
            true
        });
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0],
            Request {
                generation: 4,
                requested: 5000
            }
        );
        delivery.set_requested(5000, now);
        assert_eq!(
            delivery.request, sent[0],
            "같은 설정은 세대를 올리지 않는다"
        );
    }

    #[test]
    fn queue_full은_유계_재시도_후_실패하고_수동으로만_다시_시작한다() {
        let start = Instant::now();
        let mut delivery = Delivery::new(7, 100, start);
        let mut now = start;
        let mut calls = 0;
        for _ in 0..20 {
            let wait = delivery.poll(now, |_| {
                calls += 1;
                false
            });
            now += wait.unwrap_or(Duration::from_secs(1));
        }
        assert_eq!(calls, usize::from(MAX_ATTEMPTS));
        assert_eq!(delivery.view(100).status, Status::Failed);
        delivery.retry(now);
        let request = delivery.request;
        delivery.poll(now, |_| {
            calls += 1;
            true
        });
        assert_eq!(calls, usize::from(MAX_ATTEMPTS) + 1);
        assert!(delivery.observe(7, ack(request), now));
        assert_eq!(delivery.view(100).status, Status::Applied);
    }

    #[test]
    fn ack_유실만_재시도하고_로그삭제_미보장은_실제_적용완료다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        let request = delivery.request;
        let mut sent = Vec::new();
        assert_eq!(
            delivery.poll(now, |r| {
                sent.push(r);
                true
            }),
            Some(ACK_TIMEOUT)
        );
        delivery.poll(now + ACK_TIMEOUT, |r| {
            sent.push(r);
            true
        });
        assert_eq!(sent, [request, request]);
        assert!(delivery.observe(
            7,
            Applied {
                durable: false,
                ..ack(request)
            },
            now + ACK_TIMEOUT
        ));
        assert_eq!(delivery.view(100).status, Status::Applied);
        assert!(!delivery.view(100).durable);
        assert!(delivery.next_attempt.is_none());
        assert!(delivery.observe(7, ack(request), now + ACK_TIMEOUT));
        assert_eq!(delivery.view(100).status, Status::Applied);
        assert!(
            delivery
                .poll(now + ACK_TIMEOUT * 2, |_| panic!(
                    "완료 뒤 자동 재전송 금지"
                ))
                .is_none()
        );
    }

    #[test]
    fn active_warm_신규_runtime의_독립_상태를_집계한다() {
        let now = Instant::now();
        let mut active = Delivery::new(1, 100, now);
        let mut warm = Delivery::new(2, 100, now);
        active.observe(1, ack(active.request), now);
        assert_eq!(
            aggregate([active.view(100), warm.view(100)]).status,
            Status::Pending
        );
        warm.observe(
            2,
            Applied {
                unsupported: 1,
                ..ack(warm.request)
            },
            now,
        );
        let view = aggregate([active.view(100), warm.view(100)]);
        assert_eq!(view.status, Status::Partial);
        assert_eq!((view.applied, view.unsupported, view.trimmed), (2, 1, 24));
        let replacement = Delivery::new(3, 100, now);
        assert_eq!(
            aggregate([active.view(100), replacement.view(100)]).status,
            Status::Pending
        );
        assert_eq!(
            active.view(5000).status,
            Status::Pending,
            "render에서 편집한 최신값은 logic 전에도 완료로 표시하지 않는다"
        );
    }

    #[test]
    fn 새_세션의_동일_정책_ack은_집계만_갱신한다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        delivery.observe(7, ack(delivery.request), now);
        delivery.observe(
            7,
            Applied {
                applied: 2,
                ..ack(delivery.request)
            },
            now,
        );
        assert_eq!(delivery.view(100).applied, 2);
        assert!(
            delivery
                .poll(now, |_| panic!("세션 편입 ACK가 재전송을 만들면 안 된다"))
                .is_none()
        );
    }
    #[test]
    fn 앱_초기_정책은_factory에서_복원보다_먼저_입장한다() {
        let source = include_str!("app.rs");
        let make = source
            .split_once("    fn make_runtime(")
            .unwrap()
            .1
            .split_once("\n    // ---")
            .unwrap()
            .0;
        assert!(make.contains("scrollback_policy: Some((1, config.terminal.scrollback_lines))"));
        assert!(make.contains("scrollback_delivery: crate::scrollback_policy::Delivery::new("));
        let remote = source
            .split_once("    fn start_remote(")
            .unwrap()
            .1
            .split_once("\n    ///")
            .unwrap()
            .0;
        assert!(
            remote.contains("scrollback_policy: Some((1, self.config.terminal.scrollback_lines))")
        );
    }

    #[test]
    fn 앱은_active_warm_ack을_logic에서_관측하고_렌더는_재시도_의도만_남긴다() {
        let source = include_str!("app.rs");
        assert!(source.contains("Self::observe_scrollback_policy(rt, &events)"));
        assert!(source.contains("Self::observe_scrollback_policy(&mut self.active, &new_events)"));
        let logic = source
            .split_once("    fn logic(&mut self, ctx:")
            .unwrap()
            .1
            .split_once("    fn ui(")
            .unwrap()
            .0;
        assert!(logic.contains("self.pump_scrollback_policy(ctx)"));
        assert!(source.contains("self.pending_scrollback_policy_retry = true"));
        assert!(include_str!("ui/settings.rs").contains("pub scrollback_retry: bool"));
    }
    #[test]
    fn 같은_세대의_후속_ack은_로그삭제_미보장을_반영하되_재시도하지_않는다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        let request = delivery.request;
        delivery.observe(7, ack(request), now);
        assert!(delivery.observe(
            7,
            Applied {
                durable: false,
                ..ack(request)
            },
            now
        ));
        assert_eq!(delivery.view(100).status, Status::Applied);
        assert!(!delivery.view(100).durable);
        assert!(delivery.next_attempt.is_none());
        let mut sends = 0;
        delivery.poll(now + Duration::from_secs(1), |_| {
            sends += 1;
            true
        });
        assert_eq!(sends, 0);
    }

    #[test]
    fn warm_ack_관측은_종료가_아닌_매_logic_drain에서_실행된다() {
        let source = include_str!("app.rs");
        let logic = source.split_once("    fn logic(&mut self, ctx:").unwrap().1;
        let warm = logic
            .split_once("for rt in self.warm.values_mut()")
            .unwrap()
            .1
            .split_once("for (workspace_id, event) in approval_runtime_events")
            .unwrap()
            .0;
        assert!(warm.contains("Self::observe_scrollback_policy(rt, &events"));
    }
    #[test]
    fn 복원_명령_큐수락은_복원완료_ack을_대신하지_않는다() {
        let now = Instant::now();
        let mut delivery = Delivery::new(7, 100, now);
        let request = delivery.request;
        delivery.observe(
            7,
            Applied {
                restored: false,
                durable: false,
                ..ack(request)
            },
            now,
        );
        assert_eq!(
            delivery.view_after_restore(100, false).status,
            Status::Applied,
            "복원 없는 신규 runtime은 정책 ACK로 완료된다"
        );
        assert_eq!(
            delivery.view_after_restore(100, true).status,
            Status::Pending,
            "RestoreWorkspace 큐수락 뒤에도 실제 복원 ACK를 기다려야 한다"
        );
        delivery.observe(
            7,
            Applied {
                restored: true,
                durable: false,
                ..ack(request)
            },
            now,
        );
        assert_eq!(
            delivery.view_after_restore(100, true).status,
            Status::Applied
        );
    }

    #[test]
    fn 초기_ack_뒤_복원_ack이_유실되어도_유계_재확인한다() {
        let start = Instant::now();
        let mut delivery = Delivery::new(7, 100, start);
        let request = delivery.request;
        delivery.observe(
            7,
            Applied {
                restored: false,
                ..ack(request)
            },
            start,
        );
        delivery.require_restore(true, start);
        let mut now = start;
        let mut sends = 0;
        for _ in 0..20 {
            let wait = delivery.poll(now, |_| {
                sends += 1;
                true
            });
            now += wait.unwrap_or(ACK_TIMEOUT);
        }
        assert_eq!(sends, usize::from(MAX_ATTEMPTS));
        assert_eq!(
            delivery.view_after_restore(100, true).status,
            Status::Failed
        );
        delivery.retry(now);
        delivery.poll(now, |_| true);
        delivery.observe(
            7,
            Applied {
                restored: true,
                ..ack(request)
            },
            now,
        );
        assert_eq!(
            delivery.view_after_restore(100, true).status,
            Status::Applied
        );
    }
    #[test]
    fn 축출_취소로_warm에_복귀하기_전에도_마지막_ack을_관측한다() {
        let source = include_str!("app.rs");
        let drain = source
            .split_once("if let Some(mut rt) = self.warm.remove(workspace_id)")
            .unwrap()
            .1
            .split_once("self.warm.insert(workspace_id.to_owned(), rt)")
            .unwrap()
            .0;
        assert!(drain.contains("Self::observe_scrollback_policy(&mut rt, &events)"));
    }
    #[test]
    fn tls_worker도_기존_dispatcher와_ack으로_정책을_동기화한다() {
        let source = include_str!("app.rs");
        let start = source
            .split_once("    fn start_remote(")
            .unwrap()
            .1
            .split_once("    /// settings 체크 on")
            .unwrap()
            .0;
        assert!(start.contains("RuntimeHost::command_dispatcher(&worker)"));
        assert!(start.contains("subscribe_with_wake("));
        assert!(
            start.find("subscribe_with_wake(").unwrap()
                < start.find("RemoteRuntimeServer::serve_tls").unwrap()
        );
        let pump = source
            .split_once("    fn pump_scrollback_policy(")
            .unwrap()
            .1
            .split_once("    fn observe_scrollback_policy(")
            .unwrap()
            .0;
        assert!(pump.contains("self.remote.as_mut()"));
        assert!(pump.contains("policy.pump(requested, retry, now, ctx)"));
        let view = source
            .split_once("    fn scrollback_policy_view(")
            .unwrap()
            .1
            .split_once("    /// 설정의 exited cap")
            .unwrap()
            .0;
        assert!(view.contains("self.remote.iter()"));
        assert!(
            view.split_whitespace()
                .collect::<String>()
                .contains("remote.policy.delivery.view(")
        );
        assert!(source.contains("state.shutdown()"));
    }

    #[test]
    fn tls_교체는_이전_ack을_거부하고_종료된_worker를_집계하지_않는다() {
        let now = Instant::now();
        let mut active = Delivery::new(1, 100, now);
        active.observe(1, ack(active.request), now);
        let mut remote = Delivery::new(2, 100, now);
        remote.observe(2, ack(remote.request), now);
        remote.set_requested(5000, now);
        assert_eq!(
            aggregate([active.view(100), remote.view(5000)]).status,
            Status::Pending
        );
        let mut replacement = Delivery::new(3, 5000, now);
        assert!(!replacement.observe(2, ack(replacement.request), now));
        replacement.observe(3, ack(replacement.request), now);
        assert_eq!(
            aggregate([active.view(100), replacement.view(5000)]).applied,
            2
        );
        assert_eq!(aggregate([active.view(100)]).applied, 1);
    }

    #[test]
    fn tls_worker는_전역_캐시예산_분모와_방송에_포함된다() {
        let source = include_str!("app.rs");
        let policy = source
            .split_once("    fn terminal_cache_policy_command(")
            .unwrap()
            .1
            .split_once("    /// 폴더의")
            .unwrap()
            .0;
        assert!(policy.contains("usize::from(self.remote.is_some())"));
        assert!(policy.contains("remote.policy.dispatcher"));
        for marker in ["    fn remote_enable(", "    fn remote_disable("] {
            let body = source
                .split_once(marker)
                .unwrap()
                .1
                .split_once("    ///")
                .unwrap()
                .0;
            assert!(body.contains("self.broadcast_terminal_cache_policy()"));
        }
        let auto = source
            .split_once("match app.start_remote()")
            .unwrap()
            .1
            .split_once("    fn make_runtime(")
            .unwrap()
            .0;
        assert!(auto.contains("app.broadcast_terminal_cache_policy()"));
    }
}
