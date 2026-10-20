#![allow(dead_code)]
#[path="/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/crates/runtime/src/input_admission.rs"] mod input_admission;
use input_admission::*;
use std::time::{Instant,Duration};
fn main(){
 let spec=pty::CommandSpec{program:"/bin/sh".into(),args:vec!["-c".into(),"stty raw -echo; printf '› '; exec /bin/cat".into()],env:vec![],cwd:Some("/private/tmp".into())};
 let mut live=session::Session::spawn_with_spec(deppy_core::SessionId(1),session::SessionKind::Agent,&spec,80,24,100).unwrap();
 let deadline=Instant::now()+Duration::from_secs(5);
 while !live.screen_text().contains('›'){assert!(Instant::now()<deadline);live.pump(|_|{});std::thread::sleep(Duration::from_millis(1));}
 let group=live.process_identity().process_group.unwrap();assert_eq!(live.foreground_process_group(),Some(group));
 let guard=AgentInputGuard{foreground_process_group:group,provider:AgentPromptKind::Codex,intent:AgentInputIntent::AutomaticPrompt};
 let detector=session::StatusDetector::new(session::StatusPatterns::compile(None,None,None,None));
 assert!(guard.allows(&live,Some(&detector)));
 let admission=InputAdmission::new(InputPermit::new(),deadline,|admit|admit()).with_agent_guard(guard);
 let outcome=admission.admit(||{let(_,deferred)=live.pump_for_input_guard(|_|{});drop(deferred);assert_eq!(live.pending_output_bytes(),0);assert!(guard.allows(&live,Some(&detector)));live.write_input_batch(&[b"RETIRED-WORKFLOW-PROMPT\r"]) });
 assert!(matches!(outcome,Some(Some(pty::PtyInputEnqueueResult::Accepted))));
 while !live.screen_text().contains("RETIRED-WORKFLOW-PROMPT"){assert!(Instant::now()<deadline);live.pump(|_|{});std::thread::sleep(Duration::from_millis(1));}
 println!("DOWNSTREAM_ADMISSION: production InputAdmission and AgentInputGuard accept AutomaticPrompt at unchanged empty Codex editor, and actual owned PTY receives it; these checks have no workflow/task/attempt state");
}
