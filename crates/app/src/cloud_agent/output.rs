//! Bounded, read-only terminal history requests. Never changes native scroll position.
use super::{CloudAgent, Effect, MAX_OPERATIONS, Request, Target, field, now, truncate_utf8};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Instant};

#[derive(Default)]
pub(super) struct OutputState {
    pending: HashMap<u64, Pending>,
    cursors: HashMap<u64, Cursor>,
}

struct Pending {
    request: Request,
    target: Target,
    auth: Arc<agent_mcp::Auth>,
    max_bytes: usize,
}
struct Cursor {
    connection: String,
    session: String,
    generation: String,
    anchor: terminal::TerminalHistoryAnchor,
    rows: u16,
    created: Instant,
}

impl CloudAgent {
    pub(super) fn prepare_history_query(
        &mut self,
        req: &Request,
        target: &Target,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Result<Option<Value>, String> {
        self.expire_history_queries();
        if self.output.pending.len() >= MAX_OPERATIONS {
            return Err("busy_no_effect".into());
        }
        let max_bytes = req
            .args
            .get("max_bytes")
            .map(|value| {
                value
                    .as_u64()
                    .filter(|bytes| (1024..=65536).contains(bytes))
                    .ok_or("invalid_max_bytes")
            })
            .transpose()?
            .unwrap_or(65536) as usize;
        let cursor = req
            .args
            .get("cursor")
            .map(|v| v.as_u64().ok_or("invalid_cursor"))
            .transpose()?
            .unwrap_or(0);
        let (anchor, delta) = if cursor == 0 {
            (None, 0)
        } else {
            let cursor = self
                .output
                .cursors
                .get(&cursor)
                .filter(|c| {
                    c.connection == req.principal.id
                        && c.session == target.id
                        && c.generation == target.generation
                })
                .ok_or("invalid_cursor")?;
            (Some(cursor.anchor), i32::from(cursor.rows))
        };
        let token = self.operation_token() | (1_u64 << 62);
        let query = terminal::TerminalHistoryQuery {
            anchor,
            delta,
            reset: anchor.is_none(),
            rows: Some(256),
        };
        send(
            target,
            Effect::History {
                operation_id: token,
                query,
            },
        )?;
        let mut target = target.clone();
        target.screen = None;
        self.output.pending.insert(token, Pending {
            request: Request { deferred: None, epoch:req.epoch, principal:req.principal.clone(), access_key:req.access_key.clone(), input_scope:req.input_scope, deadline:req.deadline, tool:req.tool.clone(), args:json!({"session_id":field(&req.args,"session_id",128)?,"generation":target.generation}), reply:req.reply.clone() },
            target, auth:self.server.as_ref().ok_or("connection_not_active")?.auth.clone(), max_bytes,
        });
        Ok(None)
    }
    pub(super) fn observe_history(&mut self, runtime: u64, events: &[runtime::RuntimeEvent]) {
        for event in events {
            let runtime::RuntimeEvent::TerminalHistoryResult {
                session,
                operation_id,
                snapshot,
                expired,
            } = event
            else {
                continue;
            };
            if !self
                .output
                .pending
                .get(operation_id)
                .is_some_and(|p| p.target.runtime == runtime && p.target.session == *session)
            {
                continue;
            }
            let pending = self
                .output
                .pending
                .remove(operation_id)
                .expect("correlated query");
            let result = if !self
                .server
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(&s.auth, &pending.auth))
                || !self.management_request_current(&pending.request, &pending.target)
            {
                Err("expired_or_revoked_request_no_effect".into())
            } else if let Some(snapshot) = snapshot {
                let (raw, extraction_truncated) = super::screen_text_with_truncation(snapshot);
                let mut text = self.redact_screen(&raw);
                let truncated = extraction_truncated || text.len() > pending.max_bytes;
                truncate_utf8(&mut text, pending.max_bytes);
                let next = snapshot
                    .history
                    .filter(|meta| {
                        let end = meta
                            .live_first_line
                            .saturating_add(u64::from(meta.native_rows));
                        meta.first_line > end.saturating_sub(u64::from(meta.total_lines))
                    })
                    .map(|meta| {
                        if self.output.cursors.len() >= 256
                            && let Some(oldest) = self
                                .output
                                .cursors
                                .iter()
                                .min_by_key(|(_, c)| c.created)
                                .map(|(id, _)| *id)
                        {
                            self.output.cursors.remove(&oldest);
                        }
                        let cursor = self.operation_token();
                        self.output.cursors.insert(
                            cursor,
                            Cursor {
                                connection: pending.request.principal.id.clone(),
                                session: pending.target.id.clone(),
                                generation: pending.target.generation.clone(),
                                anchor: terminal::TerminalHistoryAnchor {
                                    generation: meta.generation,
                                    first_line: meta.first_line,
                                },
                                rows: snapshot.rows,
                                created: Instant::now(),
                            },
                        );
                        cursor
                    });
                Ok(
                    json!({"session_id":pending.target.id,"generation":pending.target.generation,"source":"terminal_history","screen":text,"history":snapshot.history,"next_cursor":next,"reset":expired,"truncated":truncated,"observed_at":now(),"lossless":false}),
                )
            } else {
                Err("history_query_failed_no_effect".into())
            };
            let _ = pending.request.reply.try_send(result);
        }
    }
    pub(super) fn expire_history_queries(&mut self) {
        self.output.pending.retain(|_, pending| {
            if Instant::now() >= pending.request.deadline {
                let _ = pending
                    .request
                    .reply
                    .try_send(Err("history_query_timeout_no_effect".into()));
                false
            } else {
                true
            }
        });
        self.output
            .cursors
            .retain(|_, c| c.created.elapsed().as_secs() < 300);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mcp::{Server, now};
    use std::{sync::mpsc, time::Duration};
    use terminal::{AlacrittyBackend, TerminalBackend};

    fn setup(cols: u16) -> (CloudAgent, Target, AlacrittyBackend) {
        let mut bridge = CloudAgent::memory();
        bridge.server = Some(Server::start(0, "", || {}).unwrap());
        let target = Target::fixture("session", "generation-1");
        bridge.set_targets(vec![target.clone()]);
        bridge.share(&target, true);
        (bridge, target, AlacrittyBackend::new(cols, 3, 600))
    }

    fn read_page(
        bridge: &mut CloudAgent,
        target: &Target,
        backend: &AlacrittyBackend,
        cursor: Option<u64>,
        max_bytes: u64,
    ) -> (Value, u16) {
        let auth = &bridge.server.as_ref().unwrap().auth;
        let header = format!("Bearer {}", auth.token_for_user().as_str());
        let (reply, rx) = mpsc::sync_channel(1);
        let mut args =
            json!({"session_id":target.id,"generation":target.generation,"max_bytes":max_bytes});
        if let Some(cursor) = cursor {
            args["cursor"] = json!(cursor);
        }
        let req = Request {
            deferred: None,
            epoch: auth.authenticate(&header, now()).unwrap(),
            principal: auth.authenticate_principal(&header, now()).unwrap(),
            access_key: None,
            input_scope: false,
            deadline: Instant::now() + Duration::from_secs(3),
            tool: "read_history".into(),
            args,
            reply,
        };
        let mut effect = None;
        bridge.handle(req, |_, value| {
            effect = Some(value);
            Ok(())
        });
        assert!(rx.try_recv().is_err());
        let Effect::History {
            operation_id,
            query,
        } = effect.unwrap()
        else {
            panic!("history must be a read-only query");
        };
        let native_offset = backend.viewport_snapshot().unwrap().scroll_offset;
        let snapshot = backend.history_snapshot(query).unwrap();
        let rows = snapshot.snapshot.rows;
        assert_eq!(
            backend.viewport_snapshot().unwrap().scroll_offset,
            native_offset
        );
        bridge.observe_input(
            target.runtime,
            &[runtime::RuntimeEvent::TerminalHistoryResult {
                session: target.session,
                operation_id,
                snapshot: Some(Arc::new(snapshot.snapshot)),
                expired: snapshot.expired,
            }],
        );
        (
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            rows,
        )
    }

    #[test]
    fn next_cursor_reads_older_real_rows_using_actual_page_height() {
        let (mut bridge, target, mut backend) = setup(512);
        for index in 0..400 {
            backend
                .feed(format!("row-{index:04}\r\n").as_bytes())
                .unwrap();
        }
        let (first, first_rows) = read_page(&mut bridge, &target, &backend, None, 65536);
        assert_eq!(
            first_rows, 128,
            "wide pages are capped by actual cell count"
        );
        let cursor = first["next_cursor"].as_u64().unwrap();
        let (second, second_rows) = read_page(&mut bridge, &target, &backend, Some(cursor), 65536);
        let first_line = first["history"]["first_line"].as_u64().unwrap();
        let second_line = second["history"]["first_line"].as_u64().unwrap();
        assert_eq!(second_rows, first_rows);
        assert_eq!(
            second_line + u64::from(first_rows),
            first_line,
            "following a cursor must read the immediately older source page"
        );
        assert_ne!(first["screen"], second["screen"]);
    }

    #[test]
    fn capped_ascii_history_page_reports_truncation() {
        let (mut bridge, target, mut backend) = setup(256);
        let line = "X".repeat(256);
        for _ in 0..300 {
            backend.feed(format!("{line}\r\n").as_bytes()).unwrap();
        }
        // Keep the final native row full too: a trailing newline leaves a
        // blank row, bringing this ASCII page just below the byte cap.
        backend.feed(line.as_bytes()).unwrap();
        let snapshot = backend
            .history_snapshot(terminal::TerminalHistoryQuery {
                rows: Some(256),
                ..terminal::TerminalHistoryQuery::live()
            })
            .unwrap();
        let (raw, _) = super::super::screen_text_with_truncation(&snapshot.snapshot);
        assert!(
            raw.len() > 65536,
            "fixture must actually exceed the byte cap"
        );
        let (page, rows) = read_page(&mut bridge, &target, &backend, None, 65536);
        assert_eq!(rows, 256);
        assert!(page["screen"].as_str().unwrap().len() <= 65536);
        assert_eq!(
            page["truncated"], true,
            "discarded rows must be visible even when extraction capped the text first"
        );
    }

    #[test]
    fn history_redacts_known_secret_across_terminal_wraps_before_publication() {
        let (mut bridge, target, mut backend) = setup(20);
        let sensitive = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
        let secret = secret::SecretString::new(sensitive.into());
        let _lease = bridge
            .redaction
            .acquire_execution_lease(&[&secret])
            .unwrap();
        backend.feed(sensitive.as_bytes()).unwrap();
        let (page, _) = read_page(&mut bridge, &target, &backend, None, 65536);
        let reconstructed: String = page["screen"]
            .as_str()
            .unwrap()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert!(
            !reconstructed.contains(sensitive),
            "display wrapping must not make a registered secret reconstructible"
        );
    }
}
