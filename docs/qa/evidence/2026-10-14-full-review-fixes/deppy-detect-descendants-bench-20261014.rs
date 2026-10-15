
use std::collections::{HashMap,HashSet,VecDeque};
const MAX_PROCESS_ROWS:usize=16_384;
const MAX_DESCENDANTS_PER_SESSION:usize=1_024;
struct ProcRow { pid:u32, ppid:Option<u32> }
fn descendant_pids(root_pid: u32, rows: &[ProcRow]) -> std::collections::HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in rows.iter().take(MAX_PROCESS_ROWS) {
        if let Some(parent) = row.ppid {
            children.entry(parent).or_default().push(row.pid);
        }
    }
    let mut wanted = HashSet::from([root_pid]);
    let mut pending = VecDeque::from([root_pid]);
    while let Some(parent) = pending.pop_front() {
        if let Some(child_pids) = children.get(&parent) {
            for &pid in child_pids {
                if wanted.len() >= MAX_DESCENDANTS_PER_SESSION {
                    return HashSet::new();
                }
                if wanted.insert(pid) {
                    pending.push_back(pid);
                }
            }
        }
    }
    wanted
}

fn main() {
    for (sessions,processes) in [(32usize,2048usize),(128,8192),(256,16384)] {
        let rows:Vec<_>=(0..processes).map(|index| ProcRow {
            pid:1000+index as u32,
            ppid:Some(if index<sessions {1} else {1000+(index%sessions) as u32}),
        }).collect();
        let started=std::time::Instant::now();
        let mut found=0usize;
        for _ in 0..10 {
            for _tier in 0..3 {
                for session in 0..sessions {
                    found+=std::hint::black_box(descendant_pids(1000+session as u32,std::hint::black_box(&rows))).len();
                }
            }
        }
        println!("source-extracted descendant_pids only: sessions={sessions} ps_rows={processes} calls_per_full_miss_pass={} mean_per_pass_ms={:.3} found={found}",sessions*3,started.elapsed().as_secs_f64()*1000.0/10.0);
    }
}
