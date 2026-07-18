//! 로컬 LLM 감지 (PR-L1) — ollama(`GET {base}/api/tags`)와 OpenAI 호환
//! 엔드포인트(`GET {base}/v1/models`)에서 사용 가능한 모델 목록을 가져온다.
//! 에이전트 설정 UI의 모델 후보로 쓰인다(PR-L3 배선 예정). UI 스레드 네트워크
//! 금지 관례 — 일회성 워커(notice_translate 스타일)가 감지 후 mpsc로 결과를
//! 보내고 종료한다. 연결 실패는 미설치/미실행으로 정상 처리(None), 에러 아님.

// PR-L3가 에이전트 설정 UI에 배선하기 전까지 bin 크레이트 미사용 pub 경고 억제 (배선 시 제거).
#![allow(dead_code)]

use std::sync::mpsc::Receiver;
use std::time::Duration;

/// ollama 기본 베이스 URL — 호출측(설정 UI)이 미지정 시 이 값을 넘긴다.
pub const DEFAULT_OLLAMA_BASE: &str = "http://localhost:11434";
/// 로컬/사설 엔드포인트 조회 상한 — 미실행이면 connection refused로 즉시 끝난다.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// 워커 → App 감지 결과. None = 조회 실패/미설정, Some(빈) = 연결됐지만 모델 없음.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLlmSnapshot {
    pub ollama: Option<Vec<String>>,
    pub custom: Option<Vec<String>>,
}

/// 일회성 감지 워커를 띄운다 — 한 번 감지해 스냅샷을 보내고 스스로 종료한다.
/// `custom`은 (base, api_key) — None이면 OpenAI 호환 감지를 건너뛴다(결과 None).
/// 호출측은 한 번에 하나만 띄운다(App이 rx 보유로 게이트 — notice_translate 관례).
pub fn spawn_detect(
    egui_ctx: egui::Context,
    ollama_base: String,
    custom: Option<(String, Option<String>)>,
) -> Receiver<LocalLlmSnapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("local-llm-detect".into())
        .spawn(move || {
            let agent = ureq::builder().timeout(HTTP_TIMEOUT).build();
            let snapshot = LocalLlmSnapshot {
                ollama: fetch_ollama_models(&agent, &ollama_base),
                custom: custom.as_ref().and_then(|(base, api_key)| {
                    fetch_openai_models(&agent, base, api_key.as_deref())
                }),
            };
            if tx.send(snapshot).is_ok() {
                egui_ctx.request_repaint();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("local-llm-detect 워커 spawn 실패: {e}");
    }
    rx
}

/// ollama 모델 목록 조회. 실패(미실행 포함)는 debug 로그 후 None — 정상 경로.
fn fetch_ollama_models(agent: &ureq::Agent, base: &str) -> Option<Vec<String>> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let response = agent
        .get(&url)
        .call()
        .map_err(|e| tracing::debug!("ollama 감지 실패({url}): {e:#}"))
        .ok()?;
    let json = response
        .into_string()
        .map_err(|e| tracing::debug!("ollama 응답 수신 실패({url}): {e:#}"))
        .ok()?;
    parse_ollama_tags(&json)
        .map_err(|e| tracing::debug!("ollama 응답 파싱 실패({url}): {e:#}"))
        .ok()
}

/// OpenAI 호환 모델 목록 조회. api_key가 있으면 Bearer 헤더를 붙인다.
fn fetch_openai_models(
    agent: &ureq::Agent,
    base: &str,
    api_key: Option<&str>,
) -> Option<Vec<String>> {
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let mut request = agent.get(&url);
    if let Some(key) = api_key {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    let response = request
        .call()
        .map_err(|e| tracing::debug!("OpenAI 호환 감지 실패({url}): {e:#}"))
        .ok()?;
    let json = response
        .into_string()
        .map_err(|e| tracing::debug!("OpenAI 호환 응답 수신 실패({url}): {e:#}"))
        .ok()?;
    parse_openai_models(&json)
        .map_err(|e| tracing::debug!("OpenAI 호환 응답 파싱 실패({url}): {e:#}"))
        .ok()
}

/// `GET /api/tags` 응답 → 모델 이름 목록 (순수 파싱). name 없는 항목은 건너뛴다.
fn parse_ollama_tags(json: &str) -> anyhow::Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let models = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("models 배열 없음"))?;
    Ok(models
        .iter()
        .filter_map(|model| model.get("name")?.as_str().map(str::to_owned))
        .collect())
}

/// `GET /v1/models` 응답 → 모델 id 목록 (순수 파싱). id 없는 항목은 건너뛴다.
fn parse_openai_models(json: &str) -> anyhow::Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let data = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("data 배열 없음"))?;
    Ok(data
        .iter()
        .filter_map(|model| model.get("id")?.as_str().map(str::to_owned))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ollama_tags는_모델_이름_목록을_뽑는다() {
        let json = r#"{"models":[
            {"name":"gpt-oss:20b","modified_at":"2026-07-01T00:00:00Z","size":13000000000},
            {"name":"qwen3:8b","size":5200000000}
        ]}"#;
        assert_eq!(
            parse_ollama_tags(json).unwrap(),
            vec!["gpt-oss:20b".to_owned(), "qwen3:8b".to_owned()]
        );
    }

    #[test]
    fn parse_ollama_tags는_빈_목록과_name_누락을_처리한다() {
        // 연결됐지만 모델 없음 — Some(빈)의 근거가 되는 빈 목록.
        assert_eq!(
            parse_ollama_tags(r#"{"models":[]}"#).unwrap(),
            Vec::<String>::new()
        );
        // name 없는 항목은 건너뛴다(전체를 버리지 않는다).
        let json = r#"{"models":[{"size":1},{"name":"a:1b"}]}"#;
        assert_eq!(parse_ollama_tags(json).unwrap(), vec!["a:1b".to_owned()]);
    }

    #[test]
    fn parse_ollama_tags는_형식_이상이면_에러다() {
        assert!(parse_ollama_tags("not json").is_err());
        assert!(
            parse_ollama_tags(r#"{"tags":[]}"#).is_err(),
            "models 배열 없음"
        );
        assert!(
            parse_ollama_tags(r#"{"models":"x"}"#).is_err(),
            "배열이 아님"
        );
    }

    #[test]
    fn parse_openai_models는_모델_id_목록을_뽑는다() {
        let json = r#"{"object":"list","data":[
            {"id":"llama-3.3-70b","object":"model","created":1700000000},
            {"id":"qwen2.5-coder","object":"model"}
        ]}"#;
        assert_eq!(
            parse_openai_models(json).unwrap(),
            vec!["llama-3.3-70b".to_owned(), "qwen2.5-coder".to_owned()]
        );
    }

    #[test]
    fn parse_openai_models는_빈_목록과_id_누락을_처리한다() {
        assert_eq!(
            parse_openai_models(r#"{"data":[]}"#).unwrap(),
            Vec::<String>::new()
        );
        let json = r#"{"data":[{"object":"model"},{"id":"m1"}]}"#;
        assert_eq!(parse_openai_models(json).unwrap(), vec!["m1".to_owned()]);
    }

    #[test]
    fn parse_openai_models는_형식_이상이면_에러다() {
        assert!(parse_openai_models("not json").is_err());
        assert!(
            parse_openai_models(r#"{"models":[]}"#).is_err(),
            "data 배열 없음"
        );
        assert!(
            parse_openai_models(r#"{"data":{}}"#).is_err(),
            "배열이 아님"
        );
    }
}
