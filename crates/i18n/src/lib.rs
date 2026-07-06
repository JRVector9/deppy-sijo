use std::collections::{BTreeMap, BTreeSet};

pub const FALLBACK_LOCALE: &str = "en-US";
pub const REQUIRED_LOCALES: &[&str] = &["en-US", "ja-JP", "zh-Hans", "zh-Hant"];
pub const OPTIONAL_LOCALES: &[&str] = &["ko-KR"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    locale: String,
    primary: BTreeMap<String, String>,
    fallback: BTreeMap<String, String>,
}

impl Catalog {
    pub fn load(locale: &str) -> anyhow::Result<Self> {
        let normalized = normalize_locale(locale);
        let fallback = parse_locale_file(FALLBACK_LOCALE, locale_source(FALLBACK_LOCALE)?)?;
        let primary = if normalized == FALLBACK_LOCALE {
            fallback.clone()
        } else {
            parse_locale_file(&normalized, locale_source(&normalized)?)?
        };
        Ok(Self {
            locale: normalized,
            primary,
            fallback,
        })
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    pub fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        let template = self
            .primary
            .get(key)
            .or_else(|| self.fallback.get(key))
            .map(String::as_str)
            .unwrap_or(key);
        interpolate(template, args)
    }

    pub fn loaded_locale_count(&self) -> usize {
        if self.locale == FALLBACK_LOCALE { 1 } else { 2 }
    }
}

pub fn normalize_locale(locale: &str) -> String {
    if is_supported_locale(locale) {
        locale.to_owned()
    } else {
        FALLBACK_LOCALE.to_owned()
    }
}

pub fn is_supported_locale(locale: &str) -> bool {
    REQUIRED_LOCALES.contains(&locale) || OPTIONAL_LOCALES.contains(&locale)
}

pub fn validate_required_locale_completeness() -> anyhow::Result<()> {
    let fallback = parse_locale_file(FALLBACK_LOCALE, locale_source(FALLBACK_LOCALE)?)?;
    let fallback_keys: BTreeSet<&str> = fallback.keys().map(String::as_str).collect();
    for locale in REQUIRED_LOCALES {
        let entries = parse_locale_file(locale, locale_source(locale)?)?;
        let keys: BTreeSet<&str> = entries.keys().map(String::as_str).collect();
        let missing: Vec<&str> = fallback_keys.difference(&keys).copied().collect();
        let extra: Vec<&str> = keys.difference(&fallback_keys).copied().collect();
        if !missing.is_empty() || !extra.is_empty() {
            anyhow::bail!("{locale} locale key mismatch: missing={missing:?} extra={extra:?}");
        }
    }
    Ok(())
}

fn locale_source(locale: &str) -> anyhow::Result<&'static str> {
    match locale {
        "en-US" => Ok(include_str!("../locales/en-US/messages.txt")),
        "ja-JP" => Ok(include_str!("../locales/ja-JP/messages.txt")),
        "zh-Hans" => Ok(include_str!("../locales/zh-Hans/messages.txt")),
        "zh-Hant" => Ok(include_str!("../locales/zh-Hant/messages.txt")),
        "ko-KR" => Ok(include_str!("../locales/ko-KR/messages.txt")),
        _ => anyhow::bail!("unsupported locale: {locale}"),
    }
}

fn parse_locale_file(locale: &str, source: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for (line_no, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!("{locale}:{} invalid locale line: {raw}", line_no + 1);
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            anyhow::bail!("{locale}:{} empty locale key", line_no + 1);
        }
        if entries.insert(key.to_owned(), value.to_owned()).is_some() {
            anyhow::bail!("{locale}:{} duplicate locale key: {key}", line_no + 1);
        }
    }
    Ok(entries)
}

fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_locales_have_complete_keys() {
        validate_required_locale_completeness().unwrap();
    }

    #[test]
    fn fallback_locale_normalizes_unknown_locale() {
        assert_eq!(normalize_locale("xx-YY"), FALLBACK_LOCALE);
    }

    #[test]
    fn catalog_uses_primary_then_fallback_and_args() {
        let catalog = Catalog::load("en-US").unwrap();
        assert_eq!(
            catalog.t("app.error", &[("message", "boom")]),
            "Error: boom"
        );
        assert_eq!(catalog.t("missing.key", &[]), "missing.key");
    }

    #[test]
    fn non_fallback_catalog_loads_current_and_fallback_only() {
        let catalog = Catalog::load("ja-JP").unwrap();
        assert_eq!(catalog.locale(), "ja-JP");
        assert_eq!(catalog.loaded_locale_count(), 2);
    }

    #[test]
    fn layout_gate_core_ui_labels_fit_generous_budgets() {
        let samples = [
            ("top.settings", Vec::new(), 28),
            ("top.credentials", Vec::new(), 32),
            ("top.connectors", Vec::new(), 28),
            ("top.environment", Vec::new(), 32),
            ("top.notifications.unread", vec![("count", "999")], 36),
            ("settings.file_tree_sidebar", Vec::new(), 48),
            ("settings.output_batch_ms", Vec::new(), 56),
            ("workspace.start_shell_prompt", Vec::new(), 56),
            ("file_tree.insert_path_terminal", Vec::new(), 56),
            (
                "file_tree.permanent_delete_prompt",
                vec![("name", "プロジェクト/設定文件.rs")],
                96,
            ),
            (
                "notification.session.needs_approval",
                vec![("title", "프로젝트/설정파일.rs")],
                80,
            ),
            (
                "runtime.spawn_failed.agent_secret",
                vec![("credential_id", "cred-設定"), ("error", "not found")],
                120,
            ),
        ];
        for locale in REQUIRED_LOCALES.iter().chain(OPTIONAL_LOCALES.iter()) {
            let catalog = Catalog::load(locale).unwrap();
            for (key, args, budget) in &samples {
                let rendered = catalog.t(key, args);
                assert!(!rendered.is_empty(), "{locale} {key} rendered empty");
                assert!(
                    !rendered.contains('{'),
                    "{locale} {key} left an uninterpolated placeholder: {rendered}"
                );
                assert!(
                    visual_width(&rendered) <= *budget,
                    "{locale} {key} width {} > budget {budget}: {rendered}",
                    visual_width(&rendered)
                );
            }
        }
    }

    #[test]
    fn unsupported_locale_normalizes_to_fallback() {
        assert_eq!(normalize_locale("en-XA"), FALLBACK_LOCALE);
    }

    fn visual_width(text: &str) -> usize {
        text.chars()
            .map(|ch| if ch.is_ascii() { 1 } else { 2 })
            .sum()
    }
}
