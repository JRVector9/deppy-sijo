#!/usr/bin/env python3
"""Isolated release probe of the exact production screen_text function and backend."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
RUST = r'''
use std::{alloc::{GlobalAlloc, Layout, System}, cell::Cell, hash::{Hash, Hasher}, hint::black_box, time::Instant};
use terminal::TerminalBackend;
thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}
fn bump(n: usize) {
    if !ENABLED.try_with(Cell::get).unwrap_or(false) { return; }
    let _ = COUNT.try_with(|v| v.set(v.get()+1));
    let _ = BYTES.try_with(|v| v.set(v.get()+n as u64));
}
struct Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self,l:Layout)->*mut u8 { bump(l.size()); unsafe { System.alloc(l) } }
    unsafe fn alloc_zeroed(&self,l:Layout)->*mut u8 { bump(l.size()); unsafe { System.alloc_zeroed(l) } }
    unsafe fn realloc(&self,p:*mut u8,l:Layout,n:usize)->*mut u8 { bump(n); unsafe { System.realloc(p,l,n) } }
    unsafe fn dealloc(&self,p:*mut u8,l:Layout) { unsafe { System.dealloc(p,l) } }
}
#[global_allocator] static ALLOC: Counter = Counter;
fn stats()->(u64,u64) { (COUNT.with(Cell::get),BYTES.with(Cell::get)) }
const MAX_SCREEN:usize = 64*1024;
// PRODUCTION_FUNCTION
// BASELINE_FUNCTION
fn main() {
    for (name,unit,width) in [("blank"," ",1),("ascii","x",1),("cjk","한",2),("grapheme","a\u{301}\u{308}",1)] {
        let mut backend=terminal::AlacrittyBackend::new(300,80,100);
        let row=unit.repeat(300/width);
        for _ in 0..80 { backend.feed(row.as_bytes()).unwrap(); backend.feed(b"\r\n").unwrap(); }
        let snapshot=backend.viewport_snapshot().unwrap();
        black_box(screen_text(&snapshot));
        let before=stats(); ENABLED.with(|v|v.set(true));
        let output=screen_text(&snapshot); let after=stats(); ENABLED.with(|v|v.set(false));
        assert_eq!(output, baseline_screen_text(&snapshot), "fixture output changed: {name}");
        let mut hash=std::collections::hash_map::DefaultHasher::new(); output.hash(&mut hash);
        let begin=Instant::now();
        for _ in 0..300 { black_box(screen_text(black_box(&snapshot))); }
        println!("{{\"fixture\":\"{}\",\"allocations\":{},\"requested_bytes\":{},\"output_bytes\":{},\"output_hash\":\"{:016x}\",\"mean_us\":{:.3}}}",name,after.0-before.0,after.1-before.1,output.len(),hash.finish(),begin.elapsed().as_secs_f64()*1e6/300.0);
    }
}
'''

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--assert-alloc-limit', action='store_true')
    parser.add_argument('--source-ref', help='measure this committed source instead of the working function')
    parser.add_argument('--baseline-ref', default='c2be1e79', help='exact output oracle before these changes')
    args = parser.parse_args()
    source = subprocess.check_output(['git','show',f'{args.source_ref}:crates/app/src/cloud_agent.rs'],cwd=ROOT,text=True) if args.source_ref else (ROOT/'crates/app/src/cloud_agent.rs').read_text()
    begin = source.index('pub fn screen_text(')
    end = source.index('\n#[cfg(test)]', begin)
    function = source[begin:end]
    baseline = subprocess.check_output(['git','show',f'{args.baseline_ref}:crates/app/src/cloud_agent.rs'],cwd=ROOT,text=True)
    i = baseline.index('pub fn screen_text(')
    baseline = baseline[i:baseline.index('\n#[cfg(test)]',i)].replace('pub fn screen_text(', 'fn baseline_screen_text(',1)
    subprocess.run(['cargo','build','-p','terminal','--release','--target-dir',str(ROOT/'target')],cwd=ROOT,check=True)
    library = ROOT/'target/release/libterminal.rlib'
    with tempfile.TemporaryDirectory(prefix='deppy-screen-probe-') as temp:
        temp = Path(temp)
        script = temp/'probe.rs'
        binary = temp/'probe'
        script.write_text(RUST.replace('// PRODUCTION_FUNCTION',function).replace('// BASELINE_FUNCTION',baseline))
        subprocess.run(['rustc','--edition=2024','-O',str(script),'-L',f'dependency={ROOT}/target/release/deps','--extern',f'terminal={library}','-o',str(binary)],check=True)
        runs = []
        for _ in range(3):
            text = subprocess.check_output([str(binary)],text=True)
            runs.append([json.loads(line) for line in text.splitlines()])
        result = {'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
                  'source_ref':args.source_ref or 'working-tree','baseline_ref':args.baseline_ref,
                  'function_sha256':hashlib.sha256(function.encode()).hexdigest(),
                  'terminal_rlib_sha256':hashlib.sha256(library.read_bytes()).hexdigest(),
                  'fixture':'real Alacritty backend 300x80; isolated exact screen_text; System requested allocations; release -O; counters disabled during timing; exact baseline output oracle',
                  'runs':runs}
        args.output.write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps(runs[0],indent=2))
        if args.assert_alloc_limit:
            assert all(row['allocations'] <= 20 for run in runs for row in run), 'screen conversion must not allocate a new buffer for every row'

if __name__ == '__main__':
    main()
