//! Optional payload-free wake notification for newly durable approval requests.
//!
//! The notifier owns only a bounded socket path. It creates one transient datagram socket for one
//! synchronous send, so the absent and idle paths have no thread, socket, timer, or polling cost.

use std::path::PathBuf;

/// Portable ceiling below the smallest supported Unix `sockaddr_un.sun_path` capacity, including
/// room for the terminating NUL used by the OS.
pub(crate) const APPROVAL_NOTIFY_SOCKET_PATH_BYTES_MAX: usize = 100;
const APPROVAL_WAKE_DATAGRAM: [u8; 1] = [1];

pub(crate) trait ApprovalWakeNotifier: Send + Sync {
    /// No operation/session/tool data can enter this interface. The implementation sends only the
    /// fixed one-byte wake marker.
    fn notify(&self) -> anyhow::Result<()>;
}

pub(crate) struct UnixDatagramApprovalNotifier {
    path: PathBuf,
}

impl UnixDatagramApprovalNotifier {
    pub(crate) fn new(path: PathBuf) -> anyhow::Result<Self> {
        let encoded = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("approval notify socket path is not UTF-8"))?
            .as_bytes();
        anyhow::ensure!(
            !encoded.is_empty() && encoded.len() <= APPROVAL_NOTIFY_SOCKET_PATH_BYTES_MAX,
            "approval notify socket path byte length is invalid"
        );
        anyhow::ensure!(
            !encoded.contains(&0),
            "approval notify socket path contains NUL"
        );
        anyhow::ensure!(
            path.is_absolute(),
            "approval notify socket path must be absolute"
        );
        #[cfg(not(unix))]
        anyhow::bail!("approval notify socket is unsupported on this platform");
        #[cfg(unix)]
        Ok(Self { path })
    }
}

impl std::fmt::Debug for UnixDatagramApprovalNotifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixDatagramApprovalNotifier")
            .field("path", &"REDACTED")
            .finish()
    }
}

impl ApprovalWakeNotifier for UnixDatagramApprovalNotifier {
    #[cfg(unix)]
    fn notify(&self) -> anyhow::Result<()> {
        let socket = std::os::unix::net::UnixDatagram::unbound()
            .map_err(|_| anyhow::anyhow!("approval wake socket unavailable"))?;
        let sent = socket
            .send_to(&APPROVAL_WAKE_DATAGRAM, &self.path)
            .map_err(|_| anyhow::anyhow!("approval wake delivery failed"))?;
        anyhow::ensure!(
            sent == APPROVAL_WAKE_DATAGRAM.len(),
            "approval wake delivery was incomplete"
        );
        Ok(())
    }

    #[cfg(not(unix))]
    fn notify(&self) -> anyhow::Result<()> {
        anyhow::bail!("approval notify socket is unsupported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path는_exact_limit만_accept하고_debug를_redact한다() {
        assert_eq!(APPROVAL_WAKE_DATAGRAM, [1]);
        let exact = PathBuf::from(format!(
            "/{}",
            "a".repeat(APPROVAL_NOTIFY_SOCKET_PATH_BYTES_MAX - 1)
        ));
        assert_eq!(
            exact.to_str().unwrap().len(),
            APPROVAL_NOTIFY_SOCKET_PATH_BYTES_MAX
        );

        #[cfg(unix)]
        {
            let notifier = UnixDatagramApprovalNotifier::new(exact.clone()).unwrap();
            let debug = format!("{notifier:?}");
            assert!(debug.contains("REDACTED"));
            assert!(!debug.contains(exact.to_str().unwrap()));
        }

        let plus_one = PathBuf::from(format!(
            "/{}",
            "a".repeat(APPROVAL_NOTIFY_SOCKET_PATH_BYTES_MAX)
        ));
        assert!(UnixDatagramApprovalNotifier::new(plus_one).is_err());
        assert!(UnixDatagramApprovalNotifier::new(PathBuf::from("relative.sock")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn missing_socket_error는_path를_노출하지_않고_persistent_resource를_만들지_않는다() {
        let path = PathBuf::from(format!(
            "/tmp/deppy-approval-missing-{}.sock",
            uuid::Uuid::new_v4().simple()
        ));
        let notifier = UnixDatagramApprovalNotifier::new(path.clone()).unwrap();
        for _ in 0..3 {
            let error = notifier.notify().unwrap_err().to_string();
            assert_eq!(error, "approval wake delivery failed");
            assert!(!error.contains(path.to_str().unwrap()));
        }
        assert!(
            !path.exists(),
            "sender must not create or bind a socket path"
        );
    }
}
