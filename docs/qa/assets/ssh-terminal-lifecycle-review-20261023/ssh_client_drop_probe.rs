use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration,Instant};
use std::net::Shutdown;
use runtime::{SshRuntimeClient,SshClientHello,SshServerConfig,SshTargetIdentity,SshServerHello,SshHandshakeError};
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Counts { threads:i32, fds:usize }
fn counts() -> Counts {
    unsafe {
        let mut task:libc::proc_taskinfo=std::mem::zeroed();
        let size=std::mem::size_of_val(&task) as i32;
        assert_eq!(libc::proc_pidinfo(std::process::id() as i32,libc::PROC_PIDTASKINFO,0,(&mut task as *mut libc::proc_taskinfo).cast(),size),size);
        let needed=libc::proc_pidinfo(std::process::id() as i32,libc::PROC_PIDLISTFDS,0,std::ptr::null_mut(),0);
        assert!(needed>0);
        let mut descriptors=vec![0u8;needed as usize+1024];
        let actual=libc::proc_pidinfo(std::process::id() as i32,libc::PROC_PIDLISTFDS,0,descriptors.as_mut_ptr().cast(),descriptors.len() as i32);
        assert!(actual>0 && actual < descriptors.len() as i32);
        Counts {threads:task.pti_threadnum,fds:actual as usize/std::mem::size_of::<libc::proc_fdinfo>()}
    }
}
fn identity()->SshTargetIdentity { SshTargetIdentity {remote_workspace_id:"lifecycle-fixture".into(),account:"fixture".into(),canonical_root:"/tmp".into(),host_key_sha256:"SHA256:fixture-owned-socket".into()} }
fn main() {
    let baseline=counts();
    println!("BASELINE threads={} fds={}",baseline.threads,baseline.fds);
    let mut peak=baseline;
    let mut max_settle=Duration::ZERO;
    for iteration in 0..50 {
        let (socket,mut peer)=UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let peer_thread=std::thread::spawn(move || {
            let bytes=runtime::read_ssh_frame(&mut peer,64*1024).unwrap().unwrap();
            let request: SshClientHello=postcard::from_bytes(&bytes).unwrap();
            let config=SshServerConfig::new(request.identity.clone(),"fixture-runtime".into());
            let hello:Result<SshServerHello,SshHandshakeError>=Ok(config.negotiate(&request).unwrap());
            runtime::write_ssh_frame(&mut peer,&postcard::to_stdvec(&hello).unwrap(),64*1024).unwrap();
            loop { match runtime::read_ssh_frame(&mut peer,16*1024*1024) { Ok(Some(_))=>{}, Ok(None)=>break, Err(_)=>break } }
        });
        let close=socket.try_clone().unwrap();
        let client=SshRuntimeClient::connect(socket.try_clone().unwrap(),socket,SshClientHello::new(identity(),iteration+1),Arc::new(move ||{let _=close.shutdown(Shutdown::Both);})).unwrap();
        if iteration>=25 { client.send_rpc(100_000+iteration,vec![42;128*1024]).unwrap(); }
        let active=counts();
        peak.threads=peak.threads.max(active.threads); peak.fds=peak.fds.max(active.fds);
        assert!(active.threads >= baseline.threads+3,"three actual runtime workers must exist");
        let started=Instant::now();
        drop(client); // Exercise production nonblocking Drop; never call its explicit shutdown/join.
        peer_thread.join().unwrap();
        loop {
            let current=counts();
            if current==baseline {break;}
            assert!(started.elapsed()<Duration::from_secs(3),"iteration={} leaked: baseline={:?} current={:?}",iteration+1,baseline,current);
            std::thread::sleep(Duration::from_millis(2));
        }
        max_settle=max_settle.max(started.elapsed());
        if (iteration+1)%10==0 {println!("SETTLED iteration={} threads={} fds={}",iteration+1,counts().threads,counts().fds);}
    }
    println!("PASS iterations=50 idle=25 pending_rpc=25 baseline={:?} final={:?} peak={:?} max_settle_ms={}",baseline,counts(),peak,max_settle.as_millis());
}
