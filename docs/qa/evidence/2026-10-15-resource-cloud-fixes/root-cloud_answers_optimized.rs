pub struct SecretString(
    String,
    #[cfg(test)] Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(
            value,
            #[cfg(test)]
            None,
        )
    }

    /// 평문 접근. keyring 저장/env 주입 등 명시적 사용처에서만 호출한다.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consumes the zeroizing wrapper and transfers its plaintext `String` allocation without a
    /// copy.
    ///
    /// The caller assumes responsibility for the plaintext's lifetime after this call. Keep it as
    /// short as possible and move it promptly into the next zeroizing owner; an ordinary `String`
    /// does not overwrite its allocation when dropped. The consumed wrapper drops with an empty
    /// buffer and therefore cannot erase the transferred allocation.
    pub fn into_string(mut self) -> String {
        std::mem::take(&mut self.0)
    }

    #[cfg(test)]
    fn observe_zeroized_drop(&mut self, observer: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.1 = Some(observer);
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the String and only overwrites existing bytes.
        for byte in unsafe { self.0.as_mut_vec() } {
            // SAFETY: `byte` is an exclusively borrowed byte in the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if let Some(observer) = &self.1 {
            observer.store(
                self.0.as_bytes().iter().all(|byte| *byte == 0),
                std::sync::atomic::Ordering::Release,
            );
        }
    }
}


#[path="/private/tmp/deppy-cloud-fix-root-20261015/redaction-production.rs"]
mod redaction;
mod secret { pub use crate::SecretString; pub use crate::redaction::RedactionService; }
use std::sync::Arc;
use std::time::Instant;
use std::hint::black_box;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
struct CountAllocator;
unsafe impl GlobalAlloc for CountAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 { if TRACK.load(Ordering::Relaxed) {ALLOCS.fetch_add(1,Ordering::Relaxed); BYTES.fetch_add(layout.size(),Ordering::Relaxed);} unsafe {System.alloc(layout)} }
    unsafe fn dealloc(&self,p:*mut u8,layout:Layout) {unsafe{System.dealloc(p,layout)}}
    unsafe fn realloc(&self,p:*mut u8,old:Layout,new:usize)->*mut u8 { if TRACK.load(Ordering::Relaxed){ALLOCS.fetch_add(1,Ordering::Relaxed);BYTES.fetch_add(new,Ordering::Relaxed);}unsafe{System.realloc(p,old,new)}}
}
#[global_allocator]static ALLOCATOR:CountAllocator=CountAllocator;
mod ui { pub mod cloud_answer {
//! Cloud replies belong to their persistent session, never its PTY input.
#[derive(Clone)]
pub struct Answer {
    pub id: String,
    pub session: String,
    pub connection: String,
    pub run: Option<String>,
    pub run_label: Option<String>,
    pub created: i64,
    pub message: String,
}
}}

fn truncate_utf8(text:&mut String,max:usize){ if text.len()>max {let mut end=max;while !text.is_char_boundary(end){end-=1;}text.truncate(end);} }
struct AnswerProjection {
    source: std::sync::Arc<[crate::ui::cloud_answer::Answer]>,
    corpus_revision: u64,
    answers: std::sync::Arc<[crate::ui::cloud_answer::Answer]>,
}

