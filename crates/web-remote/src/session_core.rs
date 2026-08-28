//! 전송 중립 대시보드/런타임 명령 코어.
//!
//! 지금까지 대시보드 브리지 스레드는 [`crate::WebRemoteServer`]가 만들고 소유했다. 그래서
//! Relay만 켜려 해도 loopback 리스너를 먼저 세워야 했다 — 두 전송이 독립이라는 계약과 어긋난다.
//!
//! 이 값은 리스너 없이 혼자 설 수 있는 코어다. 세 가지 배치를 모두 지원한다:
//!
//! - Tailscale만: [`crate::WebRemoteServer::serve`]가 예전처럼 자기 코어를 만들어 소유한다.
//! - Relay만: 앱이 [`SessionCore::spawn`]으로 코어만 띄운다. 소켓도 리스너도 없다.
//! - 둘 다: 앱이 코어를 하나 만들고 [`crate::WebRemoteServer::serve_with_core`]로 공유한다.
//!   한 전송을 끄거나 실패해도 다른 전송의 코어 수명에 영향이 없다.
//!
//! 소유권 규칙은 하나다: **코어를 만든 쪽이 코어를 멈춘다.** 공유받은 서버는 자기 accept
//! 스레드와 접속만 정리하고 코어에는 손대지 않는다.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::dashboard::DashboardHandle;
use crate::push::{PushHandle, PushManager, VapidKey};
use crate::repository::WebRemoteRepository;

pub struct SessionCore {
    dashboard: DashboardHandle,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// 웹푸시 발송기. **코어가 소유한다** — 코어를 공유하는 배치에서 서버가 이걸 들고 있으면,
    /// 서버를 끄는 순간 코어가 죽은 싱크를 가리키게 된다.
    push: Mutex<Option<PushManager>>,
}

impl SessionCore {
    /// 브리지 스레드 하나를 띄운다. 리스너도 소켓도 만들지 않는다.
    pub fn spawn(repository: Option<Arc<dyn WebRemoteRepository>>) -> Arc<Self> {
        Self::spawn_inner(repository, None)
    }

    /// 브리지와 함께 웹푸시 발송기까지 코어가 소유한다. VAPID 키와 저장소가 모두 있을 때만
    /// 발송 스레드가 뜬다. 발송기 생성 실패는 푸시만 비활성화하며 대시보드는 그대로 산다.
    pub fn spawn_with_push(
        repository: Option<Arc<dyn WebRemoteRepository>>,
        vapid: Option<VapidKey>,
    ) -> Arc<Self> {
        Self::spawn_inner(repository, vapid)
    }

    fn spawn_inner(
        repository: Option<Arc<dyn WebRemoteRepository>>,
        vapid: Option<VapidKey>,
    ) -> Arc<Self> {
        let (dashboard, worker) = DashboardHandle::spawn(repository.clone());
        let push = match (repository, vapid) {
            (Some(repository), Some(vapid)) => match PushManager::spawn(repository, vapid) {
                Ok(manager) => {
                    dashboard.set_push_sink(manager.handle());
                    Some(manager)
                }
                Err(error) => {
                    tracing::warn!("web-remote 웹푸시 비활성(발송 스레드 생성 실패): {error:#}");
                    None
                }
            },
            _ => None,
        };
        Arc::new(Self {
            dashboard,
            worker: Mutex::new(Some(worker)),
            push: Mutex::new(push),
        })
    }

    pub fn dashboard(&self) -> &DashboardHandle {
        &self.dashboard
    }

    /// 발송기가 아직 없으면 지금 만든다. 이미 있으면 그대로 둔다.
    ///
    /// Relay를 먼저 켜면 코어가 VAPID 키 없이 만들어진다. 그 뒤 web을 켜면 기존 코어를
    /// 재사용하는데, 이 보정이 없으면 키가 있는데도 웹푸시가 영영 꺼진 채로 남는다.
    pub fn ensure_push(
        &self,
        repository: Option<Arc<dyn WebRemoteRepository>>,
        vapid: Option<VapidKey>,
    ) {
        let (Some(repository), Some(vapid)) = (repository, vapid) else {
            return;
        };
        let mut push = self
            .push
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if push.is_some() {
            return;
        }
        match PushManager::spawn(repository, vapid) {
            Ok(manager) => {
                self.dashboard.set_push_sink(manager.handle());
                *push = Some(manager);
            }
            Err(error) => {
                tracing::warn!("web-remote 웹푸시 비활성(발송 스레드 생성 실패): {error:#}");
            }
        }
    }

    /// 발송기를 멈춘다. 웹푸시는 **web 전송의 수명에 묶인다** — 코어가 Relay 때문에
    /// 살아남더라도, web이 내려가면 발송기도 함께 멈춰야 한다. 그러지 않으면 사용자가
    /// 모바일 웹을 껐는데도 폰으로 알림이 계속 간다.
    pub fn stop_push(&self) {
        let push = self
            .push
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(push) = push {
            push.stop_and_join();
        }
    }

    /// 이 코어가 소유한 발송기의 핸들. 서버가 구독 라우트에 쓴다.
    pub fn push_handle(&self) -> Option<PushHandle> {
        self.push
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(PushManager::handle)
    }

    /// 브리지 스레드와 발송 스레드를 정지하고 join한다. 여러 번 불러도 안전하다.
    pub fn shutdown(&self) {
        self.dashboard.stop();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        let push = self
            .push
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(push) = push {
            push.stop_and_join();
        }
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for SessionCore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionCore")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relay만 켜는 배치의 핵심 성질 — 리스너 없이 코어가 선다.
    #[test]
    fn the_core_stands_up_and_shuts_down_without_any_listener() {
        let core = SessionCore::spawn(None);
        assert_eq!(core.dashboard().poll_count(), 0);
        core.dashboard().set_notice(Some("relay-only".to_owned()));
        core.shutdown();
        // 두 번 불러도 안전하다(Drop이 다시 부른다).
        core.shutdown();
    }

    /// 웹푸시는 web 전송의 수명에 묶인다. 코어가 Relay 때문에 살아남아도 발송기는 멈춘다.
    #[test]
    fn stop_push_detaches_the_sender_while_the_core_keeps_running() {
        let core = SessionCore::spawn(None);
        // 저장소/키가 없으면 발송기 자체가 없다 — 멈추기는 그래도 안전해야 한다.
        assert!(core.push_handle().is_none());
        core.stop_push();
        assert!(core.push_handle().is_none());
        // 코어는 계속 산다.
        core.dashboard()
            .set_notice(Some("still-running".to_owned()));
        core.shutdown();
    }

    /// 코어 생성 경로에 리스너·바인드가 끼어들지 않는다는 것을 소스로 고정한다.
    #[test]
    fn the_core_module_names_no_socket_type() {
        let source = include_str!("session_core.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap();
        for forbidden in ["TcpListener", "TcpStream", "SocketAddr", "bind("] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
    }
}
