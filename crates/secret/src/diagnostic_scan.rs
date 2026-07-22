//! Bounded secret-like scanner for sanitized diagnostics and log artifacts.
//!
//! This is a last-line validation gate, not a replacement for typed secrets or redaction. The
//! report intentionally contains only low-cardinality categories, counts, and a truncation bit;
//! matched bytes and their positions never leave this module.

/// Maximum artifact size accepted by one production scan.
///
/// Oversized input is rejected before inspecting even a prefix. The returned report is marked
/// truncated and therefore fails [`DiagnosticScanReport::is_safe`]. Callers scanning larger files
/// must divide them into independently bounded artifacts with explicit overlap at their layer.
pub const DIAGNOSTIC_SCAN_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of matches retained by one production scan.
///
/// The scanner probes for one additional match to distinguish exactly-full reports from truncated
/// reports, but never exposes a count above this ceiling.
pub const DIAGNOSTIC_SCAN_MAX_FINDINGS: usize = 64;

const FINDING_KIND_COUNT: usize = 5;

/// Stable, low-cardinality secret-like diagnostic categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticFindingKind {
    BearerAuthorization,
    SensitiveJsonValue,
    SensitiveAssignment,
    ProviderToken,
    PrivateKeyMaterial,
}

impl DiagnosticFindingKind {
    const ALL: [Self; FINDING_KIND_COUNT] = [
        Self::BearerAuthorization,
        Self::SensitiveJsonValue,
        Self::SensitiveAssignment,
        Self::ProviderToken,
        Self::PrivateKeyMaterial,
    ];

    const fn index(self) -> usize {
        match self {
            Self::BearerAuthorization => 0,
            Self::SensitiveJsonValue => 1,
            Self::SensitiveAssignment => 2,
            Self::ProviderToken => 3,
            Self::PrivateKeyMaterial => 4,
        }
    }
}

/// Count for one stable finding category. No matched text or location is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticFindingCount {
    pub kind: DiagnosticFindingKind,
    pub count: usize,
}

/// Bounded scan result containing no source bytes, paths, keys, values, or offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct DiagnosticScanReport {
    findings: Vec<DiagnosticFindingCount>,
    truncated: bool,
}

impl DiagnosticScanReport {
    pub fn findings(&self) -> &[DiagnosticFindingCount] {
        &self.findings
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// A truncated report is always unsafe, including the oversized pre-scan rejection case.
    pub fn is_safe(&self) -> bool {
        !self.truncated && self.findings.is_empty()
    }

    pub fn count(&self, kind: DiagnosticFindingKind) -> usize {
        self.findings
            .iter()
            .find(|finding| finding.kind == kind)
            .map_or(0, |finding| finding.count)
    }
}

/// Scan one bounded diagnostic artifact without retaining source text or match locations.
///
/// Input larger than [`DIAGNOSTIC_SCAN_MAX_INPUT_BYTES`] is rejected before scanning and yields an
/// empty, truncated report. Consumers must treat [`DiagnosticScanReport::is_safe`] as the gate.
pub fn scan_diagnostic_bytes(input: &[u8]) -> DiagnosticScanReport {
    if input.len() > DIAGNOSTIC_SCAN_MAX_INPUT_BYTES {
        return DiagnosticScanReport {
            findings: Vec::new(),
            truncated: true,
        };
    }

    let mut accumulator = FindingAccumulator::default();
    let truncated = scan_bearer_authorization(input, &mut accumulator)
        || scan_sensitive_json_values(input, &mut accumulator)
        || scan_sensitive_assignments(input, &mut accumulator)
        || scan_provider_tokens(input, &mut accumulator)
        || scan_private_key_markers(input, &mut accumulator);
    accumulator.finish(truncated)
}

#[derive(Default)]
struct FindingAccumulator {
    counts: [usize; FINDING_KIND_COUNT],
    total: usize,
}

impl FindingAccumulator {
    /// Returns false for the limit-plus-one match without retaining it.
    fn record(&mut self, kind: DiagnosticFindingKind) -> bool {
        if self.total >= DIAGNOSTIC_SCAN_MAX_FINDINGS {
            return false;
        }
        let index = kind.index();
        self.counts[index] += 1;
        self.total += 1;
        true
    }

