//! Bounded machine errors. Internal diagnostics must never become wire messages.
use serde_json::{Value, json};

fn safe_error_code(error: &str) -> &str {
    match error {
        "busy_no_effect"
        | "completion_native_proof_unsupported"
        | "completion_invalid_scope"
        | "completion_target_busy"
        | "completion_watch_capacity"
        | "completion_original_request_required_no_effect"
        | "completion_transport_required_no_effect"
        | "invalid_completion_identity_no_effect"
        | "completion_result_too_large_no_effect"
        | "completion_revoked_no_effect"
        | "completion_busy_no_effect"
        | "completion_capacity_no_effect"
        | "local_task_busy_no_effect"
        | "completion_transport_already_reserved_no_effect"
        | "completion_transport_closed_no_effect"
        | "completion_history_full_no_effect"
        | "completion_operation_already_claimed_no_effect"
        | "completion_result_too_large_unknown"
        | "completion_identity_mismatch"
        | "completion_not_claimed"
        | "completion_receipt_conflict"
        | "completion_admission_conflict"
        | "completion_result_too_large"
        | "invalid_completion_identity"
        | "invalid_completion_receipt"
        | "poll_budget_no_effect"
        | "timeout_no_effect"
        | "outcome_unknown_do_not_retry_input"
        | "internal_error_outcome_unknown"
        | "expired_or_revoked_request_no_effect"
        | "shutdown_no_effect"
        | "history_busy_no_effect"
        | "history_unavailable"
        | "history_read_failed"
        | "history_unavailable_no_effect"
        | "history_query_failed_no_effect"
        | "history_arguments_too_large"
        | "history_identity_too_large"
        | "history_outcome_too_large"
        | "answer_too_large"
        | "operation_conflict_or_history_unavailable_no_effect"
        | "operation_id_conflict"
        | "operation_context_conflict"
        | "operation_context_mismatch"
        | "operation_context_missing"
        | "operation_history_full"
        | "operation_not_claimed"
        | "session_changed_or_closed"
        | "session_not_shared"
        | "session_or_permission_changed_no_effect"
        | "session_control_busy"
        | "input_not_allowed_or_session_exited"
        | "oauth_input_scope_required"
        | "client_input_not_allowed"
        | "client_read_not_allowed"
        | "client_permission_limit"
        | "client_session_not_allowed"
        | "client_permission_revoked_no_effect"
        | "connection_not_active"
        | "control_lease_limit"
        | "control_epoch_exhausted"
        | "control_not_owned"
        | "control_required_no_effect"
        | "invalid_control_lease"
        | "ai_execution_unconfirmed_no_effect"
        | "bracketed_paste_required_no_effect"
        | "paste_arguments_too_large_no_effect"
        | "paste_contains_controls_or_invalid_size"
        | "text_contains_controls_or_invalid_size"
        | "pty_writer_unavailable"
        | "run_not_loaded_or_not_owned"
        | "history_query_timeout_no_effect"
        | "runtime_changed_no_effect"
        | "session_changed_or_closed_no_effect"
        | "runtime_queue_rejected_no_effect"
        | "connection_stopped_no_effect"
        | "token_redaction_capacity_no_connection"
        | "cloud_snapshot_unavailable"
        | "invalid_connection_profile"
        | "connection_profile_limit"
        | "connection_must_be_stopped"
        | "pty_admission_unknown"
        | "pty_admission_revoked"
        | "pty_deadline_expired"
        | "pty_queue_full"
        | "pty_session_closed"
        | "pty_input_too_large"
        | "pty_input_invalid"
        | "runtime_rejected"
        | "runtime_error"
        | "unknown_tool"
        | "invalid_arguments"
        | "invalid_session_id"
        | "invalid_generation"
        | "invalid_operation_id"
        | "invalid_run_id"
        | "invalid_title"
        | "invalid_instruction"
        | "invalid_state"
        | "invalid_progress"
        | "invalid_message"
        | "invalid_result"
        | "invalid_artifacts"
        | "invalid_submit"
        | "invalid_text"
        | "invalid_paste_size"
        | "invalid_cursor"
        | "invalid_limit"
        | "invalid_max_bytes"
        | "invalid_before"
        | "invalid_ttl_seconds"
        | "invalid_identity"
        | "invalid_client_permission"
        | "invalid_history_query"
        | "invalid_run_query"
        | "invalid_run"
        | "invalid_run_state"
        | "invalid_run_progress"
        | "invalid_run_message"
        | "invalid_run_result"
        | "invalid_run_artifacts"
        | "invalid_run_report"
        | "invalid_stored_run"
        | "invalid_run_time"
        | "invalid_run_transition"
        | "invalid_artifact"
        | "invalid_artifact_uri"
        | "invalid_cancel_state"
        | "run_actor_mismatch"
        | "run_identity_changed"
        | "run_report_out_of_order"
        | "run_revision_conflict"
        | "run_revision_full"
        | "run_terminal"
        | "run_too_large"
        | "run_history_full"
        | "run_not_started"
        | "run_filter_mismatch"
        | "run_unknown_is_local_observation"
        | "run_update_busy_no_effect"
        | "run_not_owned_or_session_changed"
        | "run_id_conflict"
        | "run_not_found"
        | "invalid_run_transition_or_report"
        | "run_terminal_or_cancel_invalid"
        | "history_reply_mismatch"
        | "run_save_unknown_query_before_retry"
        | "run_save_failed"
        | "run_recovery_failed"
        | "run_history_read_failed"
        | "run_text_too_large_or_invalid"
        | "run_report_too_large"
        | "cancel_not_requested"
        | "cancel_request_changed" => error,
        _ => "internal_error_outcome_unknown",
    }
}

