//! PTY 에이전트의 추론 강도를 한 단계 올리고 내리는 provider별 전략.
//!
//! structured(app-server) 세션은 `AgentSessionsUi::adjust_selected_effort`가 로컬
//! 상태만 바꿔 다음 `turn/start`에 실어 보내면 끝이다. PTY는 그런 채널이 없어서
//! **CLI가 이해하는 입력을 그대로 써 넣는** 수밖에 없고, 그 방법이 provider마다
//! 완전히 다르다. 아래는 2026-08-02에 실제 CLI를 PTY에 띄워 확인한 결과다.
//!
//! - **Claude** (2.1.220): `/effort <level>` 슬래시 명령이 인자를 받는다.
//!   실측에서 `xhigh → low`로 즉시 바뀌었다. 단 CLI가 이 값을 사용자 전역
//!   기본값으로도 저장한다 — 출력이 `Set effort level to low (saved as your
//!   default for new sessions)`이다. 되돌리지 않고 호출부가 사용자에게 알린다.
//! - **Codex** (0.146.0): `/model`은 대화형 피커라 한 줄로 못 끝낸다. 대신 TUI에
//!   네이티브 액션이 있다 — `/keymap` 뷰어 실측으로 `Chat / Increase Reasoning
//!   Effort = alt-. , shift-up`, `Decrease = alt-, , shift-down`. 키를 보내면
//!   CLI가 알아서 처리하므로 현재 강도를 몰라도 되고, 진행 중 여부도 CLI가 판단한다.
//! - **그 외**: 근거가 없다. 추측 구현 대신 `Unsupported`를 돌려 호출부가 그렇게
//!   말하게 한다.

use crate::agent_surface::AgentProvider;

/// 한 단계 올릴지 내릴지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortStep {
    Up,
    Down,
}

/// 무엇을 조정하는가. 낙관적 값을 강도/모델별로 따로 들고 있기 위한 키다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdjustKind {
    Effort,
    Model,
}

/// 권위 있는 값(statusLine→DB)이 우리가 방금 보낸 값을 따라잡았는지.
///
/// 강도는 표기가 같아서 그대로 비교하면 되고, 모델은 권위 쪽이 표시명("Opus 5 (1M
/// context)")이라 슬러그(`opus`) 포함 여부로 본다.
pub fn authoritative_caught_up(kind: AdjustKind, authoritative: &str, pending: &str) -> bool {
    let authoritative = authoritative.trim().to_ascii_lowercase();
    match kind {
        AdjustKind::Effort => authoritative == pending.to_ascii_lowercase(),
        AdjustKind::Model => authoritative.contains(&pending.to_ascii_lowercase()),
    }
}

/// 이 provider에서 강도를 바꾸는 방법.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortPlan {
    /// PTY에 그대로 써 넣을 바이트. CLI의 네이티브 키 입력이다.
    Keys(Vec<u8>),
    /// 슬래시 명령 한 줄. `level`은 사다리에서 계산된 다음 단계다.
    Slash { line: String, level: &'static str },
}

/// 강도를 못 옮기는 이유. 호출부가 로그로 남긴다.
///
/// "지원하지 않는 provider" 변형은 두지 않는다 — `AgentProvider`가 Claude/Codex
/// 둘뿐이고 둘 다 방법이 확인됐다. 세 번째가 생기면 `supports`가 게이트에서
/// 먼저 걸러 여기까지 오지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortBlocked {
    /// 현재 값을 몰라 다음 단계를 계산할 수 없다 (Claude 경로 전용).
    ///
    /// "사다리 끝" 변형은 없다 — 끝에서 순환하므로 막힐 일이 없다.
    UnknownCurrentEffort,
}