    fn finish(self, truncated: bool) -> DiagnosticScanReport {
        let findings = DiagnosticFindingKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let count = self.counts[kind.index()];
                (count > 0).then_some(DiagnosticFindingCount { kind, count })
            })
            .collect();
        DiagnosticScanReport {
            findings,
            truncated,
        }
    }
}

fn scan_bearer_authorization(input: &[u8], findings: &mut FindingAccumulator) -> bool {
    let mut index = 0;
    while index < input.len() {
        let Some(end) = token_end(input, index) else {
            index += 1;
            continue;
        };
        if ascii_key_eq(&input[index..end], b"authorization") {
            let mut cursor = skip_ascii_space(input, end);
            if matches!(input.get(cursor), Some(b':' | b'=')) {
                cursor = skip_ascii_space(input, cursor + 1);
                let quote = input
                    .get(cursor)
                    .copied()
                    .filter(|byte| matches!(byte, b'\'' | b'"'));
                if quote.is_some() {
                    cursor += 1;
                }
                if starts_with_ascii_case(input, cursor, b"bearer")
                    && !input.get(cursor + 6).is_some_and(|byte| is_key_byte(*byte))
                {
                    cursor += 6;
                    let after_scheme = skip_ascii_space(input, cursor);
                    if after_scheme > cursor {
                        let value_end = scan_value_end(input, after_scheme, quote);
                        if secret_value_present(&input[after_scheme..value_end])
                            && !findings.record(DiagnosticFindingKind::BearerAuthorization)
                        {
                            return true;
                        }
                        index = value_end.max(end);
                        continue;
                    }
                }
            }
        }
        index = end.max(index + 1);
    }
    false
}

fn scan_sensitive_json_values(input: &[u8], findings: &mut FindingAccumulator) -> bool {
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'"' {
            index += 1;
            continue;
        }
        let Some(key_end) = json_string_end(input, index + 1) else {
            break;
        };
        let key = &input[index + 1..key_end];
        let mut cursor = skip_ascii_space(input, key_end + 1);
        if is_sensitive_key(key) && input.get(cursor) == Some(&b':') {
            cursor = skip_ascii_space(input, cursor + 1);
            if json_secret_value_present(input, cursor)
                && !findings.record(DiagnosticFindingKind::SensitiveJsonValue)
            {
                return true;
            }
        }
        index = key_end + 1;
    }
    false
}

fn scan_sensitive_assignments(input: &[u8], findings: &mut FindingAccumulator) -> bool {
    let mut index = 0;
    while index < input.len() {
        let Some(end) = token_end(input, index) else {
            index += 1;
            continue;
        };
        if is_sensitive_key(&input[index..end]) {
            let mut cursor = skip_ascii_space(input, end);
            if input.get(cursor) == Some(&b'=') {
                cursor = skip_ascii_space(input, cursor + 1);
                let quote = input
                    .get(cursor)
                    .copied()
                    .filter(|byte| matches!(byte, b'\'' | b'"'));
                if quote.is_some() {
                    cursor += 1;
                }
                let value_end = scan_value_end(input, cursor, quote);
                if secret_value_present(&input[cursor..value_end])
                    && !findings.record(DiagnosticFindingKind::SensitiveAssignment)
                {
                    return true;
                }
                index = value_end.max(end);
                continue;
            }
        }
        index = end.max(index + 1);
    }
    false
}

