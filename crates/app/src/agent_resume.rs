//! 세션 「이어가기」에 필요한 CLI 인자 판정 — 순수 함수, I/O 없음.
//!
//! 앱을 껐다 켜면 에이전트 pane은 열람 전용으로 복원된다(프로세스는 안 살림). 사용자가
//! 「다시 실행」을 누르면 DB `sessions` 행의 `args_json`(저장된 실행 인자) 뒤에 이 모듈이
//! 돌려주는 인자를 덧붙여 재실행한다 — agent_shim.rs의 shim이 그 뒤를 `"$@"`로 이어받는다.
//!
//! CLI마다 이어가기 플래그의 문법·위치 제약이 달라 `--help` 실측 없이 추측으로 넣으면
//! 시작 자체가 거부될 수 있다(오늘 겪은 codex `--dangerously-bypass-hook-trust` 중복 사건
//! 참고 — 추측 인자 하나가 에이전트를 통째로 못 띄우게 만들었다). 그래서 표에 없는
//! agent_id, 그리고 실측하지 못한 CLI는 무조건 빈 벡터를 돌려준다 — 이어가기 실패보다
//! 새 대화로 시작하는 쪽이 낫다.

/// 이 에이전트를 이전 대화를 이어받아 실행하려면 어떤 인자를 저장된 `args_json` 뒤에
/// 덧붙여야 하나. 빈 벡터면 「이어가기 미지원」 — 새 대화로 시작한다.
// 「다시 실행」 UI(PR-3)가 아직 이 모듈을 안 불러 clippy가 dead_code로 막는다 — UI가
// 붙으면 이 allow는 지운다.
#[allow(dead_code)]
pub fn resume_args(agent_id: &str) -> Vec<String> {
    match agent_id {
        // claude 2.1.227, `claude --help` 실측: `-c, --continue  Continue the most
        // recent conversation in the current directory`. 인자를 받지 않는 불리언
        // 플래그라 저장된 `--model`/`--effort` 뒤에 붙어도 파싱에 영향이 없다 — 실측:
        // `claude --model opus --effort high -c --help` → exit 0.
        "deppy-builtin-claude" => vec!["-c".to_owned()],

        // codex-cli 0.147.0. `codex --help`에는 `--effort`가 아예 없다 — deppy가
        // effort를 넘길 때도 `--config model_reasoning_effort="..."`를 쓴다
        // (agent_launcher.rs build_launch_spec). resume은 서브커맨드라
        // `codex [OPTIONS] <COMMAND>` 규칙상 전역 옵션(--model, --config) *뒤에*
        // 와야 하는데, 저장된 인자가 바로 그 전역 옵션들이다. 실측으로 위치 규칙을
        // 확인했다(실제 세션은 만들지 않고 --help로 파싱만 확인):
        // `codex --model gpt-5.1-codex-max --config 'model_reasoning_effort="high"'
        //  resume --last --help` → exit 0, `codex resume --help`와 동일한 출력.
        // `--last`는 세션 id 없이 "가장 최근 세션"을 고른다(`codex resume --help`).
        "deppy-builtin-codex" => vec!["resume".to_owned(), "--last".to_owned()],

        // Kimi CLI 0.34.0, `~/.kimi-code/bin/kimi --help` 실측: `-c, --continue
        // Continue the previous session for the working directory.` claude와 같은
        // 형태의 불리언 플래그. 실측: `kimi --model kimi-code/k3 -c --help` → exit 0.
        "deppy-builtin-kimi" => vec!["-c".to_owned()],

        // grok: 이 작업 환경에는 실제 xAI grok CLI가 설치돼 있지 않아 `--help`를 실행할
        // 수 없었다(PATH의 `grok`은 다른 도구의 wrapper로, 실행하면 "grok not found in
        // PATH"를 반환한다). 실측하지 못한 채 넣지 않는다.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 표에_있는_에이전트는_실측된_이어가기_인자를_돌려준다() {
        assert_eq!(resume_args("deppy-builtin-claude"), vec!["-c".to_owned()]);
        assert_eq!(
            resume_args("deppy-builtin-codex"),
            vec!["resume".to_owned(), "--last".to_owned()]
        );
        assert_eq!(resume_args("deppy-builtin-kimi"), vec!["-c".to_owned()]);
    }

    #[test]
    fn 실측하지_못한_grok과_미지원_agent_id는_빈_벡터다() {
        // grok: --help를 실행할 CLI 자체가 이 환경에 없어 실측하지 못했다.
        assert!(resume_args("deppy-builtin-grok").is_empty());
        // 표에 아예 없는 agent_id(다른 빌트인, 오타, 빈 문자열)도 전부 빈 벡터.
        assert!(resume_args("deppy-builtin-opencode").is_empty());
        assert!(resume_args("deppy-builtin-gemini").is_empty());
        assert!(resume_args("unknown-agent").is_empty());
        assert!(resume_args("").is_empty());
    }
}
