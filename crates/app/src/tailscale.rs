//! Tailscale ts.net 호스트명 자동 감지 (모바일 웹 P1 후속 — 2026-07-11).
//!
//! `tailscale status --json`의 `Self.DNSName`을 읽어 설정의 ts.net 호스트명을 채운다.
//! macOS GUI 앱 설치는 CLI가 PATH에 없으므로(Tailscale.app 내장) 알려진 경로 후보를
//! 순서대로 시도한다. 감지는 설정 페이지 진입/버튼에서만 1회성 스레드로 돈다 — 상주
//! 폴링·타이머 없음(§14 예산 관례). status 출력에는 tailnet 피어 정보가 실리므로
//! 원문을 로그에 남기지 않는다.

use std::sync::mpsc;

/// 감지 결과. 실패를 두 층으로 나눠 설정 UI가 원인별 안내를 보여준다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// 데몬 실행 중 + MagicDNS 호스트명 확보 (후행 '.' 제거본).
    Hostname(String),
    /// CLI는 찾았지만 미로그인/정지/MagicDNS off 등으로 호스트명이 없다.
    NoHostname,
    /// CLI를 찾지 못했다 — 미설치 또는 알 수 없는 설치 경로.
    CliNotFound,
}

/// CLI 후보 — PATH의 tailscale을 먼저, macOS는 GUI 앱 내장 CLI와 homebrew 경로를
/// 이어서 시도한다 (GUI 앱으로 뜬 프로세스의 PATH에는 homebrew가 없다).
fn cli_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from("tailscale")];
    if cfg!(target_os = "macos") {
        candidates.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
        candidates.push("/opt/homebrew/bin/tailscale".into());
        candidates.push("/usr/local/bin/tailscale".into());
    }
    candidates
}

/// `status --json` 출력에서 ts.net 호스트명을 뽑는다 (순수 — 단위 테스트 대상).
/// 데몬이 Running이 아니거나 DNSName이 없으면 None.
fn parse_status_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("BackendState")?.as_str()? != "Running" {
        return None;
    }
    let dns_name = value.get("Self")?.get("DNSName")?.as_str()?;
    let host = dns_name.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_owned())
}

/// CLI 후보를 순서대로 실행해 호스트명을 감지한다. 실행 자체가 실패한 후보(미존재 등)는
/// 건너뛰고, 하나라도 실행됐지만 호스트명이 없으면 [`Detected::NoHostname`].
fn detect() -> Detected {
    let mut cli_found = false;
    for bin in cli_candidates() {
        let output = std::process::Command::new(&bin)
            .args(["status", "--json"])
            .stdin(std::process::Stdio::null())
            .output();
        let Ok(output) = output else {
            continue; // 이 후보 경로에 CLI 없음 — 다음 후보
        };
        cli_found = true;
        if let Some(host) = parse_status_json(&String::from_utf8_lossy(&output.stdout)) {
            return Detected::Hostname(host);
        }
    }
    if cli_found {
        Detected::NoHostname
    } else {
        Detected::CliNotFound
    }
}

/// 1회성 감지 스레드를 띄운다 — 완료 시 결과를 보내고 repaint로 UI를 깨운다
/// (agent_detect_worker 관례). 수신측은 try_recv로 폴링 없이 프레임 내에서 확인한다.
pub fn spawn_detect(ctx: egui::Context) -> mpsc::Receiver<Detected> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("ts-detect".into())
        .spawn(move || {
            if tx.send(detect()).is_ok() {
                ctx.request_repaint();
            }
        });
    if let Err(e) = spawned {
        // 스레드 생성 실패 → rx가 즉시 Disconnected — 수신측이 시도 종료로 처리한다.
        tracing::warn!("tailscale 감지 스레드 생성 실패: {e:#}");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_상태의_dnsname을_후행_점_제거로_파싱한다() {
        let json = r#"{
            "BackendState": "Running",
            "Self": { "DNSName": "jr-macbookair.tail02799e.ts.net." }
        }"#;
        assert_eq!(
            parse_status_json(json),
            Some("jr-macbookair.tail02799e.ts.net".to_owned())
        );
    }

    #[test]
    fn 정지_상태나_dnsname_부재는_none() {
        // 데몬 정지 — DNSName이 있어도 신뢰하지 않는다
        let stopped = r#"{
            "BackendState": "Stopped",
            "Self": { "DNSName": "x.ts.net." }
        }"#;
        assert_eq!(parse_status_json(stopped), None);
        // Running이지만 Self/DNSName 없음 (미로그인·MagicDNS off)
        assert_eq!(parse_status_json(r#"{"BackendState":"Running"}"#), None);
        assert_eq!(
            parse_status_json(r#"{"BackendState":"Running","Self":{"DNSName":""}}"#),
            None
        );
        assert_eq!(
            parse_status_json(r#"{"BackendState":"Running","Self":{"DNSName":"."}}"#),
            None
        );
    }

    #[test]
    fn 기형_json은_none() {
        assert_eq!(parse_status_json("not json"), None);
        assert_eq!(parse_status_json("{}"), None);
    }

    #[test]
    fn 후보_경로는_path_우선이고_macos는_gui_앱_cli를_포함한다() {
        let candidates = cli_candidates();
        assert_eq!(candidates[0], std::path::PathBuf::from("tailscale"));
        if cfg!(target_os = "macos") {
            assert!(candidates.iter().any(|p| {
                p.to_string_lossy()
                    .contains("Tailscale.app/Contents/MacOS/Tailscale")
            }));
        }
    }
}
