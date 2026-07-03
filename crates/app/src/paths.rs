use std::path::PathBuf;

use anyhow::Context;
use directories::ProjectDirs;

/// config/data/log 디렉터리 경로. init()이 생성까지 보장한다.
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn init() -> anyhow::Result<Self> {
        // 번들 ID 규칙: app.vector9.<서비스명>
        let dirs = ProjectDirs::from("app", "vector9", "deppy-sijo")
            .context("홈 디렉터리를 찾을 수 없음")?;
        let paths = Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            log_dir: dirs.data_dir().join("logs"),
        };
        for dir in [&paths.config_dir, &paths.data_dir, &paths.log_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("디렉터리 생성 실패: {}", dir.display()))?;
        }
        Ok(paths)
    }
}
