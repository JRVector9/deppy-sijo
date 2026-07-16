use std::path::PathBuf;

use anyhow::Context;
use directories::ProjectDirs;

/// 앱 디렉터리 식별자 단일 지점 — 번들 ID 규칙: app.vector9.<서비스명>.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("app", "vector9", "deppy-sijo")
}

/// 사용자 홈 디렉터리. `std::env::var_os("HOME")` 직접 조회 대신 이 단일 지점을 쓴다 —
/// HOME 환경변수가 없는 환경(네이티브 Windows 등)에서도 OS API로 해석된다.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

/// 앱 캐시 디렉터리 (AppPaths와 동일 식별자).
pub fn cache_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.cache_dir().to_path_buf())
}

/// config/data/log 디렉터리 경로. init()이 생성까지 보장한다.
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn init() -> anyhow::Result<Self> {
        let dirs = project_dirs().context("홈 디렉터리를 찾을 수 없음")?;
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
