//! 로컬 LLM 감지 (PR-L1) — ollama(`GET {base}/api/tags`)와 OpenAI 호환
//! 엔드포인트(`GET {base}/v1/models`)에서 사용 가능한 모델 목록을 가져온다.
//! Agents 창 OSS 프로바이더의 모델 후보로 쓰인다(PR-L3 배선). UI 스레드 네트워크
//! 금지 관례 — 일회성 워커(notice_translate 스타일)가 감지 후 mpsc로 결과를
//! 보내고 종료한다. 연결 실패는 미설치/미실행으로 정상 처리(None), 에러 아님.
//! 커스텀 OpenAI 호환 쪽 목록 조회는 배선하지 않는다 — 사용자 지시(2026-07-18):
//! 커스텀 API는 검색 없이 입력값이 다음 실행에 적용되면 충분.

use std::io::Read as _;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// ollama 기본 베이스 URL — 호출측(설정 UI)이 미지정 시 이 값을 넘긴다.
pub const DEFAULT_OLLAMA_BASE: &str = "http://localhost:11434";
/// 로컬/사설 엔드포인트 조회 상한 — 미실행이면 connection refused로 즉시 끝난다.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// 모델 목록 응답은 파싱 전에 이 크기로 자른다.
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
/// 한 endpoint가 반환할 수 있는 원시 모델 항목 수.
const MAX_MODEL_ITEMS: usize = 256;
/// 모델 이름/ID 하나의 UTF-8 바이트 상한.
const MAX_MODEL_FIELD_BYTES: usize = 4 * 1024;
/// 스냅샷 한 endpoint가 보관할 모델 문자열의 합계 상한.
const MAX_MODEL_AGGREGATE_BYTES: usize = 256 * 1024;

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
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("local-llm-detect".into())
        .spawn(move || {
            let agent = ureq::Agent::config_builder()
                .max_redirects(5)
                .timeout_global(Some(HTTP_TIMEOUT))
                .build()
                .new_agent();
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
    if spawned.is_err() {
        tracing::warn!("local_llm_detect_worker_spawn_failed");
    }
    rx
}

/// ollama 모델 목록 조회. 실패(미실행 포함)는 debug 로그 후 None — 정상 경로.
fn fetch_ollama_models(agent: &ureq::Agent, base: &str) -> Option<Vec<String>> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let response = agent
        .get(&url)
        .call()
        .map_err(|_| tracing::debug!("local_llm_ollama_request_failed"))
        .ok()?;
    let json = read_bounded_utf8(response.into_body().into_reader(), MAX_HTTP_BODY_BYTES)
        .map_err(|_| tracing::debug!("local_llm_ollama_response_rejected"))
        .ok()?;
    parse_ollama_tags(&json)
        .map_err(|_| tracing::debug!("local_llm_ollama_projection_rejected"))
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
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let response = request
        .call()
        .map_err(|_| tracing::debug!("local_llm_openai_request_failed"))
        .ok()?;
    let json = read_bounded_utf8(response.into_body().into_reader(), MAX_HTTP_BODY_BYTES)
        .map_err(|_| tracing::debug!("local_llm_openai_response_rejected"))
        .ok()?;
    parse_openai_models(&json)
        .map_err(|_| tracing::debug!("local_llm_openai_projection_rejected"))
        .ok()
}

fn read_bounded_utf8(reader: impl std::io::Read, max_bytes: usize) -> anyhow::Result<String> {
    let read_limit = u64::try_from(max_bytes)
        .map_err(|_| anyhow::anyhow!("local_llm_response_limit_invalid"))?
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("local_llm_response_read_failed"))?;
    anyhow::ensure!(
        bytes.len() <= max_bytes,
        "local_llm_response_bytes_exceeded"
    );
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("local_llm_response_utf8_invalid"))
}

fn project_models(json: &str, array_key: &str, field_key: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        json.len() <= MAX_HTTP_BODY_BYTES,
        "local_llm_response_bytes_exceeded"
    );
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| anyhow::anyhow!("local_llm_json_invalid"))?;
    let items = value
        .get(array_key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("local_llm_model_array_missing"))?;
    anyhow::ensure!(
        items.len() <= MAX_MODEL_ITEMS,
        "local_llm_model_items_exceeded"
    );

    let mut aggregate_bytes = 0usize;
    let mut models = Vec::with_capacity(items.len());
    for item in items {
        let Some(model) = item.get(field_key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        anyhow::ensure!(
            model.len() <= MAX_MODEL_FIELD_BYTES,
            "local_llm_model_field_bytes_exceeded"
        );
        aggregate_bytes = aggregate_bytes
            .checked_add(model.len())
            .ok_or_else(|| anyhow::anyhow!("local_llm_model_aggregate_bytes_exceeded"))?;
        anyhow::ensure!(
            aggregate_bytes <= MAX_MODEL_AGGREGATE_BYTES,
            "local_llm_model_aggregate_bytes_exceeded"
        );
        models.push(model.to_owned());
    }
    Ok(models)
}