pub(crate) fn error_payload(error: &str) -> Value {
    let code = safe_error_code(error);
    let unknown = code.contains("unknown")
        || code.contains("do_not_retry")
        || matches!(
            code,
            "history_reply_mismatch" | "run_save_failed" | "run_recovery_failed"
        );
    let retryable = !unknown
        && matches!(
            code,
            "busy_no_effect"
                | "completion_busy_no_effect"
                | "completion_capacity_no_effect"
                | "poll_budget_no_effect"
                | "timeout_no_effect"
                | "history_busy_no_effect"
                | "history_query_timeout_no_effect"
                | "run_update_busy_no_effect"
        );
    let mut payload = json!({"error":code,"code":code,"retryable":retryable,"effect_state":if unknown {"unknown"} else {"none"}});
    if retryable {
        payload["retry_after_ms"] = json!(1000);
    }
    payload
}

pub(crate) fn normalize_tool_payload(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        if let Some(error) = object.get("error") {
            let normalized = error_payload(error.as_str().unwrap_or(""));
            object.extend(normalized.as_object().unwrap().clone());
        }
        if object.get("local_completion").is_some_and(Value::is_object)
            && let Some(admission) = object.get("admission").and_then(Value::as_object)
        {
            // A later local observation does not change the input's recorded
            // admission, and returning a result never acknowledges model use.
            let effect = match admission.get("status").and_then(Value::as_str) {
                Some("queued") => "queued",
                Some("rejected") => "none",
                _ => "unknown",
            };
            object.insert("effect_state".into(), json!(effect));
            object.insert("retry".into(), json!(false));
            object.insert("retryable".into(), json!(false));
            object.remove("retry_after_ms");
            return payload;
        }
        match object.get("status").and_then(Value::as_str) {
            Some("unknown") => {
                object.insert("effect_state".into(), json!("unknown"));
                object.insert("retryable".into(), json!(false));
                if object.contains_key("retry") {
                    object.insert("retry".into(), json!(false));
                }
                object.remove("retry_after_ms");
            }
            Some("rejected") => {
                object.insert("effect_state".into(), json!("none"));
            }
            Some("queued") => {
                object.insert("effect_state".into(), json!("queued"));
            }
            Some("stored") => {
                object.insert("effect_state".into(), json!("stored"));
            }
            _ => {
                object.entry("effect_state").or_insert(json!("none"));
            }
        }
        object.entry("retryable").or_insert(json!(false));
    }
    payload
}