fn scan_provider_tokens(input: &[u8], findings: &mut FindingAccumulator) -> bool {
    const PREFIXES: [(&[u8], usize); 15] = [
        (b"github_pat_", 30),
        (b"sk-ant-", 24),
        (b"sk-proj-", 24),
        (b"sk_live_", 24),
        (b"rk_live_", 24),
        (b"ghp_", 20),
        (b"gho_", 20),
        (b"ghu_", 20),
        (b"ghs_", 20),
        (b"ghr_", 20),
        (b"xoxb-", 20),
        (b"xoxp-", 20),
        (b"xoxa-", 20),
        (b"xoxr-", 20),
        (b"xapp-", 20),
    ];

    let mut index = 0;
    while index < input.len() {
        if index > 0 && is_provider_token_byte(input[index - 1]) {
            index += 1;
            continue;
        }

        let mut matched_end = None;
        for (prefix, minimum_length) in PREFIXES {
            if input[index..].starts_with(prefix) {
                let end = provider_token_end(input, index);
                if end - index >= minimum_length {
                    matched_end = Some(end);
                    break;
                }
            }
        }
        if matched_end.is_none() && input[index..].starts_with(b"sk-") {
            let end = provider_token_end(input, index);
            if end - index >= 24 {
                matched_end = Some(end);
            }
        }
        if matched_end.is_none() && input[index..].starts_with(b"AIza") {
            let end = provider_token_end(input, index);
            if end - index >= 35 {
                matched_end = Some(end);
            }
        }
        if matched_end.is_none() && input[index..].starts_with(b"hf_") {
            let end = provider_token_end(input, index);
            let suffix = &input[index + 3..end];
            if end - index >= 32 && suffix.iter().all(u8::is_ascii_alphanumeric) {
                matched_end = Some(end);
            }
        }
        if matched_end.is_none()
            && (input[index..].starts_with(b"AKIA") || input[index..].starts_with(b"ASIA"))
        {
            let end = input[index..]
                .iter()
                .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                .count()
                + index;
            if end - index == 20 {
                matched_end = Some(end);
            }
        }

        if let Some(end) = matched_end {
            if !findings.record(DiagnosticFindingKind::ProviderToken) {
                return true;
            }
            index = end;
        } else {
            index += 1;
        }
    }
    false
}

fn scan_private_key_markers(input: &[u8], findings: &mut FindingAccumulator) -> bool {
    const MARKERS: [&[u8]; 5] = [
        b"-----BEGIN PRIVATE KEY-----",
        b"-----BEGIN ENCRYPTED PRIVATE KEY-----",
        b"-----BEGIN RSA PRIVATE KEY-----",
        b"-----BEGIN EC PRIVATE KEY-----",
        b"-----BEGIN OPENSSH PRIVATE KEY-----",
    ];
    let mut index = 0;
    while index < input.len() {
        let mut matched = None;
        for marker in MARKERS {
            if input[index..].starts_with(marker) {
                matched = Some(marker.len());
                break;
            }
        }
        if let Some(length) = matched {
            if !findings.record(DiagnosticFindingKind::PrivateKeyMaterial) {
                return true;
            }
            index += length;
        } else {
            index += 1;
        }
    }
    false
}

fn token_end(input: &[u8], start: usize) -> Option<usize> {
    if !input.get(start).is_some_and(|byte| is_key_byte(*byte))
        || start > 0 && is_key_byte(input[start - 1])
    {
        return None;
    }
    let mut end = start + 1;
    while input.get(end).is_some_and(|byte| is_key_byte(*byte)) {
        end += 1;
    }
    Some(end)
}

fn is_sensitive_key(candidate: &[u8]) -> bool {
    const KEYS: [&[u8]; 10] = [
        b"access_token",
        b"refresh_token",
        b"id_token",
        b"client_secret",
        b"client_assertion",
        b"api_key",
        b"api_token",
        b"auth_token",
        b"private_key",
        b"password",
    ];
    KEYS.into_iter().any(|key| ascii_key_eq(candidate, key))
}

