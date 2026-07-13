use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const MIN_OUTPUT_BATCH_MS: u64 = 16;

/// live warm 워크스페이스 상한을 이 기기 RAM에서 유도한다 — live warm 1슬롯당 4GB 예산으로
/// 잡아(에이전트 실측 수백 MB보다 넉넉) 활성 작업+브라우저+IDE와 공존해도 스왑에 안 빠지게
/// 3~8로 클램프한다. **첫 실행(또는 필드 첫 등장) 기본값일 뿐** — 저장 후엔 사용자 값을 쓴다.
pub fn recommended_max_live_warm() -> u32 {
    // RAM은 프로세스 수명 동안 불변이라 sysctl을 1회만 하고 캐시한다 — 설정(성능) 페이지가
    // 힌트용으로 매 프레임 호출해도(리뷰 P3) 이후엔 원자 로드 한 번이다.
    static CACHE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| max_live_warm_for_ram_gb(ram_bytes() / (1024 * 1024 * 1024)))
}

/// 순수 함수(테스트 용이) — RAM(GB)에서 권장 live warm 상한. 1슬롯당 4GB 예산, 3~8 클램프.
fn max_live_warm_for_ram_gb(ram_gb: u64) -> u32 {
    ((ram_gb / 4) as u32).clamp(3, 8)
}

/// 물리 메모리 바이트. 조회 실패 시 16GB로 가정(보수적 기본 4 유도).
fn ram_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // sysctl hw.memsize — 실패하면 폴백.
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: sysctlbyname에 유효한 이름·버퍼·길이를 넘긴다. 실패 시 size는 0 유지.
        let ok = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut size as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 && size > 0 {
            return size;
        }
    }
    16 * 1024 * 1024 * 1024
}

/// config.toml 루트. 각 항목의 소비처는 설계문서 v2.5 참조.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub terminal: TerminalConfig,
    pub performance: PerformanceConfig,
    pub remote: RemoteConfig,
    pub web: WebConfig,
    pub i18n: I18nConfig,
    /// 기본 단축키에서 달라진 항목만 저장한다. 키 이름은 `shortcuts` 모듈이 해석하며,
    /// 알 수 없는 항목은 무시해 이전/이후 버전의 config와 호환한다.
    pub shortcuts: ShortcutsConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShortcutsConfig {
    /// action id -> `Command+Shift+E` 형태의 portable chord.
    pub bindings: BTreeMap<String, String>,
    /// 사용자가 명시적으로 비운 action. bindings와 동시에 있으면 disabled가 우선한다.
    pub disabled: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: Theme,
    /// 폴더 트리 사이드바 ON/OFF (file-tree-design §6). OFF면 Panel 미생성 + 상태 drop.
    pub file_tree_enabled: bool,
    /// 재시작 시 이전 claude/codex 세션을 native resume 명령으로 자동 이어가기(옵션2).
    /// OFF면 자동 명령 주입 안 함(사용자가 수동으로 이어감).
    #[serde(default = "default_true")]
    pub auto_resume_agents: bool,
    /// 에이전트 상태 hook 전역 설치(옵션2 needsInput). ON이면 claude/codex 설정에
    /// deppy hook을 넣어 승인/입력 대기를 정확히 감지. OFF면 제거(regex fallback만).
    #[serde(default = "default_true")]
    pub agent_status_hooks: bool,
    /// .env 라이브 반영 (E5 ⑨, 옵트인): deppy 셸(zsh)에 precmd 훅을 주입해 .env
    /// 변경을 이미 떠 있는 셸에도 다음 프롬프트부터 반영한다. OFF면 새 세션부터만.
    #[serde(default)]
    pub env_live_reload: bool,
    /// 세션 위치 표시명 스타일 (2026-07-13): 기본 = 현재(마지막) 폴더명,
    /// Repo = git 저장소 루트명(.git 상향 탐색 — 저장소 하위 어디서든 레포명).
    #[serde(default)]
    pub session_name_style: SessionNameStyle,
    /// 다음 실행 때 다시 열 마지막 활성 workspace. 삭제되었거나 없으면 default workspace로 대체.
    pub last_workspace_id: Option<String>,
    /// UI(Proportional) 폰트 파일 경로. None = 기본(자동 — macOS는 Apple SD Gothic Neo).
    /// 설정 화면의 목록은 시스템에 설치된 한글 지원 폰트에서 고른다(2026-07-07).
    #[serde(default)]
    pub ui_font: Option<String>,
    /// UI 텍스트 배율 — egui zoom_factor로 UI 전체를 확대/축소한다(사이드바·헤더·설정 등
    /// 하드코딩 FontId까지 포함). 터미널은 font_size를 이 값으로 역보정해 크기가 유지된다
    /// (2026-07-13, 터미널과 독립). 1.0 = 기본.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

fn default_ui_scale() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            file_tree_enabled: true,
            auto_resume_agents: true,
            agent_status_hooks: true,
            env_live_reload: false,
            session_name_style: SessionNameStyle::default(),
            last_workspace_id: None,
            ui_font: None,
            ui_scale: 1.0,
        }
    }
}

