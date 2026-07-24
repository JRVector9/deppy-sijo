//! 프롬프트 라이브러리 — 저장·재사용 가능한 에이전트 지시(파라미터 포함).
//!
//! Warp Drive의 Prompts에 대응하는 호스트 레벨 기능(기능2). 자주 쓰는 지시("이 PR
//! 리뷰해", "X에 테스트 추가")를 `{{param}}` 파라미터와 함께 저장해 두고, 팔레트에서
//! 골라 파라미터만 채워 활성 세션에 주입한다.
//!
//! 이 모듈은 **순수 데이터 + 로직**이다(egui·PTY를 모른다). 후속 PR에서 leaf 팔레트가
//! 이 데이터로 UI를 그리고, `app.rs`가 치환 결과를 composer/WriteInput 경로로 주입한다
//! — 기존 leaf+intent+host I/O 경계와 동일하다.

// PR-1은 데이터 모델·로직만이다. 팔레트/composer 배선(실사용)은 PR-2에서 붙는다 —
// 그전까지 미사용 항목이 있어 경고를 억제한다(compressed.rs 선례와 동일 정책).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

/// 저장된 프롬프트 하나. 파라미터는 별도 필드가 아니라 `body`의 `{{name}}`에서
/// 파생한다(단일 진실원). `tags`는 검색·분류용.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prompt {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Prompt {
    /// 이 프롬프트가 요구하는 파라미터 이름(등장 순서·중복 제거).
    pub fn params(&self) -> Vec<String> {
        param_names(&self.body)
    }
}

/// 사용자 프롬프트 모음. `config_dir/prompt_library.json`에 직렬화된다.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptLibrary {
    #[serde(default)]
    pub prompts: Vec<Prompt>,
}

impl PromptLibrary {
    /// 파일에서 로드한다. 파일이 없거나 파싱 실패면 빈 라이브러리(사용자 데이터이므로
    /// 손상 시 앱을 막지 않는다 — 로드 실패는 호출자가 로깅한다).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 파일에 저장한다. 부모 디렉터리는 있다고 가정한다(AppPaths가 생성).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// 제목·본문·태그에 대해 대소문자 무시 부분일치로 검색한다. 빈 쿼리는 전체를
    /// 저장 순서대로 돌려준다.
    pub fn search(&self, query: &str) -> Vec<&Prompt> {
        let q = query.trim().to_lowercase();
        self.prompts
            .iter()
            .filter(|p| {
                q.is_empty()
                    || p.title.to_lowercase().contains(&q)
                    || p.body.to_lowercase().contains(&q)
                    || p.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Prompt> {
        self.prompts.iter().find(|p| p.id == id)
    }
}

/// `body`의 `{{name}}` 파라미터 이름을 등장 순서로, 중복 없이 뽑는다. name은
/// `[A-Za-z0-9_]+`(양옆 공백 허용: `{{ name }}`)만 파라미터로 본다 — 그 밖의 `{{...}}`는
/// 프롬프트 본문의 리터럴로 두고 무시한다(예: JSON 예시 안의 중괄호).
pub fn param_names(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break; // 닫힘 없음 — 끝
        };
        let name = after[..close].trim();
        if is_param_name(name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        rest = &after[close + 2..];
    }
    out
}

/// `body`의 `{{name}}`을 `values`로 치환한다. 값이 없는 파라미터는 빈 문자열로 치환한다
/// (UI가 제출 전 전부 채우게 강제하므로 정상 경로에선 발생하지 않는다). 파라미터가
/// 아닌 `{{...}}`(리터럴 중괄호)는 원문 그대로 남긴다.
pub fn render(body: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            // 닫힘 없음 — 남은 전부 리터럴
            out.push_str(&rest[open..]);
            return out;
        };
        let name = after[..close].trim();
        if is_param_name(name) {
            out.push_str(values.get(name).map(String::as_str).unwrap_or(""));
        } else {
            // 파라미터 아님 — `{{...}}` 원문 유지
            out.push_str(&rest[open..open + 2 + close + 2]);
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

fn is_param_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn param_names_등장순서_중복제거() {
        assert_eq!(
            param_names("Review PR {{pr}} in {{repo}}, then {{ pr }} again"),
            vec!["pr", "repo"]
        );
        assert_eq!(param_names("no params here"), Vec::<String>::new());
    }

    #[test]
    fn param_names_비파라미터_중괄호_무시() {
        // JSON 예시 안의 중괄호나 공백 포함은 파라미터로 보지 않는다.
        assert_eq!(param_names(r#"emit {{ "a": 1 }} and {{x}}"#), vec!["x"]);
        assert_eq!(param_names("unclosed {{oops"), Vec::<String>::new());
    }

    #[test]
    fn render_치환과_누락_빈문자열() {
        assert_eq!(
            render("add tests for {{module}}", &vals(&[("module", "pty")])),
            "add tests for pty"
        );
        // 값 없는 파라미터 → 빈 문자열
        assert_eq!(render("hi {{name}}", &vals(&[])), "hi ");
    }

    #[test]
    fn render_반복_파라미터와_리터럴_보존() {
        assert_eq!(
            render("{{x}} then {{x}}; keep {{ not param }}", &vals(&[("x", "A")])),
            "A then A; keep {{ not param }}"
        );
    }

    #[test]
    fn search_대소문자무시_제목본문태그() {
        let lib = PromptLibrary {
            prompts: vec![
                Prompt {
                    id: "1".into(),
                    title: "Review PR".into(),
                    body: "review {{pr}}".into(),
                    tags: vec!["git".into()],
                },
                Prompt {
                    id: "2".into(),
                    title: "Add tests".into(),
                    body: "add tests for {{module}}".into(),
                    tags: vec![],
                },
            ],
        };
        assert_eq!(lib.search("review").len(), 1);
        assert_eq!(lib.search("TEST").len(), 1); // 제목 "Add tests"
        assert_eq!(lib.search("git").len(), 1); // 태그
        assert_eq!(lib.search("").len(), 2); // 빈 쿼리 = 전체
    }

    #[test]
    fn load_save_왕복_그리고_없는_파일은_빈것() {
        let dir = std::env::temp_dir().join("deppy_prompt_lib_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("lib.json");
        let _ = std::fs::remove_file(&path);
        // 없는 파일 → 빈 라이브러리
        assert_eq!(PromptLibrary::load(&path), PromptLibrary::default());

        let lib = PromptLibrary {
            prompts: vec![Prompt {
                id: "1".into(),
                title: "T".into(),
                body: "b {{p}}".into(),
                tags: vec!["x".into()],
            }],
        };
        lib.save(&path).unwrap();
        assert_eq!(PromptLibrary::load(&path), lib);
        let _ = std::fs::remove_file(&path);
    }
}
