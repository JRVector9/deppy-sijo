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
    pub fn parse(text:&str)->Option<crate::app::ProviderUsage>{parse_usage(text)}
}
mod grok_usage {
    include!("/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/docs/qa/assets/provider-usage-review-20261023/grok-usage-instrumented.rs");
    pub fn probe(path:&std::path::Path)->anyhow::Result<Option<GrokUsage>>{fetch_grok_usage(path,None)}
    pub fn parse(text:&str)->Option<GrokUsage>{parse_usage(text)}
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
    if args[1]=="claude" {println!("provider=claude result={:?} elapsed_ms={}",claude_usage::probe(path),start.elapsed().as_millis());}
    else {println!("provider=grok result={:?} elapsed_ms={}",grok_usage::probe(path),start.elapsed().as_millis());}
}

fn observe_usage(clean:&str,byte_count:usize){
    let lower=clean.to_lowercase();
    let phrases=["trust this folder","trust","do you trust","directory","welcome","usage","/usage","weekly limit","monthly limit","credits","billing","loading","not authenticated","sign in","log in","update","press enter","terminal","color","theme","oauth","error","failed","connecting","ready","1;2R","0;0R","6n"];
    let counts:std::collections::BTreeMap<_,_>=phrases.into_iter().map(|s|(s,lower.matches(s).count())).collect();
    eprintln!("SAFE_DIAGNOSTIC bytes={byte_count} text_chars={} phrase_counts={counts:?}",clean.len());
    let line_kinds:Vec<_>=clean.lines().map(|line| {
        let lower=line.to_lowercase();
        if lower.contains("usage") || lower.contains("trust") || lower.contains("weekly") || lower.contains("monthly") || lower.contains("billing") || lower.contains("error") || lower.contains("terminal") || lower.contains("update") {
            lower.chars().filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace() || "?/:.%$-_()'".contains(*c)).collect::<String>()
        } else {String::new()}
    }).filter(|s|!s.is_empty()).map(|s|{
        let email=regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap();
        let tokens=regex::Regex::new(r"[A-Za-z0-9_=-]{24,}").unwrap();
        tokens.replace_all(&email.replace_all(&s,"[EMAIL]"),"[LONG_TOKEN]").to_string()
    }).take(18).collect();
    eprintln!("SAFE_LABEL_LINES={line_kinds:?}");
}

fn observe_raw(bytes:&[u8]) {
    let text=String::from_utf8_lossy(bytes);
    let osc=regex::Regex::new(r"\x1b\][^\x07]*?(?:\x07|\x1b\\)").unwrap();
    let csi=regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    let fixed=csi.replace_all(&osc.replace_all(&text,""),"").into_owned();
    eprintln!("RAW_CONTROL_COUNTS osc={} st={} bel={} raw_ascii_usage={} raw_weekly={} raw_slash_usage={} shortest_strip_chars={}",text.matches("\x1b]").count(),text.matches("\x1b\\").count(),text.matches('\x07').count(),text.to_lowercase().contains("usage"),text.to_lowercase().contains("weekly"),text.contains("/usage"),fixed.len());
    eprintln!("SHORTEST_STRIP_PARSE={:?}",grok_usage::parse(&fixed));
    observe_usage(&fixed,bytes.len());
}
