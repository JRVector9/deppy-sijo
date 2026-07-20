//! 홈 업데이트 피드 — Claude/OpenAI/GitHub Statuspage, Hugging Face trending
//! models, Grok 공식 상태 RSS. 하단 상태바의 서비스 점등과 홈 업데이트 목록이 쓴다
//! (2026-07-18 사용자). 백그라운드 워커 1개가 폴링해 mpsc로 스냅샷을 보낸다 —
//! UI 스레드 네트워크 금지 관례. 주기는 이원화: **상태 점등 5분**(터미널 작업용
//! 신선도), **공지 60분**(사용자 지정) + 홈의 수동 갱신 버튼(refresh 채널).
//! 실패 시 해당 provider만 None (오프라인이어도 앱 동작 무영향).

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

pub const CLAUDE_STATUS_URL: &str = "https://status.claude.com";
pub const OPENAI_STATUS_URL: &str = "https://status.openai.com";
pub const GITHUB_STATUS_URL: &str = "https://www.githubstatus.com";
const HUGGING_FACE_MODELS_API: &str =
    "https://huggingface.co/api/models?sort=trendingScore&direction=-1&limit=5";
const GROK_STATUS_RSS: &str = "https://status.x.ai/feed.xml";
/// 상태(점등) 폴링 주기 — 장애 감지용이라 짧게 유지.
const STATUS_INTERVAL: Duration = Duration::from_secs(300);
/// 공지(인시던트 목록) 갱신 주기 (2026-07-18 사용자: 60분).
const INCIDENTS_INTERVAL: Duration = Duration::from_secs(3600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 홈 공지 카드 수 (provider당, 2026-07-20 사용자: "5줄").
const INCIDENTS_PER_PROVIDER: usize = 5;

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
    pub github: Option<ProviderStatus>,
    pub hugging_face: Option<ProviderStatus>,
    pub grok: Option<ProviderStatus>,
}

