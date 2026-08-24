use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrokCurrency {
    Usd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrokCredits {
    pub(crate) currency: GrokCurrency,
    pub(crate) minor_units: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrokUsage {
    pub(crate) weekly_remaining_percent: Option<u8>,
    pub(crate) monthly_remaining_percent: Option<u8>,
    pub(crate) credits_left: Option<GrokCredits>,
}

fn parse_usage(output: &str) -> Option<GrokUsage> {
    let clean = strip_terminal_control_sequences(output);
    let lines = clean.split(['\r', '\n']).collect::<Vec<_>>();
    let usage = GrokUsage {
        weekly_remaining_percent: extract_window_remaining(&lines, "weeklylimit"),
        monthly_remaining_percent: extract_window_remaining(&lines, "monthlylimit"),
        credits_left: extract_credits_left(&lines),
    };
    (usage.weekly_remaining_percent.is_some()
        || usage.monthly_remaining_percent.is_some()
        || usage.credits_left.is_some())
    .then_some(usage)
}

fn extract_window_remaining(lines: &[&str], label: &str) -> Option<u8> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    static LIMIT: OnceLock<regex::Regex> = OnceLock::new();
    let percent = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d+)(?:\.\d+)?\s*%\s*(used|left|remaining)")
            .expect("static Grok percent regex")
    });
    let limit = LIMIT.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)\s*(?:used\s*)?of\s*\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)",
        )
        .expect("static Grok limit regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains(label) {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let compact = compact_label(candidate);
            if candidate != line
                && (compact.contains("weeklylimit") || compact.contains("monthlylimit"))
            {
                break;
            }
            if let Some(captures) = percent.captures(candidate) {
                let value = captures.get(1)?.as_str().parse::<u64>().ok()?.min(100) as u8;
                return match captures.get(2)?.as_str().to_ascii_lowercase().as_str() {
                    "used" => Some(100 - value),
                    "left" | "remaining" => Some(value),
                    _ => None,
                };
            }
            if let Some(captures) = limit.captures(candidate) {
                let used = parse_usd_minor(captures.get(1)?.as_str())?;
                let total = parse_usd_minor(captures.get(2)?.as_str())?;
                return remaining_percent(used, total);
            }
        }
    }
    None
}

fn extract_credits_left(lines: &[&str]) -> Option<GrokCredits> {
    static MONEY: OnceLock<regex::Regex> = OnceLock::new();
    let money = MONEY.get_or_init(|| {
        regex::Regex::new(r"\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)").expect("static Grok money regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains("creditsleft") {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let compact = compact_label(candidate);
            if candidate != line
                && (compact.contains("weeklylimit")
                    || compact.contains("monthlylimit")
                    || compact.contains("creditsleft"))
            {
                break;
            }
            if let Some(captures) = money.captures(candidate) {
                return Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: parse_usd_minor(captures.get(1)?.as_str())?,
                });
            }
        }
    }
    None
}

fn parse_usd_minor(text: &str) -> Option<u64> {
    let normalized = text.replace(',', "");
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(100)?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()?.checked_mul(10)?,
        2 => fraction.parse::<u64>().ok()?,
        _ => return None,
    };
    whole.checked_add(fraction)
}

fn remaining_percent(used: u64, total: u64) -> Option<u8> {
    if total == 0 || used > total {
        return None;
    }
    let remaining = total.checked_sub(used)?;
    let scaled = remaining.checked_mul(100)?;
    let rounded = scaled.checked_add(total / 2)?.checked_div(total)?;
    u8::try_from(rounded.min(100)).ok()
}

fn compact_label(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn strip_terminal_control_sequences(output: &str) -> String {
    static OSC: OnceLock<regex::Regex> = OnceLock::new();
    static CSI: OnceLock<regex::Regex> = OnceLock::new();
    let osc = OSC.get_or_init(|| {
        regex::Regex::new(r"\x1b\][^\x07]*(?:\x07|\x1b\\)").expect("static OSC regex")
    });
    let csi = CSI
        .get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("static CSI regex"));
    csi.replace_all(&osc.replace_all(output, ""), "")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_PANEL: &str = "\
Usage
  Context window  41% (205k / 500k)
  WEEKLY
    Weekly limit  30% used  Next reset: 4d 2h
  MONTHLY
    Monthly limit  $15.00 used of $100.00 limit
  Credits left: $12.34
";

    #[test]
    fn full_panel_returns_remaining_windows_and_exact_credits() {
        assert_eq!(
            parse_usage(FULL_PANEL),
            Some(GrokUsage {
                weekly_remaining_percent: Some(70),
                monthly_remaining_percent: Some(85),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 1_234,
                }),
            })
        );
    }

    #[test]
    fn partial_and_redrawn_panels_keep_only_labeled_latest_values() {
        let panel = "\
Weekly limit 90% used
Context window 4% used
Weekly limit
20% used
Monthly limit
15% left
Credits left: $1,234.50
";
        assert_eq!(
            parse_usage(panel),
            Some(GrokUsage {
                weekly_remaining_percent: Some(80),
                monthly_remaining_percent: Some(15),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 123_450,
                }),
            })
        );
    }

    #[test]
    fn invalid_or_accountless_panels_do_not_invent_usage() {
        assert_eq!(parse_usage("You are not authenticated"), None);
        assert_eq!(parse_usage("Manage billing to view usage"), None);
        assert_eq!(parse_usage("Context window 41% used"), None);
        assert_eq!(parse_usage("Weekly limit $1 used of $0 limit"), None);
        assert_eq!(parse_usage("Credits left: $18446744073709551616.00"), None);
    }

    #[test]
    fn credits_without_money_do_not_capture_later_section_money() {
        assert_eq!(
            parse_usage("Credits left:\nMonthly limit $15.00 used of $100.00 limit\n",),
            Some(GrokUsage {
                weekly_remaining_percent: None,
                monthly_remaining_percent: Some(85),
                credits_left: None,
            })
        );
    }

    #[test]
    fn ansi_is_removed_and_percentages_are_clamped_without_context_false_positives() {
        assert_eq!(
            parse_usage("\u{1b}[31mWeekly limit 999% used\u{1b}[0m"),
            Some(GrokUsage {
                weekly_remaining_percent: Some(0),
                monthly_remaining_percent: None,
                credits_left: None,
            })
        );
    }

    #[test]
    fn oversized_percentages_capture_the_full_integer_before_clamping() {
        assert_eq!(
            parse_usage("Weekly limit 1000% used"),
            Some(GrokUsage {
                weekly_remaining_percent: Some(0),
                monthly_remaining_percent: None,
                credits_left: None,
            })
        );
        assert_eq!(
            parse_usage("Weekly limit 184467440737095516160% used"),
            None
        );
    }
}
