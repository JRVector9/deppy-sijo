use pty::{PortablePtyBackend, PtyBackend, CommandSpec};
use std::{path::PathBuf,time::{Instant,Duration}};
#[derive(Clone,Copy,Debug,PartialEq)]struct Identity {pid:i32,group:i32,session:i32,birth:(u64,u64),zombie:bool}
fn inspect(pid:i32)->Option<Identity>{
 let mut info=std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();let size=std::mem::size_of::<libc::proc_bsdinfo>() as i32;
 if unsafe{libc::proc_pidinfo(pid,libc::PROC_PIDTBSDINFO,0,info.as_mut_ptr().cast(),size)} != size {return None;}
 let info=unsafe{info.assume_init()};let session=unsafe{libc::getsid(pid)};if info.pbi_pid!=pid as u32 || session<=0{return None;}
 Some(Identity{pid,group:info.pbi_pgid as i32,session,birth:(info.pbi_start_tvsec,info.pbi_start_tvusec),zombie:info.pbi_status==libc::SZOMB})
}
struct Cleanup(Identity);
impl Drop for Cleanup{fn drop(&mut self){if inspect(self.0.pid).is_some_and(|current|current.pid==self.0.pid && current.group==self.0.group && current.session==self.0.session && current.birth==self.0.birth && !current.zombie){unsafe{libc::kill(self.0.pid,libc::SIGKILL)};}}}
fn run(kind:&str,action:&str,freeze:bool)->bool{
 let prefix=PathBuf::from("/private/tmp/deppy-cloud-review-perf-20261015");
 let run=prefix.join(format!("run-{}-{}-{}-{}",std::process::id(),kind,action,freeze));std::fs::create_dir(&run).unwrap();
 let pid_path=run.join("pid");
 let spec=CommandSpec{program:"/bin/bash".into(),args:vec!["--noprofile".into(),"--norc".into(),"-i".into()],env:vec![("HISTFILE".into(),"/dev/null".into()),("PS1".into(),"FIXTURE> ".into()),("BASH_ENV".into(),"/dev/null".into())],cwd:Some(run.clone())};
 let mut session=PortablePtyBackend.spawn(&spec,80,24).unwrap();let shell=session.process_identity();
 let output=session.take_output().unwrap();
 let command=format!("/usr/bin/python3 {}/owned-workload.py {} {}\n",prefix.display(),pid_path.display(),kind);
 assert!(session.write_input(command.as_bytes()).unwrap().is_accepted());
 let deadline=Instant::now()+Duration::from_secs(5);let fields=loop{if let Ok(text)=std::fs::read_to_string(&pid_path){let values:Vec<i32>=text.split_whitespace().filter_map(|s|s.parse().ok()).collect();if values.len()==3{break values;}}while output.try_recv().is_ok(){}assert!(Instant::now()<deadline,"fixture ready deadline");std::thread::sleep(Duration::from_millis(5));};
 let identity=inspect(fields[0]).unwrap();let cleanup=Cleanup(identity);assert_eq!(identity.group,fields[1]);assert_eq!(identity.session,fields[2]);
 assert_eq!(session.foreground_process_group(),Some(identity.group as u32));assert_ne!(shell.process_group,Some(identity.group as u32));assert!(!identity.zombie);
 if freeze{session.freeze().unwrap();}
 println!("before kind={kind} action={action} freeze={freeze} shell={shell:?} workload={identity:?} actual_foreground={:?}",session.foreground_process_group());
 let started=Instant::now();if action=="kill"{session.kill().unwrap();}drop(session);drop(output);let elapsed=started.elapsed();
 std::thread::sleep(Duration::from_millis(300));let after=inspect(identity.pid);
 let survived=after.is_some_and(|current|current.birth==identity.birth && current.group==identity.group && current.session==identity.session && !current.zombie);
 println!("after kind={kind} action={action} freeze={freeze} teardown_ms={:.3} same_live_workload_survives={survived} identity={after:?}",elapsed.as_secs_f64()*1000.);
 drop(cleanup);let deadline=Instant::now()+Duration::from_secs(2);while inspect(identity.pid).is_some_and(|p|p.birth==identity.birth && !p.zombie){assert!(Instant::now()<deadline,"owned fixture cleanup failed");std::thread::sleep(Duration::from_millis(5));}println!("cleanup kind={kind} action={action} freeze={freeze} PASS");survived
}
fn main(){let normal=run("normal","drop",false);let drop_survives=run("ignore-hup","drop",false);let kill_survives=run("ignore-hup","kill",false);let frozen=run("ignore-hup","drop",true);assert!(!normal,"control normal foreground must terminate");assert!(!frozen,"frozen group current regression contract");assert!(!drop_survives && !kill_survives,"non-frozen foreground escaped pane/session close");}
