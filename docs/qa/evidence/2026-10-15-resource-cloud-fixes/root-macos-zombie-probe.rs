fn main() {
    use std::os::unix::process::CommandExt as _;
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "sleep 0.1; exit 0"]);
    unsafe { command.pre_exec(|| {
        if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    }); }
    let mut child = command.spawn().unwrap();
    let pid = child.id() as i32;
    println!("libc CLD_EXITED={} CLD_KILLED={} CLD_DUMPED={} CLD_STOPPED={} CLD_CONTINUED={}", libc::CLD_EXITED, libc::CLD_KILLED, libc::CLD_DUMPED, libc::CLD_STOPPED, libc::CLD_CONTINUED);
    let mut saw_exited_uninspectable = false;
    for step in 0..20 {
        if step == 1 { assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0); }
        if step == 3 { assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0); }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let result = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
        let error = std::io::Error::last_os_error();
        let sid = unsafe { libc::getsid(pid) };
        let sid_error = std::io::Error::last_os_error();
        let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let peek = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, status.as_mut_ptr(), libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        assert_eq!(peek, 0);
        let status = unsafe { status.assume_init() };
        let exited_pid = unsafe { status.si_pid() };
        assert!(exited_pid == 0 || exited_pid == pid);
        println!("waitid WNOWAIT step={step} result={peek} returned_pid={exited_pid} si_code={} si_status={}", status.si_code, unsafe { status.si_status() });
        saw_exited_uninspectable |= exited_pid == pid && result != size && sid < 0;
        if result == size {
            let info = unsafe { info.assume_init() };
            println!("pid={pid} result={result} status={} birth=({}, {}) pgid={} getsid={sid} sid_error={sid_error}", info.pbi_status, info.pbi_start_tvsec, info.pbi_start_tvusec, info.pbi_pgid);
        } else { println!("pid={pid} result={result}/{size} error={error} getsid={sid} sid_error={sid_error}"); }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(saw_exited_uninspectable, "probe must qualify the Darwin exited/unreaped API gap");
    println!("owned child wait={:?}", child.wait());
    let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let peek = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, status.as_mut_ptr(), libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    let error = std::io::Error::last_os_error();
    println!("after reap waitid={peek} error={error}");
    assert_eq!(peek, -1);
    assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
}
