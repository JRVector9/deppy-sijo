mod app;
mod config;
mod env;
mod fonts;
mod paths;
mod perf;
mod storage;
mod ui;

fn main() -> anyhow::Result<()> {
    let paths = paths::AppPaths::init()?;
    // guard가 drop되면 파일 로그 flush가 끊기므로 main 끝까지 유지한다.
    let _log_guard = init_logging(&paths);
    // 중복 실행 방지 lock (설계문서 PR-14 crash recovery). config 로드/생성보다
    // 먼저 잡는다 — 두 인스턴스의 config I/O 경쟁도 이 lock이 보호한다 (codex 리뷰).
    // drop 시 자동 해제되므로 main 끝까지 살려 둔다.
    let _run_lock = persist::LockFile::acquire(&paths.data_dir.join("deppy.lock"))
        .map_err(|e| anyhow::anyhow!("이미 실행 중이거나 lock 획득 실패: {e:#}"))?;

    let config = config::Config::load_or_create(&paths.config_dir)?;
    let config_path = config::config_path(&paths.config_dir);
    // insecure fallback 금지(설계문서 1.4) — 등록 실패 시 credential 조작이 에러로 표면화된다
    if let Err(e) = secret::init_platform_store() {
        tracing::warn!("keyring store 초기화 실패 — 자격증명 기능 비활성: {e:#}");
    }

    let db_path = paths.data_dir.join("metadata.sqlite3");
    let db = storage::Db::open(&db_path)?;
    let workspace_id = db.ensure_default_workspace()?;
    // 이전 실행이 비정상 종료됐다면 남은 세션을 Exited로 정리 (crash recovery).
    // 실패는 기동 중단 — 거짓 running 상태로 복원 UI가 뜨면 안 된다 (codex 리뷰 반영)
    let reconciled = db
        .reconcile_orphan_sessions()
        .map_err(|e| anyhow::anyhow!("crash recovery(세션 reconcile) 실패: {e:#}"))?;
    if reconciled > 0 {
        tracing::info!("이전 실행의 orphan 세션 {reconciled}건 Exited 처리");
    }
    // 세션 로그 베이스 (설계문서 7장: logs/<workspace_id>/<session_id>/) —
    // workspace별 하위 디렉터리는 App이 workspace_id로 만든다 (전환 지원).
    let logs_base = paths.data_dir.join("logs");
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        data_dir = %paths.data_dir.display(),
        log_dir = %paths.log_dir.display(),
        "앱 시작"
    );

    eframe::run_native(
        "Deppy Sijo",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            // 저장된 테마를 시작 시점에 적용
            cc.egui_ctx.set_theme(config.ui.theme.to_egui());
            fonts::install_cjk_fallback(&cc.egui_ctx);
            Ok(Box::new(app::App::new(
                config,
                config_path,
                db,
                workspace_id,
                logs_base,
                db_path,
                cc.egui_ctx.clone(),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe 실행 실패: {e}"))
}

fn init_logging(paths: &paths::AppPaths) -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(&paths.log_dir, "app.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(file_writer)
        .with_ansi(false)
        .init();
    guard
}
