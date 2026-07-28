//! 프롬프트 라이브러리 — 저장·재사용 가능한 에이전트 지시(파라미터 포함).
//!
//! Warp Drive의 Prompts에 대응하는 호스트 레벨 기능(기능2). 자주 쓰는 지시("이 PR
//! 리뷰해", "X에 테스트 추가")를 `{{param}}` 파라미터와 함께 저장해 두고, 팔레트에서
//! 골라 파라미터만 채워 활성 세션에 주입한다.
//!
//! 이 모듈은 **순수 데이터 + 로직**이다(egui·PTY를 모른다). 후속 PR에서 leaf 팔레트가
//! 이 데이터로 UI를 그리고, `app.rs`가 치환 결과를 composer/WriteInput 경로로 주입한다
//! — 기존 leaf+intent+host I/O 경계와 동일하다.

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
    /// 손상 시 앱을 막지 않는다). 없음/손상/정상-빈 파일을 구분하지 않으므로 호출자가
    /// 오류를 로깅할 방법은 없다 — 파괴적 손상을 막는 책임은 save의 원자성에 있다.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 파일에 원자적으로 저장한다. 부모 디렉터리는 있다고 가정한다(AppPaths가 생성).
    /// 임시 파일에 쓴 뒤 rename으로 교체한다 — 저장이 매 편집/삭제마다 일어나므로,
    /// 쓰기 도중 크래시로 파일이 잘려 load()가 빈 라이브러리로 되돌아가는(=전량 손실)
    /// 걸 막는다(PR-3 리뷰 Low). 같은 디렉터리 rename은 Unix에서 원자적이다.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
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

    /// 새 프롬프트에 부여할 라이브러리 내 유일한 id를 title 슬러그로 만든다. 슬러그가
    /// 비면(예: 한글 전용 제목) `prompt`를 쓰고, 충돌 시 `-2`, `-3`…을 붙인다. id는
    /// 내부 식별자일 뿐 화면에는 title이 나온다.
    pub fn fresh_id(&self, title: &str) -> String {
        let base = slugify(title);
        let base = if base.is_empty() {
            "prompt".to_owned()
        } else {
            base
        };
        if self.get(&base).is_none() {
            return base;
        }
        let mut n = 2;
        loop {
            let candidate = format!("{base}-{n}");
            if self.get(&candidate).is_none() {
                return candidate;
            }
            n += 1;
        }
    }

    /// id가 같은 프롬프트가 있으면 교체(편집), 없으면 추가(신규)한다.
    pub fn upsert(&mut self, prompt: Prompt) {
        match self.prompts.iter_mut().find(|p| p.id == prompt.id) {
            Some(existing) => *existing = prompt,
            None => self.prompts.push(prompt),
        }
    }

    /// id로 프롬프트를 삭제한다(없으면 무시).
    pub fn delete(&mut self, id: &str) {
        self.prompts.retain(|p| p.id != id);
    }

    /// 첫 실행(저장 파일 없음)에서 팔레트가 비지 않도록 채우는 예시 프롬프트들.
    pub fn default_seed() -> Self {
        let p = |id: &str, title: &str, body: &str, tags: &[&str]| Prompt {
            id: id.into(),
            title: title.into(),
            body: body.into(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
        };
        Self {
            prompts: vec![
                p(
                    "review-branch",
                    "브랜치 리뷰",
                    "이 브랜치의 변경을 리뷰해줘. 특히 {{focus}}를 중점으로 보고 버그·엣지케이스·테스트 누락을 짚어줘.",
                    &["review", "git"],
                ),
                p(
                    "add-tests",
                    "테스트 추가",
                    "{{module}}에 대한 단위 테스트를 추가해줘. 경계값과 실패 경로를 포함하고 기존 테스트 스타일을 따라줘.",
                    &["test"],
                ),
                p(
                    "explain-error",
                    "에러 설명",
                    "방금 출력된 에러의 원인을 설명하고 최소 수정안을 제안해줘.",
                    &["debug"],
                ),
                p(
                    "commit-msg",
                    "커밋 메시지",
                    "지금 staged 변경에 대한 한국어 커밋 메시지를 conventional commits 형식으로 써줘.",
                    &["git"],
                ),
            ],
        }
    }
}