struct CloudAgent {answers:Arc<[ui::cloud_answer::Answer]>, answer_projection:std::cell::RefCell<Option<AnswerProjection>>,redaction:secret::RedactionService}
impl CloudAgent {
    pub fn cloud_answers(&self) -> std::sync::Arc<[crate::ui::cloud_answer::Answer]> {
        // One corpus lock makes both the cache check and a rebuilt projection coherent with
        // credential registration, replacement, fail-closed changes and grace expiration.
        let redaction = self.redaction.snapshot();
        let mut cache = self.answer_projection.borrow_mut();
        if let Some(projection) = cache.as_ref()
            && std::sync::Arc::ptr_eq(&projection.source, &self.answers)
            && projection.corpus_revision == redaction.revision()
        {
            return std::sync::Arc::clone(&projection.answers);
        }
        let answers: std::sync::Arc<[crate::ui::cloud_answer::Answer]> = self
            .answers
            .iter()
            .cloned()
            .map(|mut answer| {
                answer.message = redaction.redact_wrapped_text(&answer.message);
                answer.connection = redaction.redact_wrapped_text(&answer.connection);
                answer.run_label = answer.run.as_deref().map(|run| {
                    let mut label = redaction.redact_wrapped_text(run);
                    truncate_utf8(&mut label, 512);
                    label
                });
                answer
            })
            .collect::<Vec<_>>()
            .into();
        *cache = Some(AnswerProjection {
            source: std::sync::Arc::clone(&self.answers),
            corpus_revision: redaction.revision(),
            answers: std::sync::Arc::clone(&answers),
        });
        answers
    }
}
fn fixture(rows:usize,size:usize,keys:usize)->CloudAgent {
    let redaction=secret::RedactionService::new();
    for i in 0..keys {redaction.register_permanent(&secret::SecretString::new(format!("synthetic-fixture-token-{i:04}-ABC123"))).unwrap();}
    let answers=(0..rows).map(|i|ui::cloud_answer::Answer{id:format!("id-{i}"),session:"session-id".into(),connection:"Synthetic connection".into(),run:Some("run-id".into()),run_label:Some("run-id".into()),created:1,message:("ordinary paragraph text\n".repeat((size+23)/24))[..size].into()}).collect::<Vec<_>>().into();
    CloudAgent{answers,redaction,answer_projection:std::cell::RefCell::new(None)}
}
fn contracts(){
 let mut fixture=fixture(1,1024,0);
 fixture.answers=vec![ui::cloud_answer::Answer{id:"routing-id".into(),session:"routing-session".into(),connection:"synthetic-secret-token".into(),run:Some("synthetic-secret-token".into()),run_label:Some("old-label".into()),created:37,message:"prefix synthetic-secret-\ntoken suffix".into()}].into();
 assert_eq!(fixture.cloud_answers()[0].message,"prefix synthetic-secret-\ntoken suffix");
 fixture.redaction.register_permanent(&secret::SecretString::new("synthetic-secret-token".into())).unwrap();
 let answers=fixture.cloud_answers();
 assert_eq!(answers[0].message,"prefix [REDACTED] suffix");assert_eq!(answers[0].connection,"[REDACTED]");assert_eq!(answers[0].run_label.as_deref(),Some("[REDACTED]"));
 assert_eq!(answers[0].id,"routing-id");assert_eq!(answers[0].session,"routing-session");assert_eq!(answers[0].run.as_deref(),Some("synthetic-secret-token"));assert_eq!(answers[0].created,37);
 println!("contract=PASS late-registration/wrapped-secret/routing-identity");
}
fn main(){
 contracts();
 println!("scope=exact modified cloud_answers+AnswerProjection production bodies; synthetic fixtures; per-call time incl allocations and Arc publication, excluding fixture setup/output drop; source-exact redaction module optimized via rustc -O; serde_json debug dependency is not exercised");
 for (rows,size,keys) in [(0,0,32),(1,1024,1),(10,1024,32),(100,16384,1),(100,16384,32),(100,16384,128)] {
   let fixture=fixture(rows,size,keys);
   let cold_start=Instant::now(); let expected=fixture.cloud_answers(); println!("cold_projection_ms={:.3}",cold_start.elapsed().as_secs_f64()*1000.);
   assert_eq!(expected.len(),rows);for (old,new) in fixture.answers.iter().zip(expected.iter()){assert_eq!(old.message,new.message);assert_eq!(old.id,new.id);assert_eq!(old.session,new.session);}
   println!("fixture rows={rows} message_bytes={size} raw_keys={keys} corpus={:?}",fixture.redaction.corpus_stats());
   let mut ms=Vec::new();let mut ac=Vec::new();let mut by=Vec::new();
   for sample in 0..10 {
     ALLOCS.store(0,Ordering::Relaxed);BYTES.store(0,Ordering::Relaxed);TRACK.store(true,Ordering::Relaxed);
     let now=Instant::now();let answers=black_box(fixture.cloud_answers());let elapsed=now.elapsed().as_secs_f64()*1000.;
     TRACK.store(false,Ordering::Relaxed);ms.push(elapsed);ac.push(ALLOCS.load(Ordering::Relaxed));by.push(BYTES.load(Ordering::Relaxed));
     assert_eq!(answers.len(),rows);assert!(Arc::ptr_eq(&expected,&answers),"steady corpus/history must reuse projection");println!("sample={sample} ms={elapsed:.3} allocs={} cumulative_alloc_bytes={}",ac[sample],by[sample]);drop(answers);
   }
   ms.sort_by(f64::total_cmp);println!("summary rows={rows} message_bytes={size} raw_keys={keys} mean_ms={:.3} median_ms={:.3} min_ms={:.3} max_ms={:.3} allocs_per_call={} cumulative_alloc_bytes_per_call={}",ms.iter().sum::<f64>()/10.,(ms[4]+ms[5])/2.,ms[0],ms[9],ac[0],by[0]);
 }
}