/// Claude `/effort`가 받는 단계. `agent_launcher::CLAUDE_EFFORTS`와 같은 순서다.
///
/// `ultracode`는 일부러 뺐다 — 실측 출력이 "xhigh + dynamic workflow
/// orchestration"이라 단순한 한 단계 위가 아니고, 한 번 걸리면 해제에도 별도
/// 명령이 필요하다. 단축키로 무심코 진입할 자리가 아니다.
const CLAUDE_LADDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Codex TUI 네이티브 키를 **CSI 화살표**로 보낸다 — `shift-up` / `shift-down`.
///
/// 같은 액션에 `alt-.` / `alt-,`도 묶여 있지만(문서상 기본값) 그쪽은 쓸 수 없다.
/// Codex는 기동 시 Kitty 키보드 프로토콜을 켜고(`CSI > 7 u`), 그 모드에서 Alt는
/// **수정자 비트**지 ESC 접두사가 아니다. `\x1b,`를 보내면 Codex가 ESC로 읽는다 —
/// `/keymap`의 키 캡처 화면에 그대로 넣어 확인했다(2026-08-02): `\x1b,`는 캡처를
/// **취소**시켰고, Kitty 형식 `\x1b[97;3u`는 `alt-a`로 정확히 잡혔다. ESC는
/// Codex에서 진행 중 턴을 중단시키므로 조용히 안 먹는 정도가 아니라 해롭다.
///
/// CSI 화살표는 레거시와 Kitty 양쪽에서 표기가 같아 모드를 몰라도 된다. 같은 캡처
/// 화면에서 `\x1b[1;2B`가 `shift-down`으로 잡히는 것을 확인했다.
const CODEX_INCREASE: &[u8] = b"\x1b[1;2A";
const CODEX_DECREASE: &[u8] = b"\x1b[1;2B";

/// 현재 값에서 한 칸 옮긴다. **사다리 끝에서 순환한다.**
///
/// 끝에서 막으면 정상 동작인데도 화면에 아무 일이 없어 고장으로 읽힌다 — 실제로
/// 최대 강도에 있던 세션에서 올리기를 눌러 "안 된다"고 보고됐다(2026-08-02).
/// 순환하면 어느 방향으로 눌러도 항상 반응이 있고, 되돌리기도 한 번이면 된다.
/// 강도 순환은 max에서 low로 크게 떨어지므로, 호출부는 보낸 값을 CLI 출력으로
/// 확인할 수 있어야 한다(Claude가 `Set effort level to ...`를 pane에 찍는다).
///
/// 비교는 대소문자를 무시한다 — statusLine이 주는 표기(`XHigh`)와 CLI가 받는
/// 표기(`xhigh`)가 다를 수 있다.
fn step_ladder(
    ladder: &[&'static str],
    current: &str,
    step: EffortStep,
) -> Result<&'static str, EffortBlocked> {
    let current = current.trim();
    let index = ladder
        .iter()
        .position(|level| level.eq_ignore_ascii_case(current))
        .ok_or(EffortBlocked::UnknownCurrentEffort)?;
    let len = ladder.len();
    let next = match step {
        EffortStep::Up => (index + 1) % len,
        EffortStep::Down => (index + len - 1) % len,
    };
    Ok(ladder[next])
}

/// PTY 에이전트 한 건의 강도를 옮기기 위해 보낼 입력을 정한다.
///
/// `current_effort`는 Claude 경로에서만 쓴다. Codex는 CLI가 자기 상태를 알고
/// 있으므로 `None`이어도 된다 — 이게 Codex 경로가 더 튼튼한 이유다.
pub fn plan(
    provider: AgentProvider,
    step: EffortStep,
    current_effort: Option<&str>,
) -> Result<EffortPlan, EffortBlocked> {
    match provider {
        AgentProvider::Codex => Ok(EffortPlan::Keys(
            match step {
                EffortStep::Up => CODEX_INCREASE,
                EffortStep::Down => CODEX_DECREASE,
            }
            .to_vec(),
        )),
        AgentProvider::Claude => {
            let current = current_effort.ok_or(EffortBlocked::UnknownCurrentEffort)?;
            let level = step_ladder(CLAUDE_LADDER, current, step)?;
            Ok(EffortPlan::Slash {
                line: format!("/effort {level}\r"),
                level,
            })
        }
    }
}

/// 이 provider에서 강도 변경이 가능한가. 게이트가 UI를 열기 전에 묻는다.
pub const fn supports(provider: AgentProvider) -> bool {
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => true,
    }
}

