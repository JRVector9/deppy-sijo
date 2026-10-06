//! 결과 본문은 건너뛰고 상태 메타데이터와 도구 입력의 해시만 추출한다.
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::hash::{Hash, Hasher};
use std::io::Read;

const INPUT_BYTES_MAX: u64 = 8 * 1024 * 1024;
const METADATA_BYTES_MAX: usize = 256 * 1024;

#[derive(Clone, Copy)]
enum Shape {
    Hook,
    Call,
    Transcript,
    Message,
    Item,
    List,
    Preview,
}

impl<'de> DeserializeSeed<'de> for Shape {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Shape {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("hook metadata")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        if matches!(self, Self::Preview) {
            let mut preview = String::new();
            for c in value.chars() {
                let c = if c.is_whitespace() || c.is_control() {
                    ' '
                } else {
                    c
                };
                if preview.len() + c.len_utf8() > 256 {
                    break;
                }
                if c != ' ' || (!preview.is_empty() && !preview.ends_with(' ')) {
                    preview.push(c);
                }
            }
            return Ok(Value::String(preview.trim().to_owned()));
        }
        // 알림 문구와 식별자는 별도 상한을 둔다. 너무 큰 값은 상태에 넣지 않는다.
        Ok(if value.len() <= 4096 {
            Value::String(value.into())
        } else {
            Value::Null
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut result = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > 128 {
                return Err(serde::de::Error::custom("hook metadata key limit"));
            }
            if matches!(self, Self::Hook) && matches!(key.as_str(), "tool_input" | "toolInput") {
                if result.contains_key("tool_input_hash") {
                    return Err(serde::de::Error::custom("duplicate tool input"));
                }
                let hash = map.next_value_seed(InputDigest(0))?;
                result.insert(
                    "tool_input_hash".into(),
                    hash.map_or(Value::Null, |h| Value::String(format!("{h:016x}"))),
                );
                continue;
            }
            let child = match (self, key.as_str()) {
                (Self::Hook, "prompt") => Some(Self::Preview),
                (Self::Hook, "tool_calls") | (Self::Message, "content") => Some(Self::List),
                (Self::Transcript, "message") => Some(Self::Message),
                (
                    Self::Hook,
                    "session_id"
                    | "sessionId"
                    | "hook_event_name"
                    | "hookEventName"
                    | "tool_use_id"
                    | "toolUseId"
                    | "call_id"
                    | "tool_call_id"
                    | "elicitation_id"
                    | "tool_name"
                    | "toolName"
                    | "agent_id"
                    | "subagentType"
                    | "turn_id"
                    | "turnId"
                    | "prompt_id"
                    | "promptId"
                    | "notification_type"
                    | "notificationType"
                    | "mcp_server_name"
                    | "mcpServerName"
                    | "mode"
                    | "cwd"
                    | "transcript_path"
                    | "transcriptPath"
                    | "agent_transcript_path"
                    | "message",
                )
                | (Self::Call | Self::Item, "type" | "tool_use_id" | "tool_name") => {
                    Some(Self::Item)
                }
                _ => None,
            };
            if let Some(child) = child {
                // 중복 키로 뒤의 값이 앞의 신호를 바꾸지 못하게 한다.
                if result.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate hook metadata"));
                }
                result.insert(key, map.next_value_seed(child)?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Value::Object(result))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while items.len() < 64 {
            let Some(item) = seq.next_element_seed(Self::Call)? else {
                return Ok(Value::Array(items));
            };
            items.push(item);
        }
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Value::Array(items))
    }
}

fn parse(reader: impl Read, shape: Shape) -> Option<Value> {
    let mut limited = reader.take(INPUT_BYTES_MAX);
    let mut deserializer = serde_json::Deserializer::from_reader(&mut limited);
    let value = shape.deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    if limited.limit() == 0 || !value.is_object() || value.to_string().len() > METADATA_BYTES_MAX {
        return None;
    }
    Some(value)
}

pub fn read(reader: impl Read) -> Option<Value> {
    parse(reader, Shape::Hook)
}

pub fn transcript_row(text: &str) -> Option<Value> {
    parse(text.as_bytes(), Shape::Transcript)
}

// 입력은 본문을 보관하지 않고 구조별로 해시한다. 객체의 키 순서는 결과에 영향을 주지 않는다.
// 깊거나 필드가 지나치게 많은 입력은 해시를 생략하고 실제 도구 ID 그룹으로만 보완한다.
struct InputDigest(usize);

fn digest(tag: u8, value: impl Hash) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut hash);
    value.hash(&mut hash);
    hash.finish()
}

impl<'de> DeserializeSeed<'de> for InputDigest {
    type Value = Option<u64>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        if self.0 > 32 {
            serde::Deserialize::deserialize(deserializer).map(|_: IgnoredAny| None)
        } else {
            deserializer.deserialize_any(self)
        }
    }
}

