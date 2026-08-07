//! ChatGPT 백엔드 사용량 보충 — Codex app-server가 5시간 창을 보고하지 않을 때
//! Codex CLI가 쓰는 백엔드 REST 엔드포인트를 직접 읽어 빠진 창만 메운다.
//! stablyai/orca의 `withBackendSessionWindow`와 같은 전략·같은 요청 형태다.
//!
//! app-server 응답에 5시간 창이 이미 있으면 이 모듈은 호출조차 되지 않는다
//! (app.rs의 merge 지점이 구멍이 있을 때만 `current`를 부른다). 토큰은
//! `~/.codex/auth.json`에서 읽어 요청 헤더에만 쓰고 어디에도 남기지 않는다.

use std::io::Read as _;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// 백엔드 폴링 주기. app-server 폴(60초)보다 낮은 빈도면 충분하고, 만료 토큰
/// 같은 실패도 이 주기로만 재시도해 네트워크를 두드리지 않는다.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

const FIVE_HOUR_WINDOW_SECONDS: f64 = 18_000.0;
const WEEKLY_WINDOW_SECONDS: f64 = 604_800.0;
/// app-server 분류의 1분 오차 허용과 같은 폭.
const WINDOW_TOLERANCE_SECONDS: f64 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendUsage {
    pub five_hour: Option<u8>,
    pub weekly: Option<u8>,
}

#[derive(Default)]
struct SupplementState {
    pending: Option<mpsc::Receiver<Option<BackendUsage>>>,
    /// 마지막 결과와 잰 시각 — 실패(None)도 캐시해 재시도를 주기에 묶는다.
    fetched: Option<(Instant, Option<BackendUsage>)>,
    last_request: Option<Instant>,
}

/// 백엔드에서 읽은 최신 사용량. 첫 호출이 백그라운드 fetch를 깨우고, 도착
/// 전에는 None을 돌려준다 — 호출자는 그동안 app-server 값만으로 그린다.
pub fn current(ctx: &egui::Context) -> Option<BackendUsage> {
    static STATE: OnceLock<Mutex<SupplementState>> = OnceLock::new();
    let state = STATE.get_or_init(|| Mutex::new(SupplementState::default()));
    let Ok(mut state) = state.lock() else {
        return None;
    };

    if let Some(receiver) = state.pending.as_ref() {
        match receiver.try_recv() {
            Ok(usage) => {
                state.fetched = Some((Instant::now(), usage));
                state.pending = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    let refresh_due = state
        .last_request
        .is_none_or(|requested| requested.elapsed() >= REFRESH_INTERVAL);
    if state.pending.is_none() && refresh_due {
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        if std::thread::Builder::new()
            .name("codex-backend-usage".to_owned())
            .spawn(move || {
                let usage = fetch_backend_usage();
                let _ = sender.send(usage);
                repaint.request_repaint();
            })
            .is_ok()
        {
            state.pending = Some(receiver);
            state.last_request = Some(Instant::now());
        }
    }
    state.fetched.and_then(|(_, usage)| usage)
}

fn fetch_backend_usage() -> Option<BackendUsage> {
    let auth_path = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::paths::home_dir().map(|home| home.join(".codex")))?
        .join("auth.json");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(auth_path).ok()?).ok()?;
    let tokens = auth.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?;

    // Codex CLI 자신이 보내는 헤더 구성을 그대로 쓴다 (orca도 동일).
    let mut request = ureq::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get("https://chatgpt.com/backend-api/wham/usage")
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("User-Agent", "codex-cli")
        .set("OpenAI-Beta", "codex-1")
        .set("originator", "Codex Desktop");
    if let Some(account_id) = tokens.get("account_id").and_then(serde_json::Value::as_str) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let response = request.call().ok()?;
    let mut body = String::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES)
        .read_to_string(&mut body)
        .ok()?;
    parse_backend_usage(&serde_json::from_str(&body).ok()?)
}