/// Claude `/model`이 받는 슬러그. 런처 내장 카탈로그와 같은 순서다
/// (`두_카탈로그가_어긋나지_않는다` 테스트가 고정한다).
const CLAUDE_MODEL_LADDER: &[&str] = &["sonnet", "opus", "fable"];

/// 이 provider에서 모델 전환이 가능한가.
///
/// **Codex는 불가능하다** (2026-08-02 실측). 강도와 달리 키맵 액션이 없고
/// (`/keymap` 검색 결과 `model` → `no matches`), `/model <슬러그>`는 슬래시 명령으로
/// 처리되지 않고 **프롬프트로 흘러 실제 턴을 태운다** — 응답이 "Model switching is
/// controlled by the client. Please select ... in the model picker"였고 모델은 그대로였다.
/// 즉 안 되는 정도가 아니라 토큰을 쓴다. 남은 길은 대화형 피커를 화살표로 모는 것뿐인데,
/// 목록 순서와 현재 위치에 의존해 조용히 엉뚱한 모델을 고를 수 있어 쓰지 않는다.
pub const fn supports_model(provider: AgentProvider) -> bool {
    match provider {
        AgentProvider::Claude => true,
        AgentProvider::Codex => false,
    }
}

/// 모델을 카탈로그 순서로 한 칸 옮긴다.
///
/// `current_model`은 statusLine이 주는 **표시명**이라 슬러그와 다르다
/// ("Opus 5 (1M context)" ↔ `opus`). 그래서 슬러그 포함 여부로 맞춘다 —
/// sonnet/opus/fable은 서로의 부분문자열이 아니라 모호하지 않다.
pub fn plan_model(
    provider: AgentProvider,
    step: EffortStep,
    current_model: Option<&str>,
) -> Result<EffortPlan, EffortBlocked> {
    if !supports_model(provider) {
        return Err(EffortBlocked::UnknownCurrentEffort);
    }
    let current = current_model.ok_or(EffortBlocked::UnknownCurrentEffort)?;
    // 모델은 statusLine이 표시명("Opus 5 (1M context)")을 주므로 슬러그 포함으로 맞춘다.
    let lowered = current.to_ascii_lowercase();
    let index = CLAUDE_MODEL_LADDER
        .iter()
        .position(|slug| lowered.contains(slug))
        .ok_or(EffortBlocked::UnknownCurrentEffort)?;
    let len = CLAUDE_MODEL_LADDER.len();
    let next = match step {
        EffortStep::Up => (index + 1) % len,
        EffortStep::Down => (index + len - 1) % len,
    };
    let level = CLAUDE_MODEL_LADDER[next];
    Ok(EffortPlan::Slash {
        line: format!("/model {level}\r"),
        level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex는_현재_강도를_몰라도_키를_보낸다() {
        // CLI가 자기 상태를 알고 있어서 deppy가 추적할 필요가 없다.
        assert_eq!(
            plan(AgentProvider::Codex, EffortStep::Up, None),
            Ok(EffortPlan::Keys(b"\x1b[1;2A".to_vec()))
        );
        assert_eq!(
            plan(AgentProvider::Codex, EffortStep::Down, None),
            Ok(EffortPlan::Keys(b"\x1b[1;2B".to_vec()))
        );
    }

    /// Codex 키는 **CSI 화살표**여야 한다. `\x1b.`/`\x1b,` 같은 레거시 Alt 표기를
    /// 쓰면 Kitty 모드의 Codex가 선두 ESC를 그대로 ESC로 읽어 진행 중 턴을
    /// 중단시킨다 — 안 먹는 게 아니라 해롭다. 회귀하면 여기서 잡는다.
    #[test]
    fn codex_키는_esc_접두사를_쓰지_않는다() {
        for step in [EffortStep::Up, EffortStep::Down] {
            let Ok(EffortPlan::Keys(bytes)) = plan(AgentProvider::Codex, step, None) else {
                panic!("Codex는 키 계획이어야 한다");
            };
            assert!(
                bytes.starts_with(b"\x1b["),
                "{bytes:?}가 CSI로 시작하지 않는다 — 맨몸 ESC는 턴을 중단시킨다"
            );
            // ESC 다음이 곧바로 문자면 Alt 레거시 표기다.
            assert_ne!(bytes.get(1), Some(&b'.'));
            assert_ne!(bytes.get(1), Some(&b','));
        }
    }

    #[test]
    fn claude는_사다리에서_다음_단계를_계산한다() {
        assert_eq!(
            plan(AgentProvider::Claude, EffortStep::Up, Some("high")),
            Ok(EffortPlan::Slash {
                line: "/effort xhigh\r".to_owned(),
                level: "xhigh",
            })
        );
        assert_eq!(
            plan(AgentProvider::Claude, EffortStep::Down, Some("high")),
            Ok(EffortPlan::Slash {
                line: "/effort medium\r".to_owned(),
                level: "medium",
            })
        );
    }

    /// statusLine 표기(`XHigh`)와 CLI 표기(`xhigh`)가 달라도 같은 단계여야 한다.
    /// 여기서 어긋나면 사용자는 "가끔 안 먹는" 단축키를 보게 된다.
    #[test]
    fn 대소문자와_공백이_달라도_같은_단계다() {
        for current in ["XHigh", "  xhigh  ", "XHIGH"] {
            assert_eq!(
                plan(AgentProvider::Claude, EffortStep::Down, Some(current)),
                Ok(EffortPlan::Slash {
                    line: "/effort high\r".to_owned(),
                    level: "high",
                }),
                "{current}가 xhigh로 인식되지 않았다"
            );
        }
    }

    /// 끝에서 막으면 정상 동작인데도 "안 된다"로 읽힌다 — 실제 보고된 증상이다.
    /// 어느 방향으로 눌러도 항상 값이 바뀌어야 한다.
    #[test]
    fn 강도는_사다리_끝에서_순환한다() {
        let level = |step, current| {
            let Ok(EffortPlan::Slash { level, .. }) =
                plan(AgentProvider::Claude, step, Some(current))
            else {
                panic!("{current}에서 계획이 나와야 한다");
            };
            level
        };
        assert_eq!(level(EffortStep::Up, "max"), "low");
        assert_eq!(level(EffortStep::Down, "low"), "max");
        // 중간은 그대로 한 칸씩.
        assert_eq!(level(EffortStep::Up, "high"), "xhigh");
        assert_eq!(level(EffortStep::Down, "high"), "medium");
    }

    /// 현재 강도를 모르면 **아무것도 보내지 않는다.** 임의로 "high"를 가정하면
    /// 사용자가 누른 적 없는 값으로 세션과 전역 기본값이 함께 바뀐다.
    #[test]
    fn 현재_강도를_모르면_보내지_않는다() {
        assert_eq!(
            plan(AgentProvider::Claude, EffortStep::Up, None),
            Err(EffortBlocked::UnknownCurrentEffort)
        );
        assert_eq!(
            plan(AgentProvider::Claude, EffortStep::Up, Some("ultracode")),
            Err(EffortBlocked::UnknownCurrentEffort)
        );
    }

    /// 사다리가 런처 내장 카탈로그와 갈라지면, 사용자는 런처 드롭다운에는 있는데
    /// 단축키로는 못 가는 모델을 보게 된다.
    #[test]
    fn 두_카탈로그가_어긋나지_않는다() {
        assert_eq!(
            CLAUDE_MODEL_LADDER.to_vec(),
            crate::agent_launcher::AgentKind::Claude.builtin_model_values()
        );
    }

    /// statusLine은 표시명("Opus 5 (1M context)")을, `/model`은 슬러그(`opus`)를 쓴다.
    /// 이 변환이 깨지면 Claude 모델 전환이 통째로 조용히 죽는다.
    #[test]
    fn 표시명에서_슬러그를_알아낸다() {
        // 사용자 DB에 실제로 들어 있던 값들이다.
        for (current, expected) in [
            ("Sonnet 5", "opus"),
            ("Opus 5", "fable"),
            ("Opus 5 (1M context)", "fable"),
        ] {
            assert_eq!(
                plan_model(AgentProvider::Claude, EffortStep::Up, Some(current)),
                Ok(EffortPlan::Slash {
                    line: format!("/model {expected}\r"),
                    level: expected,
                }),
                "{current}에서 다음 모델이 {expected}가 아니다"
            );
        }
    }

    /// 3개뿐이라 끝에서 막히면 답답하다 — 순환해야 한다.
    #[test]
    fn 모델은_사다리_끝에서_순환한다() {
        let Ok(EffortPlan::Slash { level, .. }) =
            plan_model(AgentProvider::Claude, EffortStep::Up, Some("Fable 5"))
        else {
            panic!("계획이 나와야 한다");
        };
        assert_eq!(level, "sonnet");

        let Ok(EffortPlan::Slash { level, .. }) =
            plan_model(AgentProvider::Claude, EffortStep::Down, Some("Sonnet 5"))
        else {
            panic!("계획이 나와야 한다");
        };
        assert_eq!(level, "fable");
    }

    /// Codex `/model <슬러그>`는 프롬프트로 흘러 **실제 턴을 태운다**. 지원한다고
    /// 표시하면 단축키 한 번이 토큰을 쓴다.
    #[test]
    fn codex는_모델_전환을_지원하지_않는다() {
        assert!(!supports_model(AgentProvider::Codex));
        assert!(supports_model(AgentProvider::Claude));
        assert_eq!(
            plan_model(AgentProvider::Codex, EffortStep::Up, Some("gpt-5.6-sol")),
            Err(EffortBlocked::UnknownCurrentEffort)
        );
    }

    /// 모르는 표시명이면 아무것도 보내지 않는다 — 임의로 첫 모델을 고르면
    /// 사용자가 쓰던 모델이 말없이 바뀌고 전역 기본값까지 따라간다.
    #[test]
    fn 모르는_모델이면_보내지_않는다() {
        for current in [None, Some("Haiku 9"), Some("")] {
            assert_eq!(
                plan_model(AgentProvider::Claude, EffortStep::Up, current),
                Err(EffortBlocked::UnknownCurrentEffort),
                "{current:?}에서 계획이 나왔다"
            );
        }
    }

    /// 낙관적 값을 언제 버릴지 판정한다. 이게 어긋나면 둘 중 하나가 된다 —
    /// 너무 일찍 버리면 연속 입력이 다시 낡은 값에서 움직이고, 안 버리면 사용자가
    /// CLI에서 직접 바꾼 값을 영영 무시한다.
    #[test]
    fn 권위값이_따라잡으면_낙관적_값을_버린다() {
        use AdjustKind::{Effort, Model};
        // 강도는 표기가 같다.
        assert!(authoritative_caught_up(Effort, "high", "high"));
        assert!(authoritative_caught_up(Effort, "HIGH", "high"));
        assert!(authoritative_caught_up(Effort, "  high  ", "high"));
        assert!(!authoritative_caught_up(Effort, "medium", "high"));
        // 모델은 권위 쪽이 표시명이라 슬러그 포함으로 본다.
        assert!(authoritative_caught_up(
            Model,
            "Opus 5 (1M context)",
            "opus"
        ));
        assert!(authoritative_caught_up(Model, "Sonnet 5", "sonnet"));
        assert!(!authoritative_caught_up(Model, "Sonnet 5", "opus"));
    }

    /// 슬래시 명령은 CR로 끝나야 제출된다. 실측에서 CR 없이는 composer에 글자만
    /// 남았다.
    #[test]
    fn 슬래시_명령은_cr로_끝난다() {
        let Ok(EffortPlan::Slash { line, .. }) =
            plan(AgentProvider::Claude, EffortStep::Up, Some("low"))
        else {
            panic!("Claude는 슬래시 계획이어야 한다");
        };
        assert!(line.ends_with('\r'), "{line:?}가 CR로 끝나지 않는다");
        assert!(!line.contains('\n'), "개행이 섞이면 두 번 제출된다");
    }
}