/// 백그라운드 폴링 워커를 띄우고 (스냅샷 수신, 수동 갱신 송신) 채널 쌍을 돌려준다.
/// 앱 수명 내내 돈다 — App(수신측)이 드롭되면 send 실패로 스스로 종료한다.
/// 수동 갱신 신호가 오면 즉시 상태+공지를 모두 다시 가져온다.
pub fn spawn(egui_ctx: egui::Context) -> (Receiver<StatusFeedSnapshot>, Sender<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("status-feed".into())
        .spawn(move || {
            let agent = ureq::builder().timeout(HTTP_TIMEOUT).build();
            // 공지는 60분 주기 — 사이 틱에서는 마지막 성공 목록을 스냅샷에 실어
            // 보낸다(상태만 갱신돼도 공지가 사라지지 않게).
            let mut incidents_at: Option<Instant> = None;
            let mut claude_incidents: Vec<IncidentNotice> = Vec::new();
            let mut openai_incidents: Vec<IncidentNotice> = Vec::new();
            let mut hugging_face_updates: Option<Vec<IncidentNotice>> = None;
            let mut grok_updates: Option<Vec<IncidentNotice>> = None;
            loop {
                let incidents_due =
                    incidents_at.is_none_or(|at| at.elapsed() >= INCIDENTS_INTERVAL);
                if incidents_due {
                    if let Ok(list) = fetch_incidents(&agent, CLAUDE_STATUS_URL)
                        .map_err(|e| tracing::debug!("Claude 공지 조회 실패: {e:#}"))
                    {
                        claude_incidents = list;
                    }
                    if let Ok(list) = fetch_incidents(&agent, OPENAI_STATUS_URL)
                        .map_err(|e| tracing::debug!("OpenAI 공지 조회 실패: {e:#}"))
                    {
                        openai_incidents = list;
                    }
                    if let Ok(list) = fetch_hugging_face_models(&agent)
                        .map_err(|e| tracing::debug!("Hugging Face 모델 조회 실패: {e:#}"))
                    {
                        hugging_face_updates = Some(list);
                    }
                    if let Ok(list) = fetch_grok_status(&agent)
                        .map_err(|e| tracing::debug!("Grok 상태 RSS 조회 실패: {e:#}"))
                    {
                        grok_updates = Some(list);
                    }
                    incidents_at = Some(Instant::now());
                }
                let snapshot = StatusFeedSnapshot {
                    claude: fetch_status(&agent, CLAUDE_STATUS_URL)
                        .map_err(|e| tracing::debug!("Claude 상태 조회 실패: {e:#}"))
                        .ok()
                        .map(|(indicator, description)| ProviderStatus {
                            indicator,
                            description,
                            incidents: claude_incidents.clone(),
                        }),
                    openai: fetch_status(&agent, OPENAI_STATUS_URL)
                        .map_err(|e| tracing::debug!("OpenAI 상태 조회 실패: {e:#}"))
                        .ok()
                        .map(|(indicator, description)| ProviderStatus {
                            indicator,
                            description,
                            incidents: openai_incidents.clone(),
                        }),
                    github: fetch_status(&agent, GITHUB_STATUS_URL)
                        .map_err(|e| tracing::debug!("GitHub 상태 조회 실패: {e:#}"))
                        .ok()
                        .map(|(indicator, description)| ProviderStatus {
                            indicator,
                            description,
                            incidents: Vec::new(),
                        }),
                    hugging_face: hugging_face_updates.as_ref().map(|updates| ProviderStatus {
                        indicator: ServiceIndicator::Operational,
                        description: "Trending models".to_owned(),
                        incidents: updates.clone(),
                    }),
                    grok: grok_updates.as_ref().map(|updates| ProviderStatus {
                        indicator: ServiceIndicator::Operational,
                        description: "Grok status updates".to_owned(),
                        incidents: updates.clone(),
                    }),
                };
                if tx.send(snapshot).is_err() {
                    return; // App 종료
                }
                egui_ctx.request_repaint();
                // 상태 주기만큼 대기하되, 수동 갱신 신호가 오면 즉시 깨어나
                // 공지까지 강제 재조회한다(recv_timeout이 sleep 역할).
                match refresh_rx.recv_timeout(STATUS_INTERVAL) {
                    Ok(()) => {
                        while refresh_rx.try_recv().is_ok() {} // 연타 병합
                        incidents_at = None;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("status-feed 워커 spawn 실패: {e}");
    }
    (rx, refresh_tx)
}

fn fetch_status(agent: &ureq::Agent, base: &str) -> anyhow::Result<(ServiceIndicator, String)> {
    let status_json = agent
        .get(&format!("{base}/api/v2/status.json"))
        .call()?
        .into_string()?;
    parse_status(&status_json)
}

fn fetch_incidents(agent: &ureq::Agent, base: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let incidents_json = agent
        .get(&format!("{base}/api/v2/incidents.json"))
        .call()?
        .into_string()?;
    parse_incidents(&incidents_json, base)
}

fn fetch_hugging_face_models(agent: &ureq::Agent) -> anyhow::Result<Vec<IncidentNotice>> {
    let json = agent
        .get(HUGGING_FACE_MODELS_API)
        .set("Accept", "application/json")
        .set("User-Agent", "Deppy-Sijo/External-Updates")
        .call()?
        .into_string()?;
    parse_hugging_face_models(&json)
}

fn fetch_grok_status(agent: &ureq::Agent) -> anyhow::Result<Vec<IncidentNotice>> {
    let json = agent
        .get(GROK_STATUS_RSS)
        .set(
            "Accept",
            "application/rss+xml, application/xml;q=0.9, text/xml;q=0.8",
        )
        .set("User-Agent", "Deppy-Sijo/External-Updates")
        .call()?
        .into_string()?;
    parse_grok_status_rss(&json)
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

/// `/api/v2/incidents.json` → 최신 5건. 링크는 shortlink 우선, 없으면(OpenAI가 그렇다)
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

fn parse_hugging_face_models(json: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let models: Vec<serde_json::Value> = serde_json::from_str(json)?;
    Ok(models
        .into_iter()
        .filter_map(|model| {
            let title = model
                .get("id")
                .or_else(|| model.get("modelId"))?
                .as_str()?
                .to_owned();
            let date = model
                .get("createdAt")
                .and_then(|value| value.as_str())
                .map(|value| value.chars().take(10).collect())
                .unwrap_or_default();
            Some(IncidentNotice {
                url: format!("https://huggingface.co/{title}"),
                title,
                status: "trending".to_owned(),
                date,
            })
        })
        .take(INCIDENTS_PER_PROVIDER)
        .collect())
}

#[derive(serde::Deserialize)]
struct GrokRss {
    channel: GrokRssChannel,
}

#[derive(serde::Deserialize)]
struct GrokRssChannel {
    #[serde(default)]
    item: Vec<GrokRssItem>,
}

#[derive(serde::Deserialize)]
struct GrokRssItem {
    title: String,
    link: String,
    #[serde(rename = "pubDate", default)]
    published_at: String,
}

fn parse_grok_status_rss(xml: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let feed: GrokRss = quick_xml::de::from_str(xml)?;
    let mut seen_titles = std::collections::HashSet::new();
    let mut notices = Vec::with_capacity(INCIDENTS_PER_PROVIDER);
    for item in feed.channel.item {
        let title = item.title.trim();
        let link = item.link.trim();
        if title.is_empty() || link.is_empty() {
            continue;
        }
        // Status RSS가 같은 사건을 반복 게시하는 경우가 있어, 대소문자와 연속 공백을
        // 무시한 제목 기준으로 첫 항목만 남긴다. 원래 최신순은 그대로 보존한다.
        let dedupe_key = title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if !seen_titles.insert(dedupe_key) {
            continue;
        }
        notices.push(IncidentNotice {
            title: title.to_owned(),
            status: "update".to_owned(),
            date: rss_date(&item.published_at),
            url: link.to_owned(),
        });
        if notices.len() == INCIDENTS_PER_PROVIDER {
            break;
        }
    }
    Ok(notices)
}

fn rss_date(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
    {
        return value[..10].to_owned();
    }
    let parts: Vec<_> = value
        .trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == ',')
        .split_whitespace()
        .collect();
    let [day, month, year, ..] = parts.as_slice() else {
        return String::new();
    };
    let month = match *month {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => return String::new(),
    };
    let Ok(day) = day.parse::<u8>() else {
        return String::new();
    };
    format!("{year}-{month}-{day:02}")
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
    fn parse_incidents는_최신_5건과_링크_폴백을_처리한다() {
        // Claude형(shortlink 있음)과 OpenAI형(shortlink 없음) 5건 + 초과 1건.
        let json = r#"{"incidents":[
            {"name":"A","status":"resolved","created_at":"2026-07-17T18:32:32.629Z","shortlink":"https://stspg.io/a"},
            {"name":"B","status":"investigating","created_at":"2026-07-17T06:47:54.909Z"},
            {"name":"C","status":"resolved","created_at":"2026-07-16T22:54:01Z","id":"abc123"},
            {"name":"D","status":"resolved","created_at":"2026-07-15T00:00:00Z"},
            {"name":"E","status":"resolved","created_at":"2026-07-14T00:00:00Z"},
            {"name":"F","status":"resolved","created_at":"2026-07-13T00:00:00Z"}
        ]}"#;
        let notices = parse_incidents(json, "https://status.openai.com").unwrap();
        assert_eq!(notices.len(), 5, "최신 5건만");
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

    #[test]
    fn hugging_face_trending_models를_최신_5건으로_변환한다() {
        let json = r#"[
            {"id":"org/model-a","createdAt":"2026-07-14T13:23:14.000Z"},
            {"modelId":"org/model-b","createdAt":"2026-07-13T00:00:00.000Z"},
            {"id":"org/model-c","createdAt":"2026-07-12T00:00:00.000Z"},
            {"id":"org/model-d","createdAt":"2026-07-11T00:00:00.000Z"},
            {"id":"org/model-e","createdAt":"2026-07-10T00:00:00.000Z"},
            {"id":"org/model-f","createdAt":"2026-07-09T00:00:00.000Z"}
        ]"#;
        let notices = parse_hugging_face_models(json).unwrap();
        assert_eq!(notices.len(), 5);
        assert_eq!(notices[0].title, "org/model-a");
        assert_eq!(notices[0].url, "https://huggingface.co/org/model-a");
        assert_eq!(notices[0].status, "trending");
        assert_eq!(notices[0].date, "2026-07-14");
    }

    #[test]
    fn grok_공식_rss를_중복없이_최신_5건으로_변환한다() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <rss version="2.0"><channel>
          <item><title><![CDATA[Grok Web unavailable]]></title><link>https://status.x.ai/incidents/one</link><pubDate>Mon, 20 Jul 2026 10:20:00 +0000</pubDate></item>
          <item><title>API latency</title><link>https://status.x.ai/incidents/two</link><pubDate>2026-07-19T08:00:00Z</pubDate></item>
          <item><title> api   LATENCY </title><link>https://status.x.ai/incidents/two-duplicate</link><pubDate>2026-07-19T07:00:00Z</pubDate></item>
          <item><title>Grok in X</title><link>https://status.x.ai/incidents/three</link><pubDate>Sat, 18 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Older incident</title><link>https://status.x.ai/incidents/four</link><pubDate>Fri, 17 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Account issue</title><link>https://status.x.ai/incidents/five</link><pubDate>Thu, 16 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Developer API</title><link>https://status.x.ai/incidents/six</link><pubDate>Wed, 15 Jul 2026 03:00:00 +0000</pubDate></item>
        </channel></rss>"#;
        let notices = parse_grok_status_rss(xml).unwrap();
        assert_eq!(notices.len(), 5);
        assert_eq!(notices[0].title, "Grok Web unavailable");
        assert_eq!(notices[0].url, "https://status.x.ai/incidents/one");
        assert_eq!(notices[0].status, "update");
        assert_eq!(notices[0].date, "2026-07-20");
        assert_eq!(notices[1].date, "2026-07-19");
        assert_eq!(notices[4].title, "Account issue");
        assert_eq!(
            notices.iter().filter(|n| n.title == "API latency").count(),
            1
        );
    }

    #[test]
    fn grok_rss는_빈_제목과_링크를_제외한다() {
        let xml = r#"<rss><channel>
          <item><title></title><link>https://status.x.ai/incidents/one</link></item>
          <item><title>Valid</title><link></link></item>
          <item><title>Visible</title><link>https://status.x.ai/incidents/three</link></item>
        </channel></rss>"#;
        let notices = parse_grok_status_rss(xml).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Visible");
        assert!(notices[0].date.is_empty());
    }
}