/// 백엔드 창을 (5시간, 주간)으로 분류한다 — app-server 분류와 같은 규칙(길이
/// 우선, 위치는 판별 불가일 때만)이되 필드가 다르다: `used_percent` +
/// `limit_window_seconds`(초 단위).
fn parse_backend_usage(json: &serde_json::Value) -> Option<BackendUsage> {
    let rate_limit = json.get("rate_limit")?;
    let used_percent = |name: &str| {
        rate_limit
            .get(name)?
            .get("used_percent")?
            .as_f64()
            .filter(|percent| percent.is_finite())
            .map(|percent| percent.clamp(0.0, 100.0).round() as u8)
    };
    let is_window = |name: &str, expected_seconds: f64| {
        rate_limit
            .get(name)
            .and_then(|window| window.get("limit_window_seconds"))
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|seconds| (seconds - expected_seconds).abs() <= WINDOW_TOLERANCE_SECONDS)
    };
    let by_duration = |expected_seconds: f64| {
        ["primary_window", "secondary_window"]
            .into_iter()
            .filter(|name| is_window(name, expected_seconds))
            .find_map(used_percent)
    };
    let positional = |name: &str| {
        (!is_window(name, FIVE_HOUR_WINDOW_SECONDS) && !is_window(name, WEEKLY_WINDOW_SECONDS))
            .then(|| used_percent(name))
            .flatten()
    };
    let five_hour = by_duration(FIVE_HOUR_WINDOW_SECONDS).or_else(|| positional("primary_window"));
    let weekly = by_duration(WEEKLY_WINDOW_SECONDS).or_else(|| positional("secondary_window"));
    (five_hour.is_some() || weekly.is_some()).then_some(BackendUsage { five_hour, weekly })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실제 백엔드 응답 모양(2026-08 pro 계정) — 주간 창 하나뿐이다. 이 응답에서
    /// 5시간 칸이 주간 값으로 채워지면 app-server 쪽과 같은 회귀다.
    #[test]
    fn 주간만_보고하는_백엔드_응답은_5시간을_비워둔다() {
        let json = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 91,
                    "limit_window_seconds": 604_800,
                    "reset_after_seconds": 71_947,
                    "reset_at": 1_786_160_724i64,
                },
                "secondary_window": null,
            },
        });
        assert_eq!(
            parse_backend_usage(&json),
            Some(BackendUsage {
                five_hour: None,
                weekly: Some(91)
            })
        );
    }

    #[test]
    fn 백엔드_창도_자리가_아니라_길이로_분류한다() {
        let cases: &[(&str, serde_json::Value, Option<BackendUsage>)] = &[
            ("rate_limit 없음", serde_json::json!({}), None),
            (
                "5시간+주간 둘 다 (plus 모양)",
                serde_json::json!({"rate_limit": {
                    "primary_window": {"used_percent": 21, "limit_window_seconds": 18_000},
                    "secondary_window": {"used_percent": 81, "limit_window_seconds": 604_800},
                }}),
                Some(BackendUsage {
                    five_hour: Some(21),
                    weekly: Some(81),
                }),
            ),
            (
                "자리가 뒤바뀌어도 길이를 따라간다",
                serde_json::json!({"rate_limit": {
                    "primary_window": {"used_percent": 81, "limit_window_seconds": 604_800},
                    "secondary_window": {"used_percent": 21, "limit_window_seconds": 18_000},
                }}),
                Some(BackendUsage {
                    five_hour: Some(21),
                    weekly: Some(81),
                }),
            ),
            (
                "길이 필드가 없으면 위치 폴백",
                serde_json::json!({"rate_limit": {
                    "primary_window": {"used_percent": 61},
                    "secondary_window": {"used_percent": 62},
                }}),
                Some(BackendUsage {
                    five_hour: Some(61),
                    weekly: Some(62),
                }),
            ),
            (
                "낯선 길이는 판별 불가 — 위치 폴백",
                serde_json::json!({"rate_limit": {
                    "primary_window": {"used_percent": 40, "limit_window_seconds": 2_592_000},
                    "secondary_window": null,
                }}),
                Some(BackendUsage {
                    five_hour: Some(40),
                    weekly: None,
                }),
            ),
            (
                "범위 밖 수치는 0~100으로 자른다",
                serde_json::json!({"rate_limit": {
                    "primary_window": {"used_percent": 120.4, "limit_window_seconds": 18_000},
                    "secondary_window": {"used_percent": -3.0, "limit_window_seconds": 604_800},
                }}),
                Some(BackendUsage {
                    five_hour: Some(100),
                    weekly: Some(0),
                }),
            ),
        ];
        for (name, json, expected) in cases {
            assert_eq!(parse_backend_usage(json), *expected, "{name}");
        }
    }
}
