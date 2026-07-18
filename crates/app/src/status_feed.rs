//! AI 서비스 상태 피드 — status.claude.com / status.openai.com (Statuspage v2 JSON).
//! 하단 상태바의 서비스 점등과 홈 「AI 공지」 카드(최신 인시던트 3건씩)가 쓴다
//! (2026-07-18 사용자). 백그라운드 워커 1개가 5분마다 두 페이지를 폴링해 mpsc로
//! 스냅샷을 보낸다 — UI 스레드 네트워크 금지 관례. 실패 시 해당 provider만 None
//! (오프라인이어도 앱 동작 무영향, 표시만 "확인 불가").

use std::sync::mpsc::Receiver;
use std::time::Duration;

pub const CLAUDE_STATUS_URL: &str = "https://status.claude.com";
pub const OPENAI_STATUS_URL: &str = "https://status.openai.com";
/// 폴링 주기 — 상태 페이지 부하와 신선도의 절충(인시던트 대응 용도로 충분).
const POLL_INTERVAL: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 홈 공지 카드 수 (provider당, 2026-07-18 사용자: "최신 3개씩").
const INCIDENTS_PER_PROVIDER: usize = 3;

/// Statuspage `status.indicator` 매핑.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceIndicator {
    Operational,
    Minor,
    Major,
    Critical,
    Unknown,
}

impl ServiceIndicator {
    fn from_api(indicator: &str) -> Self {
        match indicator {
            "none" => Self::Operational,
            "minor" => Self::Minor,
            "major" => Self::Major,
            "critical" => Self::Critical,
            _ => Self::Unknown,
        }
    }
}

/// 인시던트 1건 — 홈 공지 카드 1장.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentNotice {
    pub title: String,
    /// Statuspage 원문 상태 (resolved/investigating/identified/monitoring/postmortem).
    pub status: String,
    /// created_at의 날짜 부분("2026-07-17") — 카드 하단 표기용.
    pub date: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderStatus {
    pub indicator: ServiceIndicator,
    /// 상태 페이지의 요약 문구("All Systems Operational" 등) — hover 표기.
    pub description: String,
    pub incidents: Vec<IncidentNotice>,
}

/// 워커 → App 스냅샷. provider별 None = 이번 라운드 조회 실패(이전 값 유지는 App 몫).
#[derive(Debug, Clone, Default)]
pub struct StatusFeedSnapshot {
    pub claude: Option<ProviderStatus>,
    pub openai: Option<ProviderStatus>,
}

/// 백그라운드 폴링 워커를 띄우고 수신 채널을 돌려준다. 앱 수명 내내 돈다 —
/// App(수신측)이 드롭되면 send 실패로 스스로 종료한다.
pub fn spawn(egui_ctx: egui::Context) -> Receiver<StatusFeedSnapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("status-feed".into())
        .spawn(move || {
            let agent = ureq::builder().timeout(HTTP_TIMEOUT).build();
            loop {
                let snapshot = StatusFeedSnapshot {
                    claude: fetch_provider(&agent, CLAUDE_STATUS_URL)
                        .map_err(|e| tracing::debug!("Claude 상태 조회 실패: {e:#}"))
                        .ok(),
                    openai: fetch_provider(&agent, OPENAI_STATUS_URL)
                        .map_err(|e| tracing::debug!("OpenAI 상태 조회 실패: {e:#}"))
                        .ok(),
                };
                if tx.send(snapshot).is_err() {
                    return; // App 종료
                }
                egui_ctx.request_repaint();
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("status-feed 워커 spawn 실패: {e}");
    }
    rx
}

fn fetch_provider(agent: &ureq::Agent, base: &str) -> anyhow::Result<ProviderStatus> {
    let status_json = agent
        .get(&format!("{base}/api/v2/status.json"))
        .call()?
        .into_string()?;
    let (indicator, description) = parse_status(&status_json)?;
    let incidents_json = agent
        .get(&format!("{base}/api/v2/incidents.json"))
        .call()?
        .into_string()?;
    let incidents = parse_incidents(&incidents_json, base)?;
    Ok(ProviderStatus {
        indicator,
        description,
        incidents,
    })
}

/// `/api/v2/status.json` → (indicator, description).
fn parse_status(json: &str) -> anyhow::Result<(ServiceIndicator, String)> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let status = value
        .get("status")
        .ok_or_else(|| anyhow::anyhow!("status 필드 없음"))?;
    let indicator = status
        .get("indicator")
        .and_then(|v| v.as_str())
        .map(ServiceIndicator::from_api)
        .unwrap_or(ServiceIndicator::Unknown);
    let description = status
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    Ok((indicator, description))
}

/// `/api/v2/incidents.json` → 최신 3건. 링크는 shortlink 우선, 없으면(OpenAI가 그렇다)
/// Statuspage 표준 경로 `{base}/incidents/{id}`로 조립한다.
fn parse_incidents(json: &str, base: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let incidents = value
        .get("incidents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("incidents 배열 없음"))?;
    Ok(incidents
        .iter()
        .filter_map(|incident| {
            let title = incident.get("name")?.as_str()?.to_owned();
            let status = incident
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let date = incident
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(10).collect())
                .unwrap_or_default();
            let url = incident
                .get("shortlink")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    incident
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| format!("{base}/incidents/{id}"))
                })
                .unwrap_or_else(|| base.to_owned());
            Some(IncidentNotice {
                title,
                status,
                date,
                url,
            })
        })
        .take(INCIDENTS_PER_PROVIDER)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status는_indicator와_설명을_뽑는다() {
        let json = r#"{"page":{"name":"Claude"},"status":{"indicator":"none","description":"All Systems Operational"}}"#;
        let (indicator, description) = parse_status(json).unwrap();
        assert_eq!(indicator, ServiceIndicator::Operational);
        assert_eq!(description, "All Systems Operational");
        let json = r#"{"status":{"indicator":"major","description":"Partial outage"}}"#;
        assert_eq!(parse_status(json).unwrap().0, ServiceIndicator::Major);
        // 미지의 indicator는 Unknown — 표시 쪽에서 회색 점.
        let json = r#"{"status":{"indicator":"weird","description":""}}"#;
        assert_eq!(parse_status(json).unwrap().0, ServiceIndicator::Unknown);
    }

    #[test]
    fn parse_incidents는_최신_3건과_링크_폴백을_처리한다() {
        // Claude형(shortlink 있음) 2건 + OpenAI형(shortlink 없음, id만) 1건 + 초과 1건.
        let json = r#"{"incidents":[
            {"name":"A","status":"resolved","created_at":"2026-07-17T18:32:32.629Z","shortlink":"https://stspg.io/a"},
            {"name":"B","status":"investigating","created_at":"2026-07-17T06:47:54.909Z"},
            {"name":"C","status":"resolved","created_at":"2026-07-16T22:54:01Z","id":"abc123"},
            {"name":"D","status":"resolved","created_at":"2026-07-15T00:00:00Z"}
        ]}"#;
        let notices = parse_incidents(json, "https://status.openai.com").unwrap();
        assert_eq!(notices.len(), 3, "최신 3건만");
        assert_eq!(notices[0].url, "https://stspg.io/a");
        assert_eq!(notices[0].date, "2026-07-17");
        assert_eq!(
            notices[1].url, "https://status.openai.com",
            "shortlink·id 둘 다 없으면 베이스"
        );
        assert_eq!(
            notices[2].url, "https://status.openai.com/incidents/abc123",
            "shortlink 없으면 id로 조립"
        );
    }
}
