//! RedactionService (설계문서 7장). 등록 기반 streaming redaction —
//! chunk 경계 분할·ANSI escape 삽입·base64/URL/JSON-escape 변형에 대응한다.
//! 패턴 corpus 확장·encrypted raw log는 PR-22.

use std::sync::{Arc, Mutex};

use crate::SecretString;

const REPLACEMENT: &[u8] = b"[REDACTED]";
/// 치환 오탐을 피하기 위한 최소 등록 길이 (이보다 짧은 값은 힌트 수준)
const MIN_SECRET_LEN: usize = 6;

/// 프로세스 전역 secret 패턴 레지스트리. 등록은 UI/worker 어디서든,
/// 매칭은 StreamRedactor가 수행한다.
#[derive(Clone, Default)]
pub struct RedactionService {
    inner: Arc<Mutex<Patterns>>,
}

#[derive(Default)]
struct Patterns {
    /// 원본 + 파생 변형. 긴 패턴 우선 매칭.
    entries: Vec<Vec<u8>>,
    max_len: usize,
}

impl RedactionService {
    pub fn new() -> Self {
        Self::default()
    }

    /// secret과 그 변형(base64, URL-encoded, JSON-escaped)을 등록한다 (설계문서 7장).
    pub fn register(&self, secret: &SecretString) {
        let plain = secret.expose().as_bytes();
        if plain.len() < MIN_SECRET_LEN {
            return;
        }
        let mut variants: Vec<Vec<u8>> = vec![
            plain.to_vec(),
            base64_encode(plain).into_bytes(),
            url_encode(plain, false).into_bytes(),
            url_encode(plain, true).into_bytes(), // 소문자 %xx 인코더 대응
            json_escape(secret.expose()).into_bytes(),
        ];
        variants.dedup();
        let mut patterns = self.inner.lock().expect("redaction patterns lock");
        for variant in variants.drain(..) {
            if variant.len() >= MIN_SECRET_LEN && !patterns.entries.contains(&variant) {
                patterns.max_len = patterns.max_len.max(variant.len());
                patterns.entries.push(variant);
            }
        }
        // 긴 패턴 우선 (부분 문자열 관계일 때 넓게 지우도록)
        patterns
            .entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.len()));
    }

    /// secret 값이 JSON이면(예: OAuth 토큰 blob — PR-18) 안의 문자열 필드들을
    /// 개별 패턴으로도 등록한다. 로그에는 blob 전체가 아니라 access token 같은
    /// 개별 값이 찍히기 때문. JSON이 아니면 아무것도 하지 않는다 (register와 병행 사용).
    pub fn register_json_fields(&self, secret: &SecretString) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(secret.expose()) else {
            return;
        };
        fn walk(value: &serde_json::Value, service: &RedactionService) {
            match value {
                serde_json::Value::String(s) => {
                    service.register(&SecretString::new(s.clone()));
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, service);
                    }
                }
                serde_json::Value::Object(map) => {
                    for item in map.values() {
                        walk(item, service);
                    }
                }
                _ => {}
            }
        }
        walk(&value, self);
    }

    pub fn stream_redactor(&self) -> StreamRedactor {
        StreamRedactor {
            service: self.clone(),
            carry: Vec::new(),
        }
    }
}

/// 세션 output 스트림 하나의 redaction 상태 (lookbehind carry 보유).
pub struct StreamRedactor {
    service: RedactionService,
    /// chunk 경계에 걸친 secret 대응 — 아직 배출하지 않은 원본 꼬리
    carry: Vec<u8>,
}

