
#[cfg(test)]
mod independent_scope_review {
    use super::*;
    fn project(extra: Value) -> AgentApproval {
        let mut command = Command::new("cat");
        command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut child = ChildTreeGuard::new(command.spawn().unwrap());
        let mut stdin = AppServerStdin::new(child.child_mut().stdin.take().unwrap(), Arc::new(AtomicBool::new(false))).unwrap();
        let mut state = WorkerState::default();
        state.thread_to_session.insert("thread".into(), "owner".into());
        state.session_to_thread.insert("owner".into(), "thread".into());
        state.session_to_turn.insert("owner".into(), "turn".into());
        let events = EventBacklog::default();
        let mut params = json!({"threadId":"thread","turnId":"turn","itemId":"item","startedAtMs":1,"command":"printf approved","cwd":"/repo"});
        for (key,value) in extra.as_object().unwrap() {params[key]=value.clone();}
        handle_server_request(json!({"id":"same-request","method":"item/commandExecution/requestApproval","params":params}), &mut stdin, &mut state, &events, &egui::Context::default());
        let approval = events.drain().into_iter().find_map(|event| match event {
            CodexAppServerEvent::Session {event:AgentSessionEvent::ApprovalRequested {approval},..} => Some(approval),_=>None
        }).expect("production provider parser emits approval");
        child.terminate();
        approval
    }
    fn scope_must_survive(extra: Value) {
        let plain = project(json!({}));
        let scoped = project(extra);
        assert_ne!(approval_request_fingerprint(&plain), approval_request_fingerprint(&scoped), "provider scope changed, so stale plain-command confirmation must not remain valid");
        assert_ne!(plain,scoped,"controller projection must retain enough scope to disable unsupported approvals");
    }
    #[test] fn independent_write_stdin_must_not_share_plain_command_authority() {scope_must_survive(json!({"kind":"writeStdin","approvalId":"stdin-callback"}));}
    #[test] fn independent_execution_environment_must_not_share_local_authority() {scope_must_survive(json!({"environmentId":"remote-env"}));}
    #[test] fn independent_network_scope_must_not_share_plain_command_authority() {scope_must_survive(json!({"networkApprovalContext":{"host":"private.example","protocol":"https"}}));}
}

#[cfg(test)] mod independent_control_review {
    use super::*;
    #[test] fn independent_actual_approval_control_failure_preserves_lifecycle() {
        let mut command=Command::new("cat"); command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());configure_process_group(&mut command);
        let mut child=ChildTreeGuard::new(command.spawn().unwrap());let stopped=Arc::new(AtomicBool::new(false));let mut stdin=AppServerStdin::new(child.child_mut().stdin.take().unwrap(),stopped.clone()).unwrap();
        let mut state=WorkerState {initialized:true,..Default::default()};state.session_to_thread.insert("owner".into(),"thread".into());state.session_to_turn.insert("owner".into(),"turn".into());
        let events=EventBacklog::default();handle_client_command(ClientCommand::RespondApproval {session_id:"owner".into(),request_key:"already-resolved".into(),decision:AgentApprovalDecision::Accept,expected:Some([1;32]),guard:None},&mut stdin,&mut state,&events,&egui::Context::default());
        let mut emitted=events.drain();assert_eq!(emitted.len(),1);let CodexAppServerEvent::Session {session_id,event}=emitted.pop().unwrap() else {panic!("control failure must remain session scoped")};assert_eq!(session_id,"owner");assert!(matches!(&event,AgentSessionEvent::ControlError {..}));
        let mut session=crate::agent_session::AgentSession::new("owner".into(),"pending".into(),None);session.status=crate::agent_session::AgentSessionStatus::AwaitingApproval;session.apply(event);assert_eq!(session.status,crate::agent_session::AgentSessionStatus::AwaitingApproval);assert!(!stopped.load(Ordering::Acquire));child.terminate();
    }
}