/// 세션 위치 표시명 스타일 (2026-07-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionNameStyle {
    /// 현재(마지막) 폴더명 — 예: Crawler/printbakery에서 "printbakery".
    #[default]
    Folder,
    /// git 저장소 루트명 — 저장소 하위 어디서든 "Crawler".
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct I18nConfig {
    /// BCP-47 locale tag. Unknown values normalize to the fallback locale.
    pub locale: String,
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            locale: i18n::FALLBACK_LOCALE.to_owned(),
        }
    }
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
    /// 종료 세션 백엔드 LRU 상한 — 초과분은 압축 아카이브 (§14.3 확장, 2026-07-11)
    pub exited_backend_cap: u32,
    /// 전역 터미널 캐시 예산 (MB) — 초과 시 exited부터 아카이브
    pub cache_budget_mb: u32,
    /// 터미널 모노 폰트 가족 (2026-07-13): [`crate::fonts::MONO_FONTS`] 중 하나 —
    /// 기본 D2Coding(한글 2:1 폭 정합), 대안 JetBrainsMono. 미지값은 기본으로 폴백.
    #[serde(default = "default_mono_font")]
    pub mono_font: String,
    /// 터미널 모노 폰트 굵기(번들 정적 weight). 가족별 지원 굵기는
    /// [`crate::fonts::mono_weights_for`] — 미지값은 로드 시 Regular로 폴백.
    pub mono_weight: String,
}

