//! `deppy-mcp-proxy` CLI 인자 파싱 — clap 없이 std::env::args만으로 처리한다.
//!   --db <path>                  공유 SQLite metadata DB
//!   --server <mcp_server_id>     프론트할 백엔드 MCP 서버 id
//!   --poll-ms <n>                승인 폴링 간격 (기본 200ms)
//!   --approval-timeout-secs <n>  Ask 승인 대기 상한 (기본 120s)
//!   --approval-notify-socket <p> 새 durable Ask를 알릴 optional local datagram socket

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::approval_notify::UnixDatagramApprovalNotifier;

/// 파싱된 실행 설정.
pub struct Cli {
    pub db_path: PathBuf,
    pub server_id: String,
    pub poll_interval: Duration,
    pub approval_timeout: Duration,
    pub approval_notifier: Option<UnixDatagramApprovalNotifier>,
}

const DEFAULT_POLL_MS: u64 = 200;
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 120;
/// 승인 대기 상한의 최댓값(초). 사람 승인엔 1시간이면 충분하고, orphan 정리 sweep이
/// 살아있는 프록시의 pending을 오살하지 않도록 상한을 둔다 — orphan cutoff가 이 값과
/// 같아, 어떤 live pending도 (나이 < 자기 timeout ≤ 이 상한 = cutoff)이라 안 쓸린다.
pub const MAX_APPROVAL_TIMEOUT_SECS: u64 = 3600;

impl Cli {
    /// process 인자에서 파싱 (main 진입점용).
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(std::env::args())
    }

    /// 임의 인자 iterator에서 파싱 (테스트 가능하도록 분리). 첫 원소는 실행 파일명.
    pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut args = args.into_iter();
        let _bin = args.next();

        let mut db_path: Option<PathBuf> = None;
        let mut server_id: Option<String> = None;
        let mut poll_ms = DEFAULT_POLL_MS;
        let mut approval_timeout_secs = DEFAULT_APPROVAL_TIMEOUT_SECS;
        let mut approval_notifier = None;

        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--db" => db_path = Some(PathBuf::from(value(&mut args, "--db")?)),
                "--server" => server_id = Some(value(&mut args, "--server")?),
                "--poll-ms" => {
                    poll_ms = value(&mut args, "--poll-ms")?
                        .parse()
                        .context("--poll-ms는 정수여야 함")?;
                }
                "--approval-timeout-secs" => {
                    approval_timeout_secs = value(&mut args, "--approval-timeout-secs")?
                        .parse()
                        .context("--approval-timeout-secs는 정수여야 함")?;
                }
                "--approval-notify-socket" => {
                    approval_notifier = Some(UnixDatagramApprovalNotifier::new(PathBuf::from(
                        value(&mut args, "--approval-notify-socket")?,
                    ))?);
                }
                other => bail!("알 수 없는 인자: {other}"),
            }
        }

        let db_path = db_path.context("--db <path> 인자가 필요합니다")?;
        let server_id = server_id.context("--server <mcp_server_id> 인자가 필요합니다")?;
        if poll_ms == 0 {
            bail!("--poll-ms는 1 이상이어야 함 (busy-loop 방지)");
        }
        // orphan sweep이 live pending을 오살하지 않도록 상한을 강제한다 (cutoff 불변식).
        let approval_timeout_secs = approval_timeout_secs.min(MAX_APPROVAL_TIMEOUT_SECS);

        Ok(Self {
            db_path,
            server_id,
            poll_interval: Duration::from_millis(poll_ms),
            approval_timeout: Duration::from_secs(approval_timeout_secs),
            approval_notifier,
        })
    }
}

/// `--flag` 다음의 값 하나를 꺼낸다 (없으면 에러).
fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .with_context(|| format!("{flag} 뒤에 값이 필요합니다"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once("deppy-mcp-proxy")
            .chain(parts.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn 필수_인자_파싱과_기본값() {
        let cli = Cli::parse(argv(&["--db", "/tmp/x.sqlite3", "--server", "srv-1"])).unwrap();
        assert_eq!(cli.db_path, PathBuf::from("/tmp/x.sqlite3"));
        assert_eq!(cli.server_id, "srv-1");
        assert_eq!(cli.poll_interval, Duration::from_millis(DEFAULT_POLL_MS));
        assert_eq!(
            cli.approval_timeout,
            Duration::from_secs(DEFAULT_APPROVAL_TIMEOUT_SECS)
        );
        assert!(cli.approval_notifier.is_none());
    }

    #[test]
    fn 선택_인자_오버라이드() {
        let cli = Cli::parse(argv(&[
            "--db",
            "/tmp/x",
            "--server",
            "s",
            "--poll-ms",
            "50",
            "--approval-timeout-secs",
            "5",
        ]))
        .unwrap();
        assert_eq!(cli.poll_interval, Duration::from_millis(50));
        assert_eq!(cli.approval_timeout, Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn approval_notify_socket은_optional_bounded_local_path다() {
        let cli = Cli::parse(argv(&[
            "--db",
            "/tmp/x",
            "--server",
            "s",
            "--approval-notify-socket",
            "/tmp/deppy-approval.sock",
        ]))
        .unwrap();
        assert!(cli.approval_notifier.is_some());
    }

    #[test]
    fn 필수_인자_누락은_에러() {
        assert!(Cli::parse(argv(&["--server", "s"])).is_err());
        assert!(Cli::parse(argv(&["--db", "/tmp/x"])).is_err());
    }

    #[test]
    fn poll_ms_0은_거부() {
        assert!(Cli::parse(argv(&["--db", "/tmp/x", "--server", "s", "--poll-ms", "0"])).is_err());
    }

    #[test]
    fn 알수없는_인자는_에러() {
        assert!(Cli::parse(argv(&["--db", "/tmp/x", "--server", "s", "--nope"])).is_err());
    }
}
