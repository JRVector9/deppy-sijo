const APP_SOURCE: &str = include_str!("../src/app.rs");

fn production_source() -> &'static str {
    APP_SOURCE
        .split_once("\n#[cfg(test)]\nmod tests")
        .expect("app.rs must keep one trailing cfg(test) module")
        .0
}

fn compact(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("fn {name}(");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("production function `{name}` is missing"));
    let open = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("production function `{name}` has no body"));
    let mut depth = 0usize;
    for (offset, byte) in source.as_bytes()[open..].iter().copied().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1).unwrap_or_else(|| {
                    panic!("production function `{name}` has unbalanced braces")
                });
                if depth == 0 {
                    return &source[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("production function `{name}` has an unterminated body")
}

#[test]
fn legacy_dotenv_poll_and_empty_restore_fallback_symbols_are_absent() {
    let production = production_source();
    for symbol in [
        "poll_dotenv_change",
        "poll_restore_timeout",
        "restore_pending_since",
        "last_dotenv_check",
    ] {
        assert_eq!(
            production.matches(symbol).count(),
            0,
            "legacy dotenv polling/empty-restore symbol remains in production app.rs: {symbol}"
        );
    }
}

#[test]
fn process_capable_runtime_variants_have_no_direct_send_command_construction() {
    let production = compact(production_source());
    for variant in ["SpawnShell", "SpawnAgent", "SplitPane", "RestoreWorkspace"] {
        for prefix in [
            "send_command(runtime::RuntimeCommand::",
            "send_command(RuntimeCommand::",
        ] {
            let pattern = format!("{prefix}{variant}");
            assert_eq!(
                production.matches(&pattern).count(),
                0,
                "process-capable {variant} bypasses the exact dotenv continuation: {pattern}"
            );
        }
    }
}

#[test]
fn dotenv_gate_classifies_every_process_capable_runtime_variant() {
    let body = function_body(production_source(), "runtime_command_requires_dotenv");
    for variant in ["SpawnShell", "SpawnAgent", "SplitPane", "RestoreWorkspace"] {
        let pattern = format!("RuntimeCommand::{variant}");
        assert_eq!(
            body.matches(&pattern).count(),
            1,
            "dotenv launch classifier must contain {pattern} exactly once"
        );
    }
}

#[test]
fn production_perf_harness_still_uses_its_frozen_entrypoints() {
    let production = compact(production_source());
    for callsite in [
        "crate::perf::harness_enabled()",
        "crate::perf::harness_command(",
    ] {
        assert!(
            production.contains(callsite),
            "production perf harness entrypoint disappeared during launch cutover: {callsite}"
        );
    }
}

#[test]
fn agent_resume_write_input_process_exception_is_explicitly_tracked() {
    let body = function_body(production_source(), "apply_resume_probe_results");
    for marker in [
        "claude --resume",
        "codex resume",
        "RuntimeCommand::WriteInput",
        "dotenv_state_for_sources",
        "resume_probe_completion_allowed",
    ] {
        assert!(
            body.contains(marker),
            "the known ResumeAgent WriteInput process exception or its completion-time freshness gate changed ({}); update the gated flow or remove this debt sentinel",
            marker
        );
    }
}
