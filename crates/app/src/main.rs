mod app;
mod config;
mod env;
mod fonts;
mod paths;
mod perf;
mod storage;
mod theme;
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
    let workspace_id = initial_workspace_id(&db, config.ui.last_workspace_id.as_deref())?;
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
        eframe::NativeOptions {
            // 창 위치/크기 영속 안 함 — 외부 모니터 분리 후 저장된 좌표로 복원되면
            // 창이 화면 밖에 떠서 "앱이 죽은 것처럼" 보인다 (2026-07-05 실증:
            // eframe 기본 복원은 현재 모니터 배치로 clamp되지 않았다). 위치 기억보다
            // 항상 보이는 것이 우선. 런타임 분리는 app.rs 오프스크린 감지가 방어.
            persist_window: false,
            // 주 화면 중앙에 뜬다 — 위치 미지정이면 OS가 임의(보조 모니터 포함) 배치해
            // 사용자가 창을 잃어버릴 수 있다 (2026-07-05: 왼쪽 외부 모니터에 떠서
            // "앱이 죽은 줄" — 실은 정상 실행 중이었다).
            centered: true,
            // 상단 바를 macOS 네이티브 타이틀바 영역으로 끌어올린다 (2026-07-06):
            // fullsize content view로 콘텐츠가 타이틀바까지 확장되고, 네이티브 제목
            // 텍스트는 숨긴다. 신호등(닫기/최소화/전체화면)은 그대로 남는다.
            // 상단 바 렌더는 신호등 폭만큼 왼쪽 여백을 두고, 빈 영역은 창 드래그로 처리한다.
            // eframe 문서 권장 조합: fullsize_content_view는 titlebar_shown(false)·
            // title_shown(false)와 함께 써야 한다. titlebar_shown(true)면 네이티브
            // 타이틀바가 상단 바를 덮어 잘라냈다(2026-07-06 사용자 화면). 신호등은
            // 데코레이션이 켜져 있어 그대로 남는다.
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1200.0, 800.0])
                .with_fullsize_content_view(true)
                .with_title_shown(false)
                .with_titlebar_shown(false),
            ..Default::default()
        },
        Box::new(move |cc| {
            // 저장된 테마를 시작 시점에 적용
            // 목업 팔레트(시안 액센트 + 쿨그레이)를 테마별로 심는다 — set_theme보다 먼저
            // 등록해야 프리퍼런스 적용 시 커스텀 색이 선택된다.
            theme::install_palette(&cc.egui_ctx);
            cc.egui_ctx.set_theme(config.ui.theme.to_egui());
            fonts::install_cjk_fallback(&cc.egui_ctx);
            install_macos_menu();
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

fn initial_workspace_id(
    db: &storage::Db,
    last_workspace_id: Option<&str>,
) -> anyhow::Result<String> {
    let fallback = db.ensure_default_workspace()?;
    let Some(last) = last_workspace_id else {
        return Ok(fallback);
    };
    let exists = db
        .list_workspaces()?
        .iter()
        .any(|workspace| workspace.id == last);
    if exists {
        Ok(last.to_owned())
    } else {
        Ok(fallback)
    }
}

/// macOS 네이티브 메뉴바 (2026-07-05 사용자 요청) — About/설정(⌘,)/종료(⌘Q).
/// 이벤트는 app.rs가 MenuEvent::receiver로 폴링한다 ("settings" id).
#[cfg(target_os = "macos")]
fn install_macos_menu() {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    let menu = muda::Menu::new();
    let app_menu = muda::Submenu::new("Deppy Sijo", true);
    let about = muda::PredefinedMenuItem::about(
        Some("Deppy Sijo에 관하여"),
        Some(muda::AboutMetadata {
            name: Some("Deppy Sijo".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        }),
    );
    let settings = muda::MenuItem::with_id(
        "settings",
        "설정…",
        true,
        Some(Accelerator::new(Some(Modifiers::META), Code::Comma)),
    );
    let quit = muda::PredefinedMenuItem::quit(Some("Deppy Sijo 종료"));
    let items: [&dyn muda::IsMenuItem; 5] = [
        &about,
        &muda::PredefinedMenuItem::separator(),
        &settings,
        &muda::PredefinedMenuItem::separator(),
        &quit,
    ];
    if let Err(e) = app_menu.append_items(&items) {
        tracing::warn!("메뉴 구성 실패: {e}");
        return;
    }
    if let Err(e) = menu.append(&app_menu) {
        tracing::warn!("메뉴 구성 실패: {e}");
        return;
    }
    menu.init_for_nsapp();
    // 메뉴는 앱 수명 내내 유지 — drop되면 NSMenu 항목이 사라진다
    std::mem::forget(menu);
    std::mem::forget(app_menu);
}

#[cfg(not(target_os = "macos"))]
fn install_macos_menu() {}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> (std::path::PathBuf, storage::Db) {
        let dir =
            std::env::temp_dir().join(format!("deppy-sijo-main-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let db = storage::Db::open(&path).unwrap();
        (dir, db)
    }

    #[test]
    fn initial_workspace_prefers_existing_last_workspace() {
        let (dir, db) = temp_db("last-existing");
        let _default = db.ensure_default_workspace().unwrap();
        let last = db.create_workspace("last").unwrap();
        assert_eq!(initial_workspace_id(&db, Some(&last)).unwrap(), last);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn initial_workspace_falls_back_when_last_workspace_is_missing() {
        let (dir, db) = temp_db("last-missing");
        let default = db.ensure_default_workspace().unwrap();
        assert_eq!(initial_workspace_id(&db, Some("missing")).unwrap(), default);
        let _ = std::fs::remove_dir_all(dir);
    }
}