fn default_mono_font() -> String {
    crate::fonts::DEFAULT_MONO_FONT.to_owned()
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_size: 11.0,
            scrollback_lines: 10_000,
            exited_backend_cap: 64,
            cache_budget_mb: 128,
            mono_font: default_mono_font(),
            mono_weight: crate::fonts::DEFAULT_MONO_WEIGHT.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Runtime worker의 idle fallback poll 간격, UI 범위 16~50ms. 실제 PTY 출력은
    /// reader wake로 즉시 pump되고, 연속 viewport는 runtime에서 8ms로 frame pacing한다.
    pub output_batch_ms: u64,
    /// warm(백그라운드) 워크스페이스로 유지할 최대 개수(활성 제외). 초과 시 오래된 것부터
    /// 절전 — **실행 중 세션이 없는 것만**(§14.1). 빈 워커라 거의 공짜(수십 MB, CPU 0).
    pub max_warm: u32,
    /// 실행 중 세션이 있는 warm 워크스페이스 hard cap. 초과하는 전환은 기존 작업을 죽이지
    /// 않고 거부한다. 높이면 동시 워커+에이전트가 늘어 메모리↑ (지배 요인은 에이전트).
    /// 기본값은 RAM 유도([`recommended_max_live_warm`]) — 첫 실행 시 1회, 이후 사용자 값.
    pub max_live_warm: u32,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            output_batch_ms: 25,
            max_warm: 2,
            max_live_warm: recommended_max_live_warm(),
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

/// 모바일 웹(PWA) 내장 서버 설정 (mobile-pwa 계획 v3.3 P1). 기본 비활성 — opt-in.
/// OFF면 리스너 스레드 자체를 만들지 않는다(리소스 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// 앱 시작 시 웹서버 자동 기동. 기본 false.
    pub enabled: bool,
    /// 127.0.0.1 bind 포트. 0 = OS 임의 할당. `tailscale serve --bg <port>` 프록시가
    /// 재시작 후에도 유효하려면 고정 포트가 필요해 기본값을 고정 포트로 둔다.
    pub port: u16,
    /// tailscale serve가 노출하는 ts.net 호스트명 — Host 검증 허용 목록과 접속 URL/QR에
    /// 사용. 빈 문자열이면 loopback 계열 Host만 허용된다(폰 접속에는 설정 필요).
    pub ts_hostname: String,
    /// (자리) cert 모드 인증서 PEM 경로 — `tailscale cert` 자체 TLS는 후속 구현. 현재 미사용.
    pub tls_cert_pem: String,
    /// (자리) cert 모드 개인키 PEM 경로. 현재 미사용.
    pub tls_key_pem: String,
    /// (자리) 비-loopback bind opt-in (remote C-4 관례) — cert 모드에서만 의미. 현재 미사용.
    pub allow_non_loopback: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8737,
            ts_hostname: String::new(),
            tls_cert_pem: String::new(),
            tls_key_pem: String::new(),
            allow_non_loopback: false,
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
        t.exited_backend_cap = t.exited_backend_cap.clamp(4, 512);
        t.cache_budget_mb = t.cache_budget_mb.clamp(32, 2048);
        if !crate::fonts::MONO_FONTS.contains(&t.mono_font.as_str()) {
            t.mono_font = crate::fonts::DEFAULT_MONO_FONT.to_owned();
        }
        // 굵기는 가족별 지원 목록으로 검증 — 가족 전환으로 미지원 굵기가 남으면 Regular.
        if !crate::fonts::mono_weights_for(&t.mono_font).contains(&t.mono_weight.as_str()) {
            t.mono_weight = crate::fonts::DEFAULT_MONO_WEIGHT.to_owned();
        }
        self.ui.ui_scale = if self.ui.ui_scale.is_finite() {
            self.ui.ui_scale.clamp(0.7, 1.5)
        } else {
            1.0
        };
        self.performance.output_batch_ms = self
            .performance
            .output_batch_ms
            .clamp(MIN_OUTPUT_BATCH_MS, 1_000);
        self.performance.max_warm = self.performance.max_warm.clamp(0, 8);
        self.performance.max_live_warm = self.performance.max_live_warm.clamp(1, 12);
        self.i18n.locale = i18n::normalize_locale(&self.i18n.locale);
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
    fn 권장_live_warm은_ram에서_유도되고_3에서_8로_클램프() {
        assert_eq!(max_live_warm_for_ram_gb(0), 3); // 하한
        assert_eq!(max_live_warm_for_ram_gb(8), 3); // 8/4=2 → 3으로 클램프
        assert_eq!(max_live_warm_for_ram_gb(16), 4);
        assert_eq!(max_live_warm_for_ram_gb(24), 6);
        assert_eq!(max_live_warm_for_ram_gb(32), 8);
        assert_eq!(max_live_warm_for_ram_gb(128), 8); // 상한
    }

    #[test]
    fn warm_상한_기본값과_정규화_클램프() {
        let config = Config::default();
        assert_eq!(config.performance.max_warm, 2);
        // live warm 기본은 이 기기 RAM 유도 — 값은 다르나 3~8 범위는 불변.
        assert!((3..=8).contains(&config.performance.max_live_warm));
        // 손으로 범위 밖 값을 넣어도 로드 정규화가 클램프한다.
        let mut c = Config::default();
        c.performance.max_warm = 99;
        c.performance.max_live_warm = 0;
        c.normalize();
        assert_eq!(c.performance.max_warm, 8);
        assert_eq!(c.performance.max_live_warm, 1);
    }

    #[test]
    fn 누락_필드는_기본값으로_채운다() {
        let parsed: Config = toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(parsed.ui.theme, Theme::Dark);
        assert_eq!(parsed.terminal.scrollback_lines, 10_000);
        assert_eq!(parsed.performance.output_batch_ms, 25);
        assert_eq!(parsed.i18n.locale, i18n::FALLBACK_LOCALE);
        assert!(parsed.shortcuts.bindings.is_empty());
        assert!(parsed.shortcuts.disabled.is_empty());
        // 구 config(file_tree_enabled 없음)도 기본 true (§6 serde 기본)
        assert!(parsed.ui.file_tree_enabled);
        assert_eq!(parsed.ui.last_workspace_id, None);
    }

    #[test]
    fn locale_설정은_저장되고_알수없는_locale은_fallback() {
        let mut config = Config::default();
        config.i18n.locale = "ja-JP".to_owned();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.i18n.locale, "ja-JP");

        let mut unknown: Config = toml::from_str("[i18n]\nlocale = \"xx-YY\"\n").unwrap();
        unknown.normalize();
        assert_eq!(unknown.i18n.locale, i18n::FALLBACK_LOCALE);
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
    fn 마지막_workspace_id_roundtrip() {
        let mut c = Config::default();
        c.ui.last_workspace_id = Some("ws-last".to_owned());
        let text = toml::to_string_pretty(&c).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.last_workspace_id.as_deref(), Some("ws-last"));
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
    fn web_config_기본값은_off_고정포트() {
        // OFF가 기본 — OFF면 리스너 스레드 자체가 없어야 한다 (v3.3 P1 리소스 예산).
        let c = WebConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.port, 8737);
        assert!(c.ts_hostname.is_empty());
        assert!(c.tls_cert_pem.is_empty());
        assert!(c.tls_key_pem.is_empty());
        assert!(!c.allow_non_loopback);
    }

    #[test]
    fn web_누락시_기본값으로_채운다() {
        // 옛 config([web] 섹션 없음)도 로드된다 (serde default).
        let parsed: Config = toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(parsed.web, WebConfig::default());
    }

    #[test]
    fn web_config_roundtrip() {
        let mut c = Config::default();
        c.web.enabled = true;
        c.web.port = 9000;
        c.web.ts_hostname = "mac.tail.ts.net".to_owned();
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
