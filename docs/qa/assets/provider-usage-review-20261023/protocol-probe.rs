#![allow(dead_code)]
mod app {
    pub(crate) type ProviderUsage=(Option<u8>,Option<u8>);
    pub(crate) fn fresh_usage_after(v:ProviderUsage,_age:std::time::Duration)->Option<ProviderUsage>{Some(v)}
}
mod paths {pub(crate) fn home_dir()->Option<std::path::PathBuf>{ std::env::var_os("HOME").map(Into::into)}}
mod agent_launcher {
    pub(crate) struct DetectedAgent { pub path:std::path::PathBuf, pub launch:Option<String> }
    impl DetectedAgent {
        pub fn executable(&self)->&std::path::Path{&self.path}
        pub fn launch_path(&self)->Option<&str>{self.launch.as_deref()}
    }
}
#[path="/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/crates/app/src/provider_usage_command.rs"] mod provider_usage_command;
mod claude_usage {
    include!("/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/crates/app/src/claude_usage.rs");
    pub fn probe(path:&std::path::Path)->anyhow::Result<Option<crate::app::ProviderUsage>>{fetch_claude_usage(path,None)}
    pub fn parse(text:&str)->Option<crate::app::ProviderUsage>{parse_usage(&strip_terminal_control_sequences(text))}
}
mod grok_usage {
    include!("/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/crates/app/src/grok_usage.rs");
    pub fn probe(path:&std::path::Path)->anyhow::Result<Option<GrokUsage>>{fetch_grok_usage(path,None)}
    pub fn parse(text:&str)->Option<GrokUsage>{parse_usage(text)}
    pub fn short_probe(path:&std::path::Path)->anyhow::Result<Option<GrokUsage>>{
        let dir=std::env::temp_dir();
        let command=grok_probe_command(path,dir,None,std::env::var_os("PATH").as_deref())?;
        let mut session=pty::PortablePtyBackend.spawn(&command,120,40)?;
        let result=run_grok_usage_probe(session.as_mut(),Instant::now()+Duration::from_secs(6));
        session.kill()?;
        result
    }

}
fn main(){
    let args:Vec<_>=std::env::args().collect();
    let start=std::time::Instant::now();
    if args[1]=="parse" {
        let text=std::io::read_to_string(std::io::stdin()).unwrap();
        if args[2]=="claude" { println!("{:?}",claude_usage::parse(&text)); }
        else {println!("{:?}",grok_usage::parse(&text));}
        return;
    }
    let path=std::path::Path::new(&args[2]);
    if args[1]=="grok-short"{ println!("{:?}",grok_usage::short_probe(path));return;}
    if args[1]=="claude" {println!("provider=claude result={:?} elapsed_ms={}",claude_usage::probe(path),start.elapsed().as_millis());}
    else {println!("provider=grok result={:?} elapsed_ms={}",grok_usage::probe(path),start.elapsed().as_millis());}
}
