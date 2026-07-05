use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const MIN_OUTPUT_BATCH_MS: u64 = 16;

/// config.toml 루트. 각 항목의 소비처는 설계문서 v2.5 참조.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub terminal: TerminalConfig,
    pub performance: PerformanceConfig,
    pub remote: RemoteConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: Theme,
    /// 폴더 트리 사이드바 ON/OFF (file-tree-design §6). OFF면 Panel 미생성 + 상태 drop.
    pub file_tree_enabled: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            file_tree_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn to_egui(self) -> egui::ThemePreference {
        match self {
            Theme::System => egui::ThemePreference::System,
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// PR-05 terminal renderer에서 소비
    pub font_size: f32,
    /// visible session scrollback 상한 (설계문서 14.3)
    pub scrollback_lines: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_size: 11.0,
            scrollback_lines: 10_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// PTY output batch 간격, 16~50ms (설계문서 10.1)
    pub output_batch_ms: u64,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            output_batch_ms: 25,
        }
    }
}

/// remote TLS 서버 설정 (설계문서 remote-tls-delta §2.5 — GUI 배선).
/// 기본은 비활성 — 원격 attach는 셸 접근 부여와 동등(§6)하므로 opt-in이다.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// 앱 시작 시 TLS 원격 서버를 자동 기동할지. 기본 false.
    pub tls_enabled: bool,
    /// bind 포트. 0이면 OS가 임의 할당(local_addr로 확인). 변경은 토글 off/on 후 적용.
    pub port: u16,
}

pub fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

impl Config {
    /// config.toml을 읽고, 없으면 기본값으로 생성한다.
    pub fn load_or_create(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_path(config_dir);
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("config 읽기 실패: {}", path.display()))?;
            let mut config: Self = toml::from_str(&text)
                .with_context(|| format!("config 파싱 실패: {}", path.display()))?;
            // TOML을 손으로 고친 경우 UI 위젯 범위 밖 값이 들어올 수 있다 —
            // 로드 경계에서 정규화 (codex 리뷰: batch 0 = busy-poll, 폭주 scrollback)
            config.normalize();
            Ok(config)
        } else {
            let config = Self::default();
            config.save(&path)?;
            Ok(config)
        }
    }

    /// 범위 밖 값을 안전 범위로 클램프한다 (settings UI 위젯 범위와 동일 기준).
    fn normalize(&mut self) {
        let t = &mut self.terminal;
        t.font_size = if t.font_size.is_finite() {
            t.font_size.clamp(8.0, 32.0)
        } else {
            TerminalConfig::default().font_size
        };
        t.scrollback_lines = t.scrollback_lines.clamp(100, 100_000);
        self.performance.output_batch_ms = self
            .performance
            .output_batch_ms
            .clamp(MIN_OUTPUT_BATCH_MS, 1_000);
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        // 임시 파일 + rename — 쓰기 중 크래시로 빈/부분 TOML이 남아
        // 다음 시작이 파싱 실패로 죽는 것 방지 (codex 리뷰. rename은 동일
        // 디렉터리 내에서 원자적)
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)
            .with_context(|| format!("config 임시 저장 실패: {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("config 교체 실패: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 손으로_고친_범위밖_config는_로드시_정규화() {
        let dir = std::env::temp_dir().join(format!("deppy-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            config_path(&dir),
            "[ui]\ntheme = \"dark\"\n[terminal]\nfont_size = 999.0\nscrollback_lines = 1\n[performance]\noutput_batch_ms = 0\n",
        )
        .unwrap();
        let config = Config::load_or_create(&dir).unwrap();
        assert_eq!(config.terminal.font_size, 32.0);
        assert_eq!(config.terminal.scrollback_lines, 100);
        assert_eq!(config.performance.output_batch_ms, MIN_OUTPUT_BATCH_MS);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_batch_1ms도_16ms로_정규화() {
        let mut config = Config::default();
        config.performance.output_batch_ms = 1;
        config.normalize();
        assert_eq!(config.performance.output_batch_ms, MIN_OUTPUT_BATCH_MS);
    }

    #[test]
    fn 기본값_roundtrip() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn 누락_필드는_기본값으로_채운다() {
        let parsed: Config = toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(parsed.ui.theme, Theme::Dark);
        assert_eq!(parsed.terminal.scrollback_lines, 10_000);
        assert_eq!(parsed.performance.output_batch_ms, 25);
        // 구 config(file_tree_enabled 없음)도 기본 true (§6 serde 기본)
        assert!(parsed.ui.file_tree_enabled);
    }

    #[test]
    fn 파일트리_토글_roundtrip() {
        let mut c = Config::default();
        assert!(c.ui.file_tree_enabled); // 기본 켜짐
        c.ui.file_tree_enabled = false;
        let text = toml::to_string_pretty(&c).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert!(!parsed.ui.file_tree_enabled);
    }

    #[test]
    fn remote_config_기본값_비활성_포트0() {
        let c = RemoteConfig::default();
        assert!(!c.tls_enabled);
        assert_eq!(c.port, 0);
    }

    #[test]
    fn remote_누락시_기본값으로_채운다() {
        // 옛 config(remote 섹션 없음)도 로드된다 (serde default).
        let parsed: Config = toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap();
        assert!(!parsed.remote.tls_enabled);
        assert_eq!(parsed.remote.port, 0);
    }

    #[test]
    fn remote_config_roundtrip() {
        let mut c = Config::default();
        c.remote.tls_enabled = true;
        c.remote.port = 7777;
        let text = toml::to_string_pretty(&c).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn load_or_create_생성_후_재로드_일치() {
        let dir =
            std::env::temp_dir().join(format!("deppy-sijo-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let created = Config::load_or_create(&dir).unwrap();
        let reloaded = Config::load_or_create(&dir).unwrap();
        assert_eq!(created, reloaded);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