impl StreamRedactor {
    /// chunk를 redact해 배출 가능한 앞부분을 돌려준다.
    /// 꼬리(최대 secret 길이 − 1, stripped 기준)는 다음 chunk와 합쳐 재검사한다.
    pub fn redact_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        let (mut redacted, stripped_len) = {
            let patterns = self.service.inner.lock().expect("redaction patterns lock");
            redact_buffer(&self.carry, &patterns.entries)
        };
        let max_len = self
            .service
            .inner
            .lock()
            .expect("redaction patterns lock")
            .max_len;
        if max_len == 0 {
            self.carry.clear();
            return std::mem::take(&mut redacted);
        }
        // stripped 기준 max_len-1 만큼을 carry로 남긴다.
        // escape만 계속 오는 병리적 스트림으로 carry가 무한히 크지 않게 cap.
        let keep_stripped = max_len.saturating_sub(1);
        let carry_start = origin_index_for_stripped_suffix(&redacted, keep_stripped);
        const CARRY_CAP: usize = 16 * 1024;
        let carry_start = carry_start.max(redacted.len().saturating_sub(CARRY_CAP));
        let _ = stripped_len;
        self.carry = redacted.split_off(carry_start);
        redacted
    }

    /// 스트림 종료 — 남은 carry를 마지막 검사 후 배출한다.
    pub fn flush(&mut self) -> Vec<u8> {
        let patterns = self.service.inner.lock().expect("redaction patterns lock");
        let (redacted, _) = redact_buffer(&self.carry, &patterns.entries);
        self.carry.clear();
        redacted
    }
}

/// buffer에서 패턴을 찾아 [REDACTED]로 치환한다.
/// 매칭은 ANSI escape를 제거한 텍스트에서 하고(escape 삽입 우회 방지 — 7장),
/// 치환은 원본 범위(escape 포함)에 적용한다. (stripped 길이도 반환)
fn redact_buffer(buffer: &[u8], patterns: &[Vec<u8>]) -> (Vec<u8>, usize) {
    let (stripped, index_map) = strip_ansi_with_map(buffer);
    if patterns.is_empty() || stripped.is_empty() {
        return (buffer.to_vec(), stripped.len());
    }
    // 원본 기준 (start, end) 치환 구간 수집
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for pattern in patterns {
        let mut from = 0;
        while let Some(pos) = find(&stripped[from..], pattern) {
            let start = from + pos;
            let end = start + pattern.len();
            // 원본 범위: 매치 첫 바이트의 원본 위치 ~ 마지막 바이트의 원본 위치+1
            spans.push((index_map[start], index_map[end - 1] + 1));
            from = end;
        }
    }
    if spans.is_empty() {
        return (buffer.to_vec(), stripped.len());
    }
    spans.sort_unstable();
    // 겹치는 구간 병합
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
            _ => merged.push(span),
        }
    }
    let mut out = Vec::with_capacity(buffer.len());
    let mut cursor = 0;
    for (start, end) in merged {
        out.extend_from_slice(&buffer[cursor..start]);
        out.extend_from_slice(REPLACEMENT);
        cursor = end;
    }
    out.extend_from_slice(&buffer[cursor..]);
    (out, stripped.len())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// ANSI escape(CSI/OSC/2byte)를 제거한 바이트열과, stripped 인덱스 → 원본 인덱스 맵.