fn ascii_key_eq(candidate: &[u8], expected: &[u8]) -> bool {
    candidate.len() == expected.len()
        && candidate.iter().zip(expected).all(|(left, right)| {
            let left = if *left == b'-' { b'_' } else { *left };
            left.eq_ignore_ascii_case(right)
        })
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn starts_with_ascii_case(input: &[u8], start: usize, expected: &[u8]) -> bool {
    input
        .get(start..start.saturating_add(expected.len()))
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(expected))
}

fn skip_ascii_space(input: &[u8], mut index: usize) -> usize {
    while input
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        index += 1;
    }
    index
}

fn json_string_end(input: &[u8], mut index: usize) -> Option<usize> {
    let mut escaped = false;
    while let Some(byte) = input.get(index).copied() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn json_secret_value_present(input: &[u8], start: usize) -> bool {
    let Some(first) = input.get(start).copied() else {
        return false;
    };
    if first == b'"' {
        let end = json_string_end(input, start + 1).unwrap_or(input.len());
        return secret_value_present(&input[start + 1..end]);
    }
    let end = input[start..]
        .iter()
        .position(|byte| matches!(byte, b',' | b'}' | b']' | b'\r' | b'\n'))
        .map_or(input.len(), |offset| start + offset);
    secret_value_present(trim_ascii_space(&input[start..end]))
}

fn scan_value_end(input: &[u8], start: usize, quote: Option<u8>) -> usize {
    let mut index = start;
    while let Some(byte) = input.get(index).copied() {
        if quote.is_some_and(|quote| byte == quote)
            || quote.is_none()
                && matches!(
                    byte,
                    b' ' | b'\t' | b'\r' | b'\n' | b'&' | b';' | b',' | b'#'
                )
        {
            break;
        }
        index += 1;
    }
    index
}

fn trim_ascii_space(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn secret_value_present(value: &[u8]) -> bool {
    let value = trim_ascii_space(value);
    if value.is_empty()
        || value
            .iter()
            .all(|byte| matches!(byte, b'*' | b'x' | b'X' | b'-'))
    {
        return false;
    }
    const SAFE_PLACEHOLDERS: [&[u8]; 8] = [
        b"null",
        b"none",
        b"redacted",
        b"[redacted]",
        b"<redacted>",
        b"masked",
        b"[masked]",
        b"<masked>",
    ];
    !SAFE_PLACEHOLDERS
        .into_iter()
        .any(|placeholder| value.eq_ignore_ascii_case(placeholder))
}

fn is_provider_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn provider_token_end(input: &[u8], mut index: usize) -> usize {
    while input
        .get(index)
        .is_some_and(|byte| is_provider_token_byte(*byte))
    {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding_count(report: &DiagnosticScanReport, kind: DiagnosticFindingKind) -> usize {
        report.count(kind)
    }

    #[test]
    fn detects_each_supported_secret_like_category() {
        let artifact = br#"
Authorization: Bearer opaque-bearer-value-123
{"access_token":"json-value-123", "client_secret": "another-json-value"}
?refresh_token=query-value-123&safe=value
API_KEY='assignment-value-123'
provider=ghp_1234567890abcdefghijklmnop
-----BEGIN OPENSSH PRIVATE KEY-----
"#;
        let report = scan_diagnostic_bytes(artifact);

        assert!(!report.is_safe());
        assert!(!report.truncated());
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::BearerAuthorization),
            1
        );
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::SensitiveJsonValue),
            2
        );
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::SensitiveAssignment),
            2
        );
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::ProviderToken),
            1
        );
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::PrivateKeyMaterial),
            1
        );
    }

    #[test]
    fn exact_key_boundaries_and_safe_placeholders_avoid_common_false_positives() {
        let artifact = br#"
{"has_access_token":true,"access_token":null,"client_secret":"[REDACTED]"}
not_access_token=value access_token_suffix=value
Authorization: Basic public-user-value
This sentence discusses Bearer authentication and -----BEGIN PUBLIC KEY-----.
sk-short hf_documentation_identifier
"#;
        let report = scan_diagnostic_bytes(artifact);

        assert!(report.is_safe(), "{report:?}");
        assert!(report.findings().is_empty());
        assert!(!report.truncated());
    }

    #[test]
    fn key_normalization_accepts_case_and_hyphen_but_not_embedded_names() {
        let artifact = b"CLIENT-SECRET=value-123 prefixCLIENT_SECRET=value ACCESS_TOKEN_SUFFIX=x";
        let report = scan_diagnostic_bytes(artifact);
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::SensitiveAssignment),
            1
        );
    }

    #[test]
    fn provider_prefix_requires_a_token_boundary_and_minimum_length() {
        let artifact =
            b"prefixghp_1234567890abcdefghijklmnop ghp_short ghp_1234567890abcdefghijklmnop";
        let report = scan_diagnostic_bytes(artifact);
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::ProviderToken),
            1
        );
    }

    #[test]
    fn common_provider_prefix_families_are_recognized_without_raw_values() {
        let artifact = b"\
sk-proj-1234567890abcdefghijklmnop \
github_pat_1234567890abcdefghijklmnopqrstuv \
xoxb-1234567890abcdefghijklmnop \
sk_live_1234567890abcdefghijklmnop \
hf_1234567890abcdefghijklmnopqrstuv \
AIza1234567890abcdefghijklmnopqrstuv \
AKIA1234567890ABCDEF";
        let report = scan_diagnostic_bytes(artifact);
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::ProviderToken),
            7
        );
    }

    #[test]
    fn oversized_input_is_rejected_before_scanning_and_fails_closed() {
        let mut artifact = vec![b'x'; DIAGNOSTIC_SCAN_MAX_INPUT_BYTES + 1];
        let marker = b"API_KEY=must-not-be-partial-123";
        artifact[..marker.len()].copy_from_slice(marker);
        let report = scan_diagnostic_bytes(&artifact);

        assert!(report.truncated());
        assert!(!report.is_safe());
        assert!(report.findings().is_empty());
    }

    #[test]
    fn finding_count_is_bounded_and_limit_plus_one_marks_truncated() {
        let mut artifact = Vec::new();
        for index in 0..DIAGNOSTIC_SCAN_MAX_FINDINGS {
            artifact.extend_from_slice(format!("api_key=value-{index:03}-secret\n").as_bytes());
        }
        let exact = scan_diagnostic_bytes(&artifact);
        assert!(!exact.truncated());
        assert_eq!(
            finding_count(&exact, DiagnosticFindingKind::SensitiveAssignment),
            DIAGNOSTIC_SCAN_MAX_FINDINGS
        );

        artifact.extend_from_slice(b"api_key=limit-plus-one-secret\n");
        let report = scan_diagnostic_bytes(&artifact);

        assert!(report.truncated());
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::SensitiveAssignment),
            DIAGNOSTIC_SCAN_MAX_FINDINGS
        );
        assert_eq!(
            report
                .findings()
                .iter()
                .map(|finding| finding.count)
                .sum::<usize>(),
            DIAGNOSTIC_SCAN_MAX_FINDINGS
        );
    }

    #[test]
    fn report_debug_never_contains_source_text_or_location() {
        const MARKER: &str = "unique-diagnostic-secret-marker-7391";
        let artifact = format!("Authorization: Bearer {MARKER}");
        let report = scan_diagnostic_bytes(artifact.as_bytes());
        let debug = format!("{report:?}");

        assert!(!debug.contains(MARKER));
        assert!(!debug.contains("unique-diagnostic"));
        assert!(!debug.contains("offset"));
        assert!(!debug.contains("path"));
        assert!(debug.contains("BearerAuthorization"));
    }

    #[test]
    fn invalid_utf8_is_scanned_without_materializing_source_text() {
        let artifact = b"\xff\xfe api_token=opaque-value-123 \x80";
        let report = scan_diagnostic_bytes(artifact);
        assert_eq!(
            finding_count(&report, DiagnosticFindingKind::SensitiveAssignment),
            1
        );
    }
}