/// `{{...}}` 스캔이 내놓는 조각 하나. param_names와 render가 이 이터레이터 하나를
/// 공유해서 find("{{") → find("}}") → trim → is_param_name 브레이스 파싱 로직이 두
/// 함수에서 따로 갈라지는 걸 막는다(PR-1 리뷰 M2: 한쪽만 고치면 파라미터 판정이
/// 조용히 어긋날 수 있었음).
enum Segment<'a> {
    /// 일반 텍스트, 또는 파라미터가 아닌 것으로 판정된 `{{...}}` 원문(중괄호 포함
    /// 그대로) — render는 이걸 그대로 출력에 이어붙이면 된다.
    Literal(&'a str),
    /// 유효한 파라미터로 판정된 `{{ name }}` 블록의 트림된 이름.
    Param(&'a str),
}

/// `body`를 앞에서부터 스캔해 Literal/Param 조각을 등장 순서대로 내놓는다. `{{`를 찾고
/// 짝이 되는 `}}`를 찾아 안쪽을 트림한 뒤 `is_param_name`으로 파라미터 여부를 판정한다
/// — 판정 로직이 이 한 곳에만 있어서 param_names와 render가 절대 어긋나지 않는다.
fn scan_braces(body: &str) -> impl Iterator<Item = Segment<'_>> {
    struct Scanner<'a> {
        rest: &'a str,
    }

    impl<'a> Iterator for Scanner<'a> {
        type Item = Segment<'a>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.rest.is_empty() {
                return None;
            }
            let Some(open) = self.rest.find("{{") else {
                // 더 이상 "{{" 없음 — 남은 전부 리터럴
                let lit = self.rest;
                self.rest = "";
                return Some(Segment::Literal(lit));
            };
            if open > 0 {
                // "{{" 앞의 일반 텍스트를 먼저 리터럴로 내놓는다
                let lit = &self.rest[..open];
                self.rest = &self.rest[open..];
                return Some(Segment::Literal(lit));
            }
            let after = &self.rest[2..];
            let Some(close) = after.find("}}") else {
                // 닫힘 없음 — 남은 전부 리터럴
                let lit = self.rest;
                self.rest = "";
                return Some(Segment::Literal(lit));
            };
            let raw = &self.rest[..2 + close + 2];
            let name = after[..close].trim();
            self.rest = &after[close + 2..];
            if is_param_name(name) {
                Some(Segment::Param(name))
            } else {
                // 파라미터 아님 — `{{...}}` 원문을 리터럴로 보존
                Some(Segment::Literal(raw))
            }
        }
    }

    Scanner { rest: body }
}