fn strip_ansi_with_map(buffer: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut stripped = Vec::with_capacity(buffer.len());
    let mut map = Vec::with_capacity(buffer.len());
    let mut i = 0;
    while i < buffer.len() {
        if buffer[i] == 0x1b {
            match buffer.get(i + 1) {
                Some(b'[') => {
                    // CSI: 최종 바이트(0x40..=0x7e)까지
                    i += 2;
                    while i < buffer.len() && !(0x40..=0x7e).contains(&buffer[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    // OSC: 헤더/종결자(BEL 또는 ESC\)만 제거하고 payload는
                    // 매칭 대상에 포함한다 — title 등에 실린 secret도 잡아야 한다.
                    i += 2;
                    while i < buffer.len() {
                        if buffer[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if buffer[i] == 0x1b && buffer.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        stripped.push(buffer[i]);
                        map.push(i);
                        i += 1;
                    }
                }
                Some(_) => i += 2,
                None => i += 1,
            }
        } else {
            stripped.push(buffer[i]);
            map.push(i);
            i += 1;
        }
    }
    (stripped, map)
}

/// redacted(원본 좌표계)에서 stripped 기준 뒤에서 keep개에 해당하는 원본 시작 인덱스.
fn origin_index_for_stripped_suffix(buffer: &[u8], keep: usize) -> usize {
    if keep == 0 {
        return buffer.len();
    }
    let (_, map) = strip_ansi_with_map(buffer);
    if map.len() <= keep {
        0
    } else {
        map[map.len() - keep]
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn url_encode(input: &[u8], lowercase: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ if lowercase => out.push_str(&format!("%{byte:02x}")),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_with(secret: &str) -> RedactionService {
        let service = RedactionService::new();
        service.register(&SecretString::new(secret.into()));
        service
    }

    fn redact_all(redactor: &mut StreamRedactor, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(redactor.redact_chunk(chunk));
        }
        out.extend(redactor.flush());
        out
    }

    #[test]
    fn json_blob의_문자열_필드도_개별_등록() {
        // OAuth 토큰 blob (PR-18): 로그에는 blob이 아닌 개별 토큰이 찍힌다
        let service = RedactionService::new();
        let blob = r#"{"access_token":"at-secret-12345","refresh_token":"rt-secret-67890","expires_in_secs":3600}"#;
        service.register_json_fields(&SecretString::new(blob.to_owned()));
        let mut r = service.stream_redactor();
        let out = redact_all(
            &mut r,
            &[b"Authorization: Bearer at-secret-12345\nrt-secret-67890\n"],
        );
        assert_eq!(out, b"Authorization: Bearer [REDACTED]\n[REDACTED]\n");
        // JSON이 아니면 아무것도 등록하지 않는다
        let service = RedactionService::new();
        service.register_json_fields(&SecretString::new("not json at all".to_owned()));
        let mut r = service.stream_redactor();
        let out = redact_all(&mut r, &[b"not json at all\n"]);
        assert_eq!(out, b"not json at all\n");
    }

    #[test]
    fn 단순_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"key=sk-abcdef123456 done\n"]);
        assert_eq!(out, b"key=[REDACTED] done\n");
    }

    #[test]
    fn chunk_경계_분할_대응() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"key=sk-abc", b"def123456 done\n"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn ansi_escape_삽입_우회_차단() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        // secret 중간에 색상 escape 삽입
        let out = redact_all(&mut r, &[b"sk-abc\x1b[31mdef123456\x1b[0m end"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("def123456"), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn base64와_url_변형_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let b64 = base64_encode(b"sk-abcdef123456");
        let out = redact_all(&mut r, &[format!("b64={b64} end\n").as_bytes()]);
        assert!(!String::from_utf8_lossy(&out).contains(&b64));

        let mut r = service_with("sk+key/with special").stream_redactor();
        let url = url_encode(b"sk+key/with special", false);
        let out = redact_all(&mut r, &[format!("url={url} end\n").as_bytes()]);
        assert!(!String::from_utf8_lossy(&out).contains(&url));
    }

    #[test]
    fn 짧은_secret은_미등록() {
        let service = service_with("abc");
        let mut r = service.stream_redactor();
        let out = redact_all(&mut r, &[b"abc def"]);
        assert_eq!(out, b"abc def"); // 등록 안 됨 — 그대로
    }

    #[test]
    fn 등록_전_출력은_그대로_이후_출력만_치환() {
        let service = RedactionService::new();
        let mut r = service.stream_redactor();
        let before = r.redact_chunk(b"plain-before ");
        service.register(&SecretString::new("sk-abcdef123456".into()));
        let mut out = before;
        out.extend(r.redact_chunk(b"now sk-abcdef123456 leaks?"));
        out.extend(r.flush());
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
    }

    #[test]
    fn osc_payload_안의_secret도_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"\x1b]0;title sk-abcdef123456\x07visible"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
    }

    #[test]
    fn 여러_출현과_겹침_병합() {
        let mut r = service_with("secret-value-1").stream_redactor();
        let out = redact_all(&mut r, &[b"a secret-value-1 b secret-value-1 c"]);
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text.matches("[REDACTED]").count(), 2);
        assert!(!text.contains("secret-value-1"));
    }
}
