//! remote TLS known_hosts (단계 C-3 잔여, 설계doc §2.2 · Open Question 3/5).
//!
//! SSH known_hosts와 같은 TOFU 저장소: 클라이언트가 attach한 서버(host = "ip:port")별로
//! 신뢰한 인증서 지문을 기억한다. 포맷은 한 줄에 `host 지문` (공백 구분, `#` 주석 허용) —
//! 사용자가 열어 보고 손으로 지울 수 있는 단순 텍스트. 저장은 tmp+rename 원자.
//!
//! 정책(호출측 UX가 소비):
//!   - FirstUse: 항목 없음 — TOFU로 핀 가능. **최초 접속은 무검증 창**(SSH와 동일 한계,
//!     설계 §6) — 대역외 지문 대조를 사용자에게 명시 요구하는 것은 앱 UI 소관.
//!   - Match: 저장된 지문과 일치 — 진행.
//!   - Mismatch: 다른 지문 — **거부**가 기본값(서버 교체 또는 MITM). 사용자가 의도한
//!     변경이면 `forget` 후 재-TOFU.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// check 결과 — 호출측(attach/TOFU UX)이 분기한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TofuDecision {
    /// 이 host의 항목이 없다 — 최초 접속(TOFU 핀 대상).
    FirstUse,
    /// 저장된 지문과 일치.
    Match,
    /// 저장된 지문과 다르다 — 서버 교체/MITM 가능성. stored는 기존 핀.
    Mismatch { stored: String },
}

/// host("ip:port") → 지문("ab:cd:…", 소문자) 저장소.
#[derive(Debug)]
pub struct KnownHosts {
    path: PathBuf,
    entries: HashMap<String, String>,
}

impl KnownHosts {
    /// 파일에서 로드한다. 파일이 없으면 빈 저장소(첫 사용). 손상 라인은 경고 후 스킵 —
    /// 한 줄이 깨졌다고 전체 신뢰 기록을 버리지 않는다.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut entries = HashMap::new();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    match (parts.next(), parts.next()) {
                        (Some(host), Some(fp)) => {
                            entries.insert(host.to_owned(), fp.to_ascii_lowercase());
                        }
                        _ => tracing::warn!("known_hosts 손상 라인 스킵: {line:?}"),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("known_hosts 읽기 실패: {}", path.display()));
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    /// 저장된 지문 (소문자 정규화).
    pub fn lookup(&self, host: &str) -> Option<&str> {
        self.entries.get(host).map(String::as_str)
    }

    /// 관찰된 지문을 저장 기록과 대조한다.
    pub fn check(&self, host: &str, fingerprint: &str) -> TofuDecision {
        match self.entries.get(host) {
            None => TofuDecision::FirstUse,
            Some(stored) if stored.eq_ignore_ascii_case(fingerprint) => TofuDecision::Match,
            Some(stored) => TofuDecision::Mismatch {
                stored: stored.clone(),
            },
        }
    }

    /// host의 지문을 핀하고 즉시 파일에 반영한다 (tmp+rename 원자).
    /// 저장 실패 시 in-memory 변경을 **롤백**한다 — 파일과 메모리가 갈라져 재시도가
    /// 영속 없이 Verified로 통과하는 것 방지 (codex P3).
    pub fn pin(&mut self, host: &str, fingerprint: &str) -> anyhow::Result<()> {
        let previous = self
            .entries
            .insert(host.to_owned(), fingerprint.to_ascii_lowercase());
        if let Err(e) = self.save() {
            match previous {
                Some(prev) => self.entries.insert(host.to_owned(), prev),
                None => self.entries.remove(host),
            };
            return Err(e);
        }
        Ok(())
    }

    /// host 항목을 제거한다 (지문 변경 시 재-TOFU 경로). 없는 host는 no-op.
    /// 저장 실패 시 롤백 — pin과 동일 원칙.
    pub fn forget(&mut self, host: &str) -> anyhow::Result<()> {
        if let Some(prev) = self.entries.remove(host)
            && let Err(e) = self.save()
        {
            self.entries.insert(host.to_owned(), prev);
            return Err(e);
        }
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("known_hosts 디렉터리 생성 실패: {}", parent.display()))?;
        }
        // 결정적 순서(정렬)로 직렬화 — diff/검사 용이
        let mut hosts: Vec<_> = self.entries.iter().collect();
        hosts.sort();
        let mut text = String::from("# deppy remote TLS known_hosts — host 지문(SHA-256)\n");
        for (host, fp) in hosts {
            text.push_str(host);
            text.push(' ');
            text.push_str(fp);
            text.push('\n');
        }
        deppy_core::fs::atomic_write(&self.path, text.as_bytes())
            .with_context(|| format!("known_hosts 원자 기록 실패: {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-kh-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("known_hosts")
    }

    #[test]
    fn 없는_파일은_빈_저장소이고_first_use() {
        let path = temp_path("empty");
        let kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.check("127.0.0.1:7777", "aa:bb"), TofuDecision::FirstUse);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn pin은_저장되고_재로드_후_match_불일치는_mismatch() {
        let path = temp_path("roundtrip");
        let mut kh = KnownHosts::load(&path).unwrap();
        kh.pin("127.0.0.1:7777", "AA:BB:CC").unwrap();
        // 재로드 — 소문자 정규화되어 일치
        let kh2 = KnownHosts::load(&path).unwrap();
        assert_eq!(kh2.check("127.0.0.1:7777", "aa:bb:cc"), TofuDecision::Match);
        assert_eq!(
            kh2.check("127.0.0.1:7777", "dd:ee:ff"),
            TofuDecision::Mismatch {
                stored: "aa:bb:cc".to_owned()
            }
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn forget후_재tofu_가능() {
        let path = temp_path("forget");
        let mut kh = KnownHosts::load(&path).unwrap();
        kh.pin("h:1", "aa").unwrap();
        kh.forget("h:1").unwrap();
        assert_eq!(kh.check("h:1", "bb"), TofuDecision::FirstUse);
        // 파일에서도 사라짐
        let kh2 = KnownHosts::load(&path).unwrap();
        assert_eq!(kh2.lookup("h:1"), None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn 손상_라인은_스킵하고_나머지는_로드() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "# 주석\nh:1 aa:bb\n망가진줄만있음\nh:2 cc:dd\n").unwrap();
        let kh = KnownHosts::load(&path).unwrap();
        assert_eq!(kh.lookup("h:1"), Some("aa:bb"));
        assert_eq!(kh.lookup("h:2"), Some("cc:dd"));
        assert_eq!(kh.lookup("망가진줄만있음"), None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