/// `body`의 `{{name}}` 파라미터 이름을 등장 순서로, 중복 없이 뽑는다. name은
/// `[A-Za-z0-9_]+`(양옆 공백 허용: `{{ name }}`)만 파라미터로 본다 — 그 밖의 `{{...}}`는
/// 프롬프트 본문의 리터럴로 두고 무시한다(예: JSON 예시 안의 중괄호).
pub fn param_names(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in scan_braces(body) {
        if let Segment::Param(name) = seg
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// `body`의 `{{name}}`을 `values`로 치환한다. 값이 없는 파라미터는 빈 문자열로 치환한다
/// (UI가 제출 전 전부 채우게 강제하므로 정상 경로에선 발생하지 않는다). 파라미터가
/// 아닌 `{{...}}`(리터럴 중괄호)는 원문 그대로 남긴다.
pub fn render(body: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    for seg in scan_braces(body) {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Param(name) => {
                out.push_str(values.get(name).map(String::as_str).unwrap_or(""));
            }
        }
    }
    out
}

fn is_param_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// title → id 슬러그. ASCII 영숫자는 소문자로 남기고 나머지는 `-`로 접은 뒤 앞뒤·중복
/// `-`를 정리한다. 비-ASCII만 있는 제목은 빈 슬러그가 되며 호출부가 fallback을 쓴다.
fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

/// 편집 폼의 태그 입력(공백·쉼표 구분)을 정규화한다. 트림·빈값 제거·중복 제거하고
/// 입력 순서를 보존한다.
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in input.split([',', ' ', '\t', '\n']) {
        let tag = tag.trim();
        // 대소문자 무시 중복제거(검색이 대소문자 무시라 "Git"/"git"이 같은 태그로
        // 매칭됨 — 저장도 같게 취급해 근접 중복을 막는다). 첫 등장 표기를 보존한다.
        if !tag.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            out.push(tag.to_owned());
        }
    }
    out
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
            render(
                "{{x}} then {{x}}; keep {{ not param }}",
                &vals(&[("x", "A")])
            ),
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
    fn fresh_id_슬러그화와_충돌회피() {
        let mut lib = PromptLibrary::default();
        assert_eq!(lib.fresh_id("Review PR"), "review-pr");
        // 한글 전용 제목 → 빈 슬러그 → fallback "prompt"
        assert_eq!(lib.fresh_id("브랜치 리뷰"), "prompt");
        lib.prompts.push(Prompt {
            id: "review-pr".into(),
            title: "x".into(),
            body: "b".into(),
            tags: vec![],
        });
        assert_eq!(lib.fresh_id("Review  PR!!"), "review-pr-2");
    }

    #[test]
    fn upsert_는_교체_또는_추가하고_delete는_제거() {
        let mut lib = PromptLibrary::default();
        let mk = |id: &str, title: &str| Prompt {
            id: id.into(),
            title: title.into(),
            body: "b".into(),
            tags: vec![],
        };
        lib.upsert(mk("a", "first"));
        lib.upsert(mk("b", "second"));
        assert_eq!(lib.prompts.len(), 2);
        // 같은 id → 교체(추가 아님)
        lib.upsert(mk("a", "first-edited"));
        assert_eq!(lib.prompts.len(), 2);
        assert_eq!(lib.get("a").unwrap().title, "first-edited");
        lib.delete("a");
        assert_eq!(lib.prompts.len(), 1);
        assert!(lib.get("a").is_none());
        lib.delete("nope"); // 없는 id는 무시
        assert_eq!(lib.prompts.len(), 1);
    }

    #[test]
    fn parse_tags_트림_빈값제거_중복제거_순서보존() {
        assert_eq!(
            parse_tags(" git, review  review,,test "),
            vec!["git", "review", "test"]
        );
        assert_eq!(parse_tags("   "), Vec::<String>::new());
    }

    #[test]
    fn parse_tags_대소문자_무시_중복제거_첫표기_보존() {
        // "Git"과 "git"은 같은 태그 — 첫 등장 표기("Git")를 남긴다(검색이 대소문자 무시).
        assert_eq!(parse_tags("Git git GIT review"), vec!["Git", "review"]);
    }

    #[test]
    fn render_닫히지_않은_중괄호는_원문_유지() {
        // param_names뿐 아니라 사용자 출력을 만드는 render도 unclosed를 안전 처리한다.
        assert_eq!(render("prefix {{oops", &vals(&[])), "prefix {{oops");
    }

    #[test]
    fn 중첩_빈_중괄호는_파라미터_아님_render_불변() {
        // {{{{x}}}}의 첫 쌍이 잡는 이름은 "{{x" → is_param_name 실패 → 통째로 리터럴.
        assert_eq!(param_names("{{{{x}}}}"), Vec::<String>::new());
        assert_eq!(render("{{{{x}}}}", &vals(&[("x", "V")])), "{{{{x}}}}");
        // 빈 이름({{}})도 파라미터 아님.
        assert_eq!(param_names("{{}}"), Vec::<String>::new());
        assert_eq!(render("{{}}", &vals(&[])), "{{}}");
    }

    #[test]
    fn load_손상_json은_빈_라이브러리로_폴백() {
        // "missing/corrupt → default" 계약의 corrupt 절반을 테스트로 고정한다.
        let dir = std::env::temp_dir().join("deppy_prompt_lib_corrupt_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("corrupt.json");
        std::fs::write(&path, "{ not valid json ").unwrap();
        assert_eq!(PromptLibrary::load(&path), PromptLibrary::default());
        let _ = std::fs::remove_file(&path);
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
