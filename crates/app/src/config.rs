use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// config.toml 루트. 각 항목의 소비처는 설계문서 v2.5 참조.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub terminal: TerminalConfig,
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: Theme,
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
            font_size: 14.0,
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
            toml::from_str(&text).with_context(|| format!("config 파싱 실패: {}", path.display()))
        } else {
            let config = Self::default();
            config.save(&path)?;
            Ok(config)
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("config 저장 실패: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn load_or_create_생성_후_재로드_일치() {
        let dir =
            std::env::temp_dir().join(format!("deppy-jelly-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let created = Config::load_or_create(&dir).unwrap();
        let reloaded = Config::load_or_create(&dir).unwrap();
        assert_eq!(created, reloaded);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
