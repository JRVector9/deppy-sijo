"""Generate saved conversations from constants; no user transcript/config reads."""
import json
from pathlib import Path
from urllib.parse import quote
import uuid

KEY = "offline-fixture-not-a-real-key"
TABLE = """Fixture paragraph: ordinary prose should wrap at the new pane width without losing words. This local fixture has no model request.

| Agent | Task | Status |
| --- | --- | --- |
| ROW_ALPHA | 한글 표의 셀 줄바꿈과 창 너비 변경을 확인합니다. Local synthetic table with enough content to wrap across several lines. | ready |
| ROW_BETA | Independent session data stays intact when the pane becomes narrower and wider again. | waiting |
| ROW_OMEGA | 원문과 복사 내용을 보존합니다. | done |
"""
FILLER = "\n\n".join(f"History filler {i:02}: saved offline content keeps the preceding table outside the live viewport." for i in range(40)) + "\n\nEND_OF_HISTORY_FIXTURE"
STAMP = "2026-10-22T00:00:00Z"


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def generate(root, provider, version, history):
    root = Path(root)
    profile, project = root / "profile", root / "project"
    profile.mkdir(parents=True)
    project.mkdir()
    sid = str(uuid.uuid4())
    turns = [("user", "Display the saved local fixture."), ("assistant", TABLE)]
    if history:
        turns += [("user", "Show saved offline details."), ("assistant", FILLER)]
    records = []
    if provider == "claude":
        write_json(profile / ".claude.json", {"hasCompletedOnboarding": True, "numStartups": 5,
            "customApiKeyResponses": {"approved": [KEY[-20:]], "rejected": []},
            "hasResetAutoModeOptInForDefaultOffer": True,
            "projects": {str(project): {"hasTrustDialogAccepted": True}}})
        write_json(profile / "settings.json", {"permissions": {"defaultMode": "default"}, "disableAllHooks": True})
        parent = None
        for index, (role, text) in enumerate(turns):
            uid = str(uuid.uuid4())
            message = {"role": role, "content": text}
            if role == "assistant":
                message = {"id": f"msg_fixture_{index}", "type": "message", "role": role,
                    "model": "claude-opus-4-6", "content": [{"type": "text", "text": text}],
                    "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 1}}
            records.append({"type": role, "uuid": uid, "parentUuid": parent, "sessionId": sid,
                "cwd": str(project), "version": version.split()[0], "timestamp": STAMP, "message": message})
            parent = uid
        # Claude's project directory encodes each non-alphanumeric path character.
        encoded = "".join(char if char.isalnum() else "-" for char in str(project))
        transcript = profile / "projects" / encoded / f"{sid}.jsonl"
    elif provider == "codex":
        (profile / "config.toml").write_text('model = "gpt-6.1-sol"\nmodel_reasoning_effort = "xhigh"\nmodel_provider = "fixture"\ncli_auth_credentials_store = "file"\ncheck_for_update_on_startup = false\n[model_providers.fixture]\nname = "Local offline fixture"\nbase_url = "http://127.0.0.1:9/v1"\nenv_key = "DEPPY_TABLE_FIXTURE_KEY"\nwire_api = "responses"\n[projects.' + json.dumps(str(project)) + ']\ntrust_level = "trusted"\n[tui]\nscreen_reader_detection_done = true\n[analytics]\nenabled = false\n')
        records.append({"timestamp": STAMP, "type": "session_meta", "payload": {"id": sid,
            "timestamp": STAMP, "cwd": str(project), "originator": "codex_cli_rs", "cli_version": version,
            "source": "cli", "model_provider": "fixture", "base_instructions": {"text": ""}}})
        for role, text in turns:
            records.append({"timestamp": STAMP, "type": "response_item", "payload": {
                "type": "message", "role": role, "content": [{"type": "input_text" if role == "user" else "output_text", "text": text}],
                **({"phase": "final_answer"} if role == "assistant" else {})}})
            records.append({"timestamp": STAMP, "type": "event_msg", "payload": {
                "type": "user_message" if role == "user" else "agent_message", "message": text}})
            if role == "assistant":
                records.append({"timestamp": STAMP, "type": "event_msg", "payload": {
                    "type": "task_complete", "turn_id": str(uuid.uuid4()), "last_agent_message": text}})
        transcript = profile / "sessions/2026/10/22" / f"rollout-2026-10-22T00-00-00-{sid}.jsonl"
    elif provider == "grok":
        (profile / "config.toml").write_text('[cli]\nauto_update = false\n[features]\ntelemetry = false\nfeedback = false\nremote_fetch = false\nmanaged_config = false\ncodebase_indexing = false\n[session]\nload_envrc = false\n[marketplace]\ndefault_skills_installs_purged = true\n[compat.claude]\nhooks = false\nmcps = false\n[compat.codex]\nhooks = false\n[compat.cursor]\nhooks = false\nmcps = false\n')
        (profile / "trusted_folders.toml").write_text('[folders.' + json.dumps(str(project)) + ']\ntrusted = true\ndecided_at = 1792627200\n')
        session_dir = profile / "sessions" / quote(str(project), safe="") / sid
        write_json(session_dir / "summary.json", {"info": {"id": sid, "cwd": str(project)},
            "agent_id": "ag1." + uuid.uuid4().hex, "attempt_id": "at1." + uuid.uuid4().hex,
            "session_summary": "Offline resize fixture", "created_at": STAMP, "updated_at": STAMP,
            "num_messages": len(turns), "num_chat_messages": len(turns), "current_model_id": "grok-4.6",
            "next_trace_turn": 0, "chat_format_version": 1, "last_active_at": STAMP,
            "agent_name": "grok-build-plan-no-subagents", "reasoning_effort": "xhigh"})
        for role, text in turns:
            records.append({"timestamp": 1792627200, "method": "session/update", "params": {"sessionId": sid,
                "update": {"sessionUpdate": "user_message_chunk" if role == "user" else "agent_message_chunk",
                           "content": {"type": "text", "text": text}}}})
        transcript = session_dir / "updates.jsonl"
    else:
        raise ValueError(provider)
    transcript.parent.mkdir(parents=True, exist_ok=True)
    transcript.write_text("".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records))
    (root / "table.md").write_text(TABLE)
    return {"session_id": sid, "profile": profile, "project": project, "transcript": transcript}
