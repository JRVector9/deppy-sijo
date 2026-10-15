use std::io::Write;
type ProviderUsage = (Option<u8>, Option<u8>);
const FIVE_HOUR_WINDOW_MINUTES: f64 = 300.0;
const WEEKLY_WINDOW_MINUTES: f64 = 10_080.0;
const USAGE_STALE_FRACTION_OF_WINDOW: f64 = 1.0;
mod paths {
 pub static REVIEW_HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
 pub fn home_dir() -> Option<std::path::PathBuf> { REVIEW_HOME.get().cloned() }
}
fn fresh_usage_windows(usage: ProviderUsage, age_minutes: f64) -> ProviderUsage {
    // age가 음수(시계 되감김)면 낡음으로 보지 않는다. NaN이면 어느 비교도 참이
    // 아니라 양쪽 다 비는데, 그때는 나이를 모르는 것이므로 버리는 쪽이 맞다.
    let fresh =
        |window_minutes: f64| age_minutes <= window_minutes * USAGE_STALE_FRACTION_OF_WINDOW;
    (
        usage.0.filter(|_| fresh(FIVE_HOUR_WINDOW_MINUTES)),
        usage.1.filter(|_| fresh(WEEKLY_WINDOW_MINUTES)),
    )
}
fn claude_usage_snapshot() -> Option<ProviderUsage> {
    type Cache = Option<(std::time::Instant, Option<ProviderUsage>)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let mut cache = cache.lock().ok()?;
    if let Some((checked_at, usage)) = *cache
        && checked_at.elapsed() < std::time::Duration::from_secs(2)
    {
        return usage;
    }
    let usage = (|| {
        let path = crate::paths::home_dir()?
            .join(".deppy-sijo")
            .join("claude-usage.json");
        if std::fs::metadata(&path).ok()?.len() > 4 * 1024 {
            return None;
        }
        let snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let percent = |name: &str| {
            snapshot
                .get(name)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 100.0).round() as u8)
        };
        // mcp-proxy가 파일을 쓴 시각(unix seconds). 이 필드가 없는 옛 파일은 나이를
        // 알 길이 없으니 0분으로 보고 그대로 살린다.
        let age_minutes = snapshot
            .get("updated_at")
            .and_then(serde_json::Value::as_i64)
            .map_or(0.0, |updated_at| {
                let now = i64::try_from(deppy_core::time::unix_ms() / 1000).unwrap_or(i64::MAX);
                (now - updated_at) as f64 / 60.0
            });
        let (five_hour, seven_day) =
            fresh_usage_windows((percent("five_hour"), percent("seven_day")), age_minutes);
        // mcp-proxy는 둘 중 하나만 읽혀도 파일을 쓴다(없는 쪽은 null). 한쪽이라도
        // 있으면 스냅샷으로 인정해야 나머지 한쪽이 화면에서 사라지지 않는다.
        (five_hour.is_some() || seven_day.is_some()).then_some((five_hour, seven_day))
    })();
    *cache = Some((std::time::Instant::now(), usage));
    usage
}

fn main() {
 let root = std::path::PathBuf::from(format!("/private/tmp/deppy-usage-fifo-review-{}",std::process::id()));
 let parent = root.join(".deppy-sijo"); std::fs::create_dir_all(&parent).unwrap();
 let fifo=parent.join("claude-usage.json");
 let c=std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
 assert_eq!(unsafe {libc::mkfifo(c.as_ptr(),0o600)},0);
 paths::REVIEW_HOME.set(root.clone()).unwrap();
 let (tx,rx)=std::sync::mpsc::channel();
 let worker=std::thread::spawn(move|| { tx.send(claude_usage_snapshot()).unwrap(); });
 let timely=rx.recv_timeout(std::time::Duration::from_millis(250));
 let blocked=timely.is_err();
 let result=if blocked {
   let mut writer=std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
   writer.write_all(br#"{"five_hour":1,"seven_day":2}"#).unwrap(); drop(writer);
   rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap()
 } else { timely.unwrap() };
 worker.join().unwrap(); std::fs::remove_dir_all(root).unwrap();
 println!("ui_usage_function_blocked_after_250ms={blocked}; after_writer_release={result:?}");
 assert!(!blocked,"usage read called on UI frame must reject nonregular path without waiting for writer");
}
