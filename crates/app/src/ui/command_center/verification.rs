//! Explicit local command execution and evidence, separate from user-entered reports.
use super::*;

pub(super) fn current_target(task: &Task) -> Option<&VerificationTarget> {
    task.verification_target.as_ref().filter(|target| {
        target.validate().is_ok()
            && target.workspace_id.0 == task.target.workspace_id
            && task.target.task_id.as_deref() == Some(target.task_id.0.as_str())
            && task.attempt_id.as_deref() == Some(target.attempt_id.0.as_str())
            && task.result_revision.as_deref()
                == Some(target.result_revision.0.to_string().as_str())
    })
}

fn outcome_label(outcome: CommandVerificationOutcome) -> &'static str {
    match outcome {
        CommandVerificationOutcome::Running => "실행 중",
        CommandVerificationOutcome::Passed => "통과",
        CommandVerificationOutcome::NonzeroExit => "실패 · 종료 코드",
        CommandVerificationOutcome::SpawnFailed => "시작 실패",
        CommandVerificationOutcome::TimedOut => "시간 초과",
        CommandVerificationOutcome::Cancelled => "취소됨",
        CommandVerificationOutcome::Interrupted => "중단됨",
        CommandVerificationOutcome::IoFailed => "출력 읽기 실패",
    }
}

fn output(ui: &mut egui::Ui, snapshot: &Snapshot, caption: &str, value: &str, id: &str) {
    ui.add_space(8.0);
    weak(ui, snapshot, caption);
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(180.0)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(if value.is_empty() {
                        "(출력 없음)"
                    } else {
                        value
                    })
                    .monospace()
                    .size(11.0 * snapshot.font_scale),
                )
                .wrap()
                .selectable(true),
            );
        });
}

pub(super) fn show(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    task: &Task,
    intents: &mut Vec<Intent>,
) {
    let Some(expected) = current_target(task) else {
        return;
    };
    let evidence = task
        .command_verification
        .as_ref()
        .filter(|e| e.target == *expected && e.validate().is_ok());
    if let Some(evidence) = evidence {
        rule(ui, snapshot);
        ui.label(
            egui::RichText::new(format!(
                "앱 실행 검증 · {}",
                outcome_label(evidence.outcome)
            ))
            .strong(),
        );
        ui.add(
            egui::Label::new(egui::RichText::new(&evidence.spec.command).monospace())
                .wrap()
                .selectable(true),
        );
        weak(ui, snapshot, format!("실행 폴더 · {}", evidence.spec.cwd));
        weak(
            ui,
            snapshot,
            format!(
                "시도 · {} · 결과 revision {}",
                expected.attempt_id, expected.result_revision.0
            ),
        );
        if let Some(source) = &expected.source_revision {
            weak(ui, snapshot, format!("원본 식별자 · {source}"));
        }
        if let Some(artifact) = &expected.artifact_revision {
            weak(ui, snapshot, format!("산출물 식별자 · {artifact}"));
        }
        weak(
            ui,
            snapshot,
            format!(
                "종료 코드 · {}",
                evidence
                    .exit_code
                    .map_or_else(|| "미확정".into(), |code| code.to_string())
            ),
        );
        if let Some(ended) = evidence.ended_at_ms {
            weak(
                ui,
                snapshot,
                format!(
                    "소요 시간 · {}ms",
                    ended.saturating_sub(evidence.started_at_ms)
                ),
            );
        }
        ui.push_id(&evidence.id.0, |ui| {
            output(ui, snapshot, "표준 출력", &evidence.stdout, "stdout");
            output(ui, snapshot, "표준 오류", &evidence.stderr, "stderr");
        });
        if evidence.stdout_truncated || evidence.stderr_truncated {
            weak(ui, snapshot, "출력이 제한되어 일부만 표시합니다.");
        }
        if let Some(error) = &evidence.error {
            weak(ui, snapshot, error);
        }
        if evidence.outcome == CommandVerificationOutcome::Running {
            if action(ui, snapshot, "검증 실행 취소", &task.cancel_verification) {
                intents.push(Intent::CancelVerification {
                    target: task.target.clone(),
                    expected: expected.clone(),
                    verification_id: evidence.id.clone(),
                });
            }
            return;
        }
    }
    if !task.execute_verification.enabled && task.execute_verification.reason.is_empty() {
        return;
    }
    rule(ui, snapshot);
    ui.label(egui::RichText::new("이 Mac에서 검증 실행").strong());
    weak(
        ui,
        snapshot,
        format!(
            "현재 결과 revision {} · 시도 {}",
            expected.result_revision.0, expected.attempt_id
        ),
    );
    let key = serde_json::to_string(expected).unwrap_or_default();
    if !state.command_verification_drafts.contains_key(&key)
        && state.command_verification_drafts.len() >= 256
    {
        weak(ui, snapshot, "검증 명령 초안 한도에 도달했습니다.");
        return;
    }
    let draft = state
        .command_verification_drafts
        .entry(key)
        .or_insert_with(|| VerificationSpec {
            command: String::new(),
            cwd: task.directory.clone(),
            timeout_ms: 60_000,
            output_limit_bytes: 32 * 1024,
        });
    ui.label("검증 명령");
    ui.add(
        egui::TextEdit::multiline(&mut draft.command)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .char_limit(16 * 1024),
    );
    ui.label("실행 폴더");
    ui.add(
        egui::TextEdit::singleline(&mut draft.cwd)
            .desired_width(f32::INFINITY)
            .char_limit(8192),
    );
    ui.horizontal_wrapped(|ui| {
        ui.label("제한 시간");
        let mut seconds = draft.timeout_ms / 1000;
        if ui
            .add(
                egui::DragValue::new(&mut seconds)
                    .range(1..=600)
                    .suffix("초"),
            )
            .changed()
        {
            draft.timeout_ms = seconds * 1000;
        }
    });
    weak(
        ui,
        snapshot,
        "명령과 폴더를 확인한 뒤 실행하세요. 표준 출력·오류는 각각 최대 32 KiB로 기록합니다.",
    );
    let capability = Capability {
        enabled: task.execute_verification.enabled && draft.validate().is_ok(),
        reason: task.execute_verification.reason.clone(),
    };
    if action(ui, snapshot, "검증 명령 실행", &capability) {
        intents.push(Intent::ExecuteVerification {
            target: task.target.clone(),
            expected: expected.clone(),
            spec: draft.clone(),
        });
    }
    if !task.execute_verification.enabled && !task.execute_verification.reason.is_empty() {
        weak(ui, snapshot, &task.execute_verification.reason);
    }
}
