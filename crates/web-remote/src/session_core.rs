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
use crate::repository::WebRemoteRepository;

pub struct SessionCore {
    dashboard: DashboardHandle,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SessionCore {
    /// 브리지 스레드 하나를 띄운다. 리스너도 소켓도 만들지 않는다.
    pub fn spawn(repository: Option<Arc<dyn WebRemoteRepository>>) -> Arc<Self> {
        let (dashboard, worker) = DashboardHandle::spawn(repository);
        Arc::new(Self {
            dashboard,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn dashboard(&self) -> &DashboardHandle {
        &self.dashboard
    }

    /// 브리지 스레드를 정지하고 join한다. 여러 번 불러도 안전하다.
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