/// `GET /api/tags` 응답 → 모델 이름 목록 (순수 파싱). name 없는 항목은 건너뛴다.
fn parse_ollama_tags(json: &str) -> anyhow::Result<Vec<String>> {
    project_models(json, "models", "name")
}

/// `GET /v1/models` 응답 → 모델 id 목록 (순수 파싱). id 없는 항목은 건너뛴다.
fn parse_openai_models(json: &str) -> anyhow::Result<Vec<String>> {
    project_models(json, "data", "id")
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

    #[test]
    fn 응답_바이트_상한은_정확히_허용하고_한_바이트_초과를_거부한다() {
        let exact = vec![b'x'; MAX_HTTP_BODY_BYTES];
        assert_eq!(
            read_bounded_utf8(std::io::Cursor::new(exact), MAX_HTTP_BODY_BYTES)
                .unwrap()
                .len(),
            MAX_HTTP_BODY_BYTES
        );
        let oversized = vec![b'x'; MAX_HTTP_BODY_BYTES + 1];
        assert_eq!(
            read_bounded_utf8(std::io::Cursor::new(oversized), MAX_HTTP_BODY_BYTES)
                .unwrap_err()
                .to_string(),
            "local_llm_response_bytes_exceeded"
        );
        assert_eq!(
            read_bounded_utf8(std::io::Cursor::new([0xff]), MAX_HTTP_BODY_BYTES)
                .unwrap_err()
                .to_string(),
            "local_llm_response_utf8_invalid"
        );
    }

    #[test]
    fn 모델_항목_상한은_정확히_허용하고_초과를_거부한다() {
        let exact = serde_json::json!({
            "models": (0..MAX_MODEL_ITEMS)
                .map(|index| serde_json::json!({"name": format!("m{index}")}))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parse_ollama_tags(&exact.to_string()).unwrap().len(),
            MAX_MODEL_ITEMS
        );
        let oversized = serde_json::json!({
            "models": (0..=MAX_MODEL_ITEMS)
                .map(|index| serde_json::json!({"name": format!("m{index}")}))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parse_ollama_tags(&oversized.to_string())
                .unwrap_err()
                .to_string(),
            "local_llm_model_items_exceeded"
        );
    }

    #[test]
    fn 모델_필드와_합계_상한을_초과하면_스냅샷을_거부한다() {
        let exact_field = serde_json::json!({
            "data": [{"id": "x".repeat(MAX_MODEL_FIELD_BYTES)}]
        });
        assert_eq!(
            parse_openai_models(&exact_field.to_string()).unwrap()[0].len(),
            MAX_MODEL_FIELD_BYTES
        );
        let oversized_field = serde_json::json!({
            "data": [{"id": "x".repeat(MAX_MODEL_FIELD_BYTES + 1)}]
        });
        assert_eq!(
            parse_openai_models(&oversized_field.to_string())
                .unwrap_err()
                .to_string(),
            "local_llm_model_field_bytes_exceeded"
        );

        let exact_aggregate = serde_json::json!({
            "models": (0..MAX_MODEL_AGGREGATE_BYTES / MAX_MODEL_FIELD_BYTES)
                .map(|_| serde_json::json!({"name": "x".repeat(MAX_MODEL_FIELD_BYTES)}))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parse_ollama_tags(&exact_aggregate.to_string())
                .unwrap()
                .iter()
                .map(String::len)
                .sum::<usize>(),
            MAX_MODEL_AGGREGATE_BYTES
        );

        let aggregate = serde_json::json!({
            "models": (0..=MAX_MODEL_AGGREGATE_BYTES / MAX_MODEL_FIELD_BYTES)
                .map(|_| serde_json::json!({"name": "x".repeat(MAX_MODEL_FIELD_BYTES)}))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parse_ollama_tags(&aggregate.to_string())
                .unwrap_err()
                .to_string(),
            "local_llm_model_aggregate_bytes_exceeded"
        );
    }

    #[test]
    fn 소스는_무제한_http_문자열과_무제한_결과_채널을_사용하지_않는다() {
        let source = include_str!("local_llm.rs");
        let unbounded_body = [".into_", "string()"].concat();
        let unbounded_channel = ["mpsc::", "channel()"].concat();
        let bounded_channel = ["mpsc::sync_", "channel(1)"].concat();
        assert!(!source.contains(&unbounded_body));
        assert!(!source.contains(&unbounded_channel));
        assert!(source.contains(&bounded_channel));
    }
}
