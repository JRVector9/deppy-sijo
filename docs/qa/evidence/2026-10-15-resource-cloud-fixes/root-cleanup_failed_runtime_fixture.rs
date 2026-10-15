use std::{path::PathBuf,time::{Instant,Duration}};
#[derive(Clone,Copy,Debug,PartialEq)]struct Identity {pid:i32,group:i32,session:i32,birth:(u64,u64),zombie:bool}
fn inspect(pid:i32)->Option<Identity>{
 let mut info=std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();let size=std::mem::size_of::<libc::proc_bsdinfo>() as i32;
 if unsafe{libc::proc_pidinfo(pid,libc::PROC_PIDTBSDINFO,0,info.as_mut_ptr().cast(),size)} != size {return None;}
 let info=unsafe{info.assume_init()};let session=unsafe{libc::getsid(pid)};if info.pbi_pid!=pid as u32 || session<=0{return None;}
 Some(Identity{pid,group:info.pbi_pgid as i32,session,birth:(info.pbi_start_tvsec,info.pbi_start_tvusec),zombie:info.pbi_status==libc::SZOMB})
}

fn main() {
 let pid:i32=std::env::args().nth(1).unwrap().parse().unwrap();
 let directory=PathBuf::from(std::env::args().nth(2).unwrap());
 assert!(directory.file_name().unwrap().to_str().unwrap().starts_with("deppy-rt-logs-pr13-exit-teardown-"));
 assert!(directory.join("descendant-ready").is_file());
 let Some(owner)=inspect(pid) else { println!("already gone pid={pid}"); return; };
 let listed=std::process::Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "command="]).output().unwrap();
 let command=String::from_utf8(listed.stdout).unwrap();
 assert!(command.trim().starts_with("/bin/sh -c (trap '' HUP; trap"));
 assert!(command.contains(directory.to_str().unwrap()));
 let same=|p:Identity|p.birth==owner.birth && p.group==owner.group && p.session==owner.session && !p.zombie;
 println!("qualified owned failed-fixture pid={pid} identity={owner:?} path={directory:?}");
 std::fs::write(directory.join("permit-revoked"), b"root-failed-fixture-cleanup").unwrap();
 if inspect(pid).is_some_and(same) { assert_eq!(unsafe{libc::kill(pid,libc::SIGTERM)},0); }
 let start=Instant::now();
 while inspect(pid).is_some_and(same) && start.elapsed()<Duration::from_secs(2) { std::thread::sleep(Duration::from_millis(5)); }
 if inspect(pid).is_some_and(same) { assert_eq!(unsafe{libc::kill(pid,libc::SIGKILL)},0); }
 let start=Instant::now();
 while inspect(pid).is_some_and(same) && start.elapsed()<Duration::from_secs(2) { std::thread::sleep(Duration::from_millis(5)); }
 assert!(!inspect(pid).is_some_and(same));
 println!("cleanup PASS, only pinned execution signalled; acknowledged={}",directory.join("descendant-acknowledged").exists());
}
