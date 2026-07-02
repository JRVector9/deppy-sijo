use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// config.toml 루트. 설정 항목은 PR-01에서 추가된다.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {}

impl Config {
    /// config.toml을 읽고, 없으면 기본값으로 생성한다.
    pub fn load_or_create(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join("config.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("config 읽기 실패: {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("config 파싱 실패: {}", path.display()))
        } else {
            let config = Self::default();
            std::fs::write(&path, toml::to_string_pretty(&config)?)
                .with_context(|| format!("config 생성 실패: {}", path.display()))?;
            Ok(config)
        }
    }
}