impl<'de> Visitor<'de> for InputDigest {
    type Value = Option<u64>;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("tool input")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(Some(digest(0, 0u8)))
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(Some(digest(1, v)))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Some(digest(2, v)))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(Some(digest(3, v)))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(Some(digest(4, v.to_bits())))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Some(digest(5, v)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        6u8.hash(&mut hash);
        let mut valid = true;
        let mut len = 0usize;
        while let Some(value) = seq.next_element_seed(InputDigest(self.0 + 1))? {
            valid &= value.is_some();
            value.hash(&mut hash);
            len += 1;
        }
        len.hash(&mut hash);
        Ok(valid.then(|| hash.finish()))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut pairs = Vec::new();
        let mut valid = true;
        while let Some(key) = map.next_key::<String>()? {
            if pairs.len() >= 64 || key.len() > 4096 || !valid {
                valid = false;
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let value = map.next_value_seed(InputDigest(self.0 + 1))?;
            valid &= value.is_some();
            pairs.push((key, value));
        }
        pairs.sort_unstable();
        valid &= !pairs.windows(2).any(|p| p[0].0 == p[1].0);
        Ok(valid.then(|| digest(7, pairs)))
    }
}

pub fn input_hash(value: &Value) -> Option<String> {
    InputDigest(0)
        .deserialize(value)
        .ok()
        .flatten()
        .map(|h| format!("{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 입력_해시는_객체_순서와_파싱_경로에_독립적이다() {
        let raw = r#"{"tool_input":{"z":[true,-1,2,1.5,null,"한글"],"a":{"y":1,"x":2}}}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let parsed = read(raw.as_bytes()).unwrap();
        assert_eq!(
            parsed["tool_input_hash"].as_str(),
            input_hash(&v["tool_input"]).as_deref()
        );
        let reordered = r#"{"tool_input":{"a":{"x":2,"y":1},"z":[true,-1,2,1.5,null,"한글"]}}"#;
        assert_eq!(
            parsed["tool_input_hash"],
            read(reordered.as_bytes()).unwrap()["tool_input_hash"]
        );
        assert_ne!(input_hash(&json!([1, 2])), input_hash(&json!([2, 1])));
        assert!(parsed.get("tool_input").is_none());
    }

    #[test]
    fn grok_완료_근거_경로를_찾을_cwd를_보존한다() {
        let raw = json!({"sessionId":"native","hookEventName":"notification","notificationType":"idle_prompt","cwd":"/tmp/project"}).to_string();
        let v = read(raw.as_bytes()).unwrap();
        assert_eq!(v["cwd"], "/tmp/project");
        assert_eq!(
            crate::agent_attention::normalize(&v, 1).unwrap().kind,
            storage::AttentionEventKind::IdleObserved
        );
    }

    #[test]
    fn 큰_배치_결과와_입력은_버리고_모든_호출_id는_남긴다() {
        let raw = json!({"session_id":"native","hook_event_name":"PostToolBatch","tool_calls":[
            {"tool_use_id":"a","tool_name":"Read","tool_input":{"body":"가".repeat(100_000)},"tool_response":"x".repeat(300_000)},
            {"tool_use_id":"b","tool_name":"Bash","tool_response":{"nested":["x".repeat(300_000)]}}
        ]}).to_string();
        let v = read(raw.as_bytes()).unwrap();
        assert_eq!(v["tool_calls"][0]["tool_use_id"], "a");
        assert_eq!(v["tool_calls"][1]["tool_use_id"], "b");
        assert!(v.to_string().len() < 1024);
    }

    #[test]
    fn 잘못된_json과_중복_식별자는_거부한다() {
        assert!(read(b"{\"session_id\":\"s\"".as_slice()).is_none());
        assert!(read(b"{\"session_id\":\"s\",\"session_id\":\"t\"}".as_slice()).is_none());
        assert!(read(b"{\"session_id\":\"s\"} trailing".as_slice()).is_none());
        let raw = json!({"tool_response":"x".repeat(INPUT_BYTES_MAX as usize),"session_id":"s"})
            .to_string();
        assert!(read(raw.as_bytes()).is_none());
    }

    #[test]
    fn 사용자_입력_hook은_긴_본문에서_짧은_작업_미리보기만_남긴다() {
        let raw = json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "shared-native",
            "prompt": format!("  서로 다른 작업\n{}", "가".repeat(5000)),
        })
        .to_string();
        let parsed = read(raw.as_bytes()).unwrap();
        let preview = parsed["prompt"].as_str().unwrap();
        assert!(preview.starts_with("서로 다른 작업 "));
        assert!(preview.len() <= 256);
        assert!(!preview.contains('\n'));
    }
}
