from pathlib import Path
root = Path('/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020')
host = (root/'crates/app/src/command_center_host.rs').read_text()
observe = (root/'crates/app/src/command_center_observe.rs').read_text()
def section(text,start,end): return text[text.index(start):text.index(end,text.index(start))]
modules = '\n'.join(f'#[path="{root}/crates/app/src/work_management/{name}.rs"] pub mod {name};' for name in ['reducer','dispatch','queue'])
recover=section(host,'fn recover_observation(', '\nfn apply_user_work_event(')
admit=section(host,'fn admit_project_load(', '\nfn project_transaction')
callback=section(observe,'    pub(super) fn command_center_native_identity_result(', '    fn send_command_center_observed_prompt(')
send=section(observe,'    fn send_command_center_observed_prompt(', '    pub(super) fn observe_command_center_turn_events(')
code='''#![allow(dead_code,unused_variables,unused_imports)]
use deppy_core::{WorkspaceId, SessionId};
use deppy_core::work_management as work;
use storage::work_management as stored;
use std::collections::{BTreeMap,VecDeque};
mod wm { MODULES pub use reducer::{ProjectWorkState,ApplyOutcome}; }
use wm::ProjectWorkState;
mod remote { pub fn project_pending(_: &super::CommandCenterHost,_: &str)->bool {false} }
#[derive(Clone)] struct Project {snapshot:stored::CommandCenterSnapshot,loaded:bool,storage_blocked:bool,generation:u64,saved_generation:u64,request:Option<()>,proposal:Option<()>,confirmed:Option<wm::dispatch::ConfirmedPlan>,scopes:Vec<String>,allowed_agents:Vec<work::AgentSelection>,ledger:wm::dispatch::DeliveryLedger,revision:Option<stored::ProjectRevision>}
struct CommandCenterHost {projects:BTreeMap<String,Project>,inputs:BTreeMap<String,Input>,publication_dirty:bool}
struct Input {target:RouteTarget,dispatch:Option<wm::dispatch::DispatchAssignment>}
#[derive(Clone,PartialEq)] struct RouteTarget {workspace_id:String,task_id:Option<String>,session_id:Option<String>}
ADMIT
RECOVER
fn fixture()->(ProjectWorkState,work::ProjectConfig,wm::dispatch::ExecutionPolicy,work::GoalId) {
 let ws=WorkspaceId("review-ws".into()); let agent=work::AgentSelection{provider:"codex".into(),model:None,effort:None};
 let cfg=work::ProjectConfig{workspace_id:ws.clone(),revision:work::ConfigRevision(1),name:"Review project".into(),folder:"/private/tmp/review-project".into(),main_agent:agent.clone(),worker_agent:agent.clone(),max_workers:3,confirm_plan:true,use_worktrees:false};
 let policy=wm::dispatch::ExecutionPolicy{workspace_id:ws.clone(),config_revision:cfg.revision,project_folder:cfg.folder.clone(),worktree_root:None,allowed_agents:vec![agent.clone()],allowed_scope_roots:vec![".".into()],max_workers:3};
 let mut state=ProjectWorkState::new(ws.clone()); let goal=state.submit_goal(work::SubmissionId::new(),"Goal one".into(),"Real goal one".into()).unwrap();
 let task=work::Task{workspace_id:ws.clone(),id:work::TaskId::new(),goal_id:goal.clone(),parent_task_id:None,role:"implement".into(),scope:"src/one".into(),assigned_agent:agent,execution_device:None,title:"Implement one".into(),instructions:"Do concrete work".into(),acceptance_criteria:"Review the changed file".into(),state:work::WorkState::PlanReview,current_attempt_id:None,dependencies:vec![],result:None};
 state.install_plan(work::WorkPlan{workspace_id:ws,goal_id:goal.clone(),tasks:vec![task]}).unwrap();(state,cfg,policy,goal)
}
fn snapshot(s:&ProjectWorkState,c:&work::ProjectConfig)->stored::CommandCenterSnapshot {stored::CommandCenterSnapshot{config:c.clone(),main_binding:None,goals:s.goals.clone(),tasks:s.tasks.clone(),attempts:s.attempts.clone(),events:vec![],outbox:vec![],drafts:vec![]}}
fn state(s:&stored::CommandCenterSnapshot)->ProjectWorkState {ProjectWorkState{workspace_id:s.config.workspace_id.clone(),goals:s.goals.clone(),tasks:s.tasks.clone(),attempts:s.attempts.clone()}}
fn confirmation_restart_and_second_goal(){
 let (mut s,c,p,g1)=fixture(); let first=wm::dispatch::ConfirmedPlan::confirm(&mut s,&g1,&c,&p).unwrap();
 let dir=std::path::PathBuf::from(format!("/private/tmp/deppy-cc-review-workflow-store-{}",std::process::id()));std::fs::create_dir_all(&dir).unwrap();
 let mut db=stored::CommandCenterStore::open(&dir).unwrap();db.save_workspace(&snapshot(&s,&c),None).unwrap();let mut loaded=db.load_workspace(&c.workspace_id).unwrap().unwrap().snapshot;recover_observation(&mut loaded);let mut restored=state(&loaded);
 assert_eq!(restored.tasks[0].state,work::WorkState::Queued);assert!(restored.goals.iter().all(|g|g.state!=work::WorkState::PlanReview));
 let err=wm::dispatch::ConfirmedPlan::confirm(&mut restored,&g1,&c,&p).unwrap_err();println!("CONFIRMED_RESTART: queued task has no PlanReview recovery request; reconfirm rejected: {}",err);
 let old_task=s.tasks[0].id.clone();let g2=s.submit_goal(work::SubmissionId::new(),"Goal two".into(),"Real goal two".into()).unwrap();let mut t2=s.tasks[0].clone();t2.id=work::TaskId::new();t2.goal_id=g2.clone();t2.state=work::WorkState::PlanReview;t2.scope="src/two".into();s.install_plan(work::WorkPlan{workspace_id:s.workspace_id.clone(),goal_id:g2.clone(),tasks:vec![t2]}).unwrap();let second=wm::dispatch::ConfirmedPlan::confirm(&mut s,&g2,&c,&p).unwrap();let ledger=wm::dispatch::DeliveryLedger::default();
 assert!(wm::queue::ready_tasks(&s,&first,&c,&p,&ledger).unwrap().contains(&old_task));assert!(!wm::queue::ready_tasks(&s,&second,&c,&p,&ledger).unwrap().contains(&old_task));
 let req=wm::dispatch::DispatchRequest{task_id:old_task.clone(),operation_id:work::OperationId::new(),folder:wm::dispatch::DispatchFolder{path:c.folder.clone(),worktree_owner:None},persistent_binding:None,runtime_binding:work::RuntimeBinding{incarnation:work::RuntimeIncarnationId::new(),session_id:SessionId(1),execution_generation:1}};
 let err=wm::dispatch::prepare_dispatch(&s,&ledger,&second,req,&c,&p).unwrap_err();println!("SECOND_GOAL: still-ready original task excluded by replacement token: {}",err);
}
fn fifth_completed_project(){
 let (mut s,c,p,g)=fixture();let token=wm::dispatch::ConfirmedPlan::confirm(&mut s,&g,&c,&p).unwrap();
 let task=s.tasks[0].id.clone();let binding=work::RuntimeBinding{incarnation:work::RuntimeIncarnationId::new(),session_id:SessionId(18),execution_generation:1};let req=wm::dispatch::DispatchRequest{task_id:task.clone(),operation_id:work::OperationId::new(),folder:wm::dispatch::DispatchFolder{path:c.folder.clone(),worktree_owner:None},persistent_binding:None,runtime_binding:binding.clone()};let wm::dispatch::PrepareOutcome::Ready(prepared)=wm::dispatch::prepare_dispatch(&s,&Default::default(),&token,req,&c,&p).unwrap() else {panic!()};s=prepared.next_state;let attempt=prepared.assignment.attempt.id.clone();
 for (seq,kind) in [(1,work::WorkEventKind::ResultSubmitted{result:work::TaskResult{revision:work::ResultRevision(1),summary:"Actual manual result".into(),source_revision:None,artifact_revision:None,verification:work::VerificationState::NotRun}}),(2,work::WorkEventKind::VerificationFinished{result_revision:work::ResultRevision(1),outcome:work::VerificationState::Passed}),(3,work::WorkEventKind::UserConfirmed{result_revision:work::ResultRevision(1)})]{assert_eq!(s.apply_event(work::WorkEvent{id:work::EventId::new(),workspace_id:s.workspace_id.clone(),task_id:task.clone(),attempt_id:attempt.clone(),runtime_binding:binding.clone(),sequence:seq,kind}).unwrap(),wm::ApplyOutcome::Applied);}
 assert_eq!(s.goals[0].state,work::WorkState::Completed);snapshot(&s,&c).validate().unwrap();
 let mut host=CommandCenterHost{projects:BTreeMap::new(),inputs:BTreeMap::new(),publication_dirty:false};
 for i in 0..5 {host.projects.insert(format!("project-{i}"),Project{snapshot:snapshot(&s,&c),loaded:i<4,storage_blocked:false,generation:1,saved_generation:1,request:Some(()),proposal:None,confirmed:Some(token.clone()),scopes:vec![".".into()],allowed_agents:p.allowed_agents.clone(),ledger:Default::default(),revision:None});}
 let err=admit_project_load(&mut host,"project-4","project-0").unwrap_err();println!("FIFTH_COMPLETED_PROJECT: all four fully saved Completed projects cannot evict: {}",err);
 for p in host.projects.values_mut(){p.request=None;p.confirmed=None;}assert!(admit_project_load(&mut host,"project-4","project-0").is_ok());
}
mod delayed {
use super::*;
mod runtime {pub use deppy_core::SessionId;}
mod ui {pub mod composer{#[derive(Clone,Copy)]pub enum PromptAdmissionOutcome{Rejected}}}
#[derive(Clone,PartialEq)]pub struct FleetPromptTarget {workspace_id:String,session:SessionId,runtime_instance:u64}
#[derive(Clone)]struct Capture {prompt:Option<FleetPromptTarget>}
#[derive(Clone)]struct LiveRoute {capture:Capture}
struct Delayed {target:FleetPromptTarget,prompt:String,runtime_token:String,deadline:std::time::Instant}
struct Begin<'a>{host_input:&'a Input,live:&'a LiveRoute,id:&'a str,prompt:&'a str,runtime_token:&'a str,native:&'a str,config_revision:work::ConfigRevision,attempt_id:Option<work::AttemptId>}
#[derive(Default)]struct Observations{delayed:BTreeMap<String,Delayed>}
impl Observations{fn begin(&mut self,_:Begin<'_>)->Result<(),()> {Ok(())}}
struct Permit;impl Permit{fn is_current(&self)->bool {true}}
struct Host{observations:Observations,inputs:BTreeMap<String,Input>,projects:BTreeMap<String,Project>,remote_permits:BTreeMap<String,Permit>,status:String,live:LiveRoute}
impl Host{fn route(&self,_:&RouteTarget)->Option<&LiveRoute>{Some(&self.live)}}
fn reject_delayed(host:&mut Host,id:&str,_:&str){host.inputs.remove(id);}
enum PromptDeliveryOrigin{CommandCenter{automatic:bool,operation_id:String}}
struct App{command_center:Option<Box<Host>>,sent:Vec<String>,rejected:usize}
impl App{
 CALLBACK SEND
 fn command_center_input_outcome(&mut self,_:&str,_:ui::composer::PromptAdmissionOutcome){self.rejected+=1;}
 fn broadcast_prompt_to(&mut self,_:&FleetPromptTarget,prompt:&str,_:PromptDeliveryOrigin){self.sent.push(prompt.into());}
}
pub fn prove(){
 let(mut s,c,p,g)=fixture();let token=wm::dispatch::ConfirmedPlan::confirm(&mut s,&g,&c,&p).unwrap();let task=s.tasks[0].id.clone();let binding=work::RuntimeBinding{incarnation:work::RuntimeIncarnationId::new(),session_id:SessionId(7),execution_generation:1};let req=wm::dispatch::DispatchRequest{task_id:task.clone(),operation_id:work::OperationId::new(),folder:wm::dispatch::DispatchFolder{path:c.folder.clone(),worktree_owner:None},persistent_binding:None,runtime_binding:binding.clone()};let wm::dispatch::PrepareOutcome::Ready(prepared)=wm::dispatch::prepare_dispatch(&s,&Default::default(),&token,req,&c,&p).unwrap() else {panic!()};let assignment=prepared.assignment;let mut s=prepared.next_state;let attempt=assignment.attempt.id.clone();
 // ReportResult, VerifyResult and ConfirmResult all use the same reducer. No PTY input was admitted.
 for (seq,kind) in [(1,work::WorkEventKind::ResultSubmitted{result:work::TaskResult{revision:work::ResultRevision(1),summary:"User manual result".into(),source_revision:None,artifact_revision:None,verification:work::VerificationState::NotRun}}),(2,work::WorkEventKind::VerificationFinished{result_revision:work::ResultRevision(1),outcome:work::VerificationState::Passed}),(3,work::WorkEventKind::UserConfirmed{result_revision:work::ResultRevision(1)})]{assert_eq!(s.apply_event(work::WorkEvent{id:work::EventId::new(),workspace_id:s.workspace_id.clone(),task_id:task.clone(),attempt_id:attempt.clone(),runtime_binding:binding.clone(),sequence:seq,kind}).unwrap(),wm::ApplyOutcome::Applied);}
 assert_eq!(s.tasks[0].state,work::WorkState::Completed);
 let operation=assignment.attempt.operation_id.0.clone();let target=FleetPromptTarget{workspace_id:c.workspace_id.0.clone(),session:SessionId(7),runtime_instance:99};let route=RouteTarget{workspace_id:c.workspace_id.0.clone(),task_id:Some(task.0),session_id:Some("route-7".into())};let mut observations=Observations::default();observations.delayed.insert(operation.clone(),Delayed{target:target.clone(),prompt:assignment.prompt.clone(),runtime_token:"runtime-token".into(),deadline:std::time::Instant::now()+std::time::Duration::from_secs(20)});
 let project=Project{snapshot:snapshot(&s,&c),loaded:true,storage_blocked:false,generation:3,saved_generation:3,request:None,proposal:None,confirmed:Some(token),scopes:vec![".".into()],allowed_agents:p.allowed_agents,ledger:Default::default(),revision:None};let host=Host{observations,inputs:BTreeMap::from([(operation.clone(),Input{target:route,dispatch:Some(assignment)})]),projects:BTreeMap::from([(c.workspace_id.0.clone(),project)]),remote_permits:BTreeMap::new(),status:String::new(),live:LiveRoute{capture:Capture{prompt:Some(target)}}};let mut app=App{command_center:Some(Box::new(host)),sent:vec![],rejected:0};
 app.command_center_native_identity_result(operation,c.workspace_id.0.clone(),SessionId(7),99,"runtime-token".into(),None);
 assert_eq!(app.sent.len(),1);assert_eq!(app.rejected,0);assert_eq!(app.command_center.as_ref().unwrap().projects[&c.workspace_id.0].snapshot.tasks[0].state,work::WorkState::Completed);println!("DELAYED_RETIRED_INPUT: exact production callback and send gate call broadcast_prompt_to for already Completed attempt");
}
}
fn main(){confirmation_restart_and_second_goal();fifth_completed_project();delayed::prove();}
'''
code=code.replace('MODULES',modules).replace('ADMIT',admit).replace('RECOVER',recover).replace('CALLBACK',callback).replace('SEND',send)
out=Path('/private/tmp/deppy-cc-review-workflow-20261020.rs');out.write_text(code)
print(out)
