//! RedactionService (설계문서 7장). 등록 기반 streaming redaction —
//! chunk 경계 분할·ANSI escape 삽입·base64/URL/JSON-escape 변형에 대응한다.
//! 패턴 corpus 확장·encrypted raw log는 PR-22.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::SecretString;

const REPLACEMENT: &[u8] = b"[REDACTED]";
/// 치환 오탐을 피하기 위한 최소 등록 길이 (이보다 짧은 값은 힌트 수준)
const MIN_SECRET_LEN: usize = 6;
const DEFAULT_GRACE: Duration = Duration::from_secs(30);
const MAX_JSON_DEPTH: usize = 64;

#[cfg(test)]
thread_local! {
    static REDACT_BUFFER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactionCorpusLimits {
    pub max_items: usize,
    pub max_bytes: usize,
}

impl RedactionCorpusLimits {
    pub const PRODUCTION: Self = Self {
        max_items: 4_096,
        max_bytes: 4 * 1024 * 1024,
    };

    fn validate(self) -> Result<Self, RedactionCapacityError> {
        if self.max_items == 0 || self.max_bytes == 0 {
            return Err(RedactionCapacityError::InvalidLimits);
        }
        Ok(self)
    }
}

impl Default for RedactionCorpusLimits {
    fn default() -> Self {
        Self::PRODUCTION
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedactionCorpusStats {
    pub items: usize,
    pub bytes: usize,
    pub permanent_items: usize,
    pub rotating_items: usize,
    pub active_leases: usize,
    pub fail_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionCapacityError {
    InvalidLimits,
    NoSecrets,
    SecretTooShort { length: usize, minimum: usize },
    SecretTooLarge { length: usize, maximum: usize },
    ItemLimit { requested: usize, maximum: usize },
    ByteLimit { requested: usize, maximum: usize },
    JsonDepthLimit { requested: usize, maximum: usize },
    JsonStringLimit { requested: usize, maximum: usize },
    InvalidStructuredJson,
    LegacyRegistrationFailed,
}

impl std::fmt::Display for RedactionCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("redaction corpus limits must be non-zero"),
            Self::NoSecrets => f.write_str("secret-backed execution has no redaction inputs"),
            Self::SecretTooShort { length, minimum } => {
                write!(f, "secret length {length} is below safe minimum {minimum}")
            }
            Self::SecretTooLarge { length, maximum } => {
                write!(
                    f,
                    "secret length {length} exceeds redaction input maximum {maximum}"
                )
            }
            Self::ItemLimit { requested, maximum } => {
                write!(
                    f,
                    "redaction corpus needs {requested} items (maximum {maximum})"
                )
            }
            Self::ByteLimit { requested, maximum } => {
                write!(
                    f,
                    "redaction corpus needs {requested} bytes (maximum {maximum})"
                )
            }
            Self::JsonDepthLimit { requested, maximum } => {
                write!(
                    f,
                    "secret JSON nesting depth {requested} exceeds maximum {maximum}"
                )
            }
            Self::JsonStringLimit { requested, maximum } => {
                write!(
                    f,
                    "secret JSON contains {requested} strings (maximum {maximum})"
                )
            }
            Self::InvalidStructuredJson => {
                f.write_str("structured secret is not valid bounded JSON")
            }
            Self::LegacyRegistrationFailed => {
                f.write_str("a legacy redaction registration failed closed")
            }
        }
    }
}

impl std::error::Error for RedactionCapacityError {}

pub trait RedactionClock: Send + Sync {
    fn now(&self) -> Duration;
}

struct MonotonicClock {
    origin: Instant,
}

impl RedactionClock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// 프로세스 전역 secret 패턴 레지스트리. 등록은 UI/worker 어디서든,
/// 매칭은 StreamRedactor가 수행한다.
#[derive(Clone)]
pub struct RedactionService {
    inner: Arc<RedactionInner>,
}

struct RedactionInner {
    patterns: Mutex<Patterns>,
    limits: RedactionCorpusLimits,
    grace: Duration,
    clock: Arc<dyn RedactionClock>,
}

#[derive(Default)]
struct Patterns {
    entries: Vec<PatternEntry>,
    max_len: usize,
    bytes: usize,
    next_expiry: Option<Duration>,
    next_id: u64,
    active_leases: usize,
    fail_closed: bool,
    #[cfg(test)]
    prune_scans: usize,
}

struct PatternEntry {
    id: u64,
    bytes: PatternBytes,
    permanent: bool,
    rotating_refs: usize,
    expires_at: Option<Duration>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PatternBytes(Vec<u8>);

impl PatternBytes {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PatternBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: `byte` is an exclusively borrowed byte in the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl Patterns {
    fn prune(&mut self, now: Duration) {
        // Matching is a hot path. Retain/recompute/sort only when the earliest grace deadline is
        // actually due; registration and lease acquisition also enter through this same gate.
        if self.next_expiry.is_none_or(|expiry| expiry > now) {
            return;
        }
        #[cfg(test)]
        {
            self.prune_scans = self.prune_scans.saturating_add(1);
        }
        self.entries.retain(|entry| {
            entry.permanent
                || entry.rotating_refs > 0
                || entry.expires_at.is_some_and(|expires| expires > now)
        });
        self.recompute();
    }

    fn recompute(&mut self) {
        self.bytes = self.entries.iter().map(|entry| entry.bytes.len()).sum();
        self.max_len = self
            .entries
            .iter()
            .map(|entry| entry.bytes.len())
            .max()
            .unwrap_or(0);
        self.next_expiry = self
            .entries
            .iter()
            .filter(|entry| !entry.permanent && entry.rotating_refs == 0)
            .filter_map(|entry| entry.expires_at)
            .min();
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.bytes.len()));
    }

    fn stats(&self) -> RedactionCorpusStats {
        RedactionCorpusStats {
            items: self.entries.len(),
            bytes: self.bytes,
            permanent_items: self.entries.iter().filter(|entry| entry.permanent).count(),
            rotating_items: self.entries.iter().filter(|entry| !entry.permanent).count(),
            active_leases: self.active_leases,
            fail_closed: self.fail_closed,
        }
    }
}

impl RedactionService {
    pub fn new() -> Self {
        Self::with_clock(
            RedactionCorpusLimits::default(),
            DEFAULT_GRACE,
            Arc::new(MonotonicClock {
                origin: Instant::now(),
            }),
        )
        .expect("production redaction limits are valid")
    }

    pub fn with_clock(
        limits: RedactionCorpusLimits,
        grace: Duration,
        clock: Arc<dyn RedactionClock>,
    ) -> Result<Self, RedactionCapacityError> {
        Ok(Self {
            inner: Arc::new(RedactionInner {
                patterns: Mutex::new(Patterns::default()),
                limits: limits.validate()?,
                grace,
                clock,
            }),
        })
    }

    /// secret과 그 변형(base64, URL-encoded/form-encoded, JSON-escaped)을
    /// 등록한다 (설계문서 7장, corpus는 PR-22에서 확장).
    pub fn register(&self, secret: &SecretString) {
        // Preserve the legacy API's historical short-value behavior. Secret-backed execution must
        // use the checked lease/permanent APIs below, which reject an unprotectable short value.
        if secret.expose().len() < MIN_SECRET_LEN {
            return;
        }
        if self.register_permanent(secret).is_err() {
            self.mark_fail_closed();
        }
    }

    pub fn register_permanent(&self, secret: &SecretString) -> Result<(), RedactionCapacityError> {
        let variants = self.variants_for(secret, true)?;
        let now = self.inner.clock.now();
        let mut patterns = self.inner.patterns.lock().expect("redaction patterns lock");
        patterns.prune(now);
        ensure_capacity(&patterns, &variants, self.inner.limits)?;
        for variant in variants {
            if let Some(entry) = patterns
                .entries
                .iter_mut()
                .find(|entry| entry.bytes == variant)
            {
                entry.permanent = true;
                entry.expires_at = None;
            } else {
                let id = patterns.next_id;
                patterns.next_id = patterns.next_id.saturating_add(1);
                patterns.entries.push(PatternEntry {
                    id,
                    bytes: variant,
                    permanent: true,
                    rotating_refs: 0,
                    expires_at: None,
                });
            }
        }
        patterns.recompute();
        Ok(())
    }

    /// Atomically registers every secret needed by one secret-backed operation. Failure leaves the
    /// corpus unchanged and the caller must not execute the operation.
    pub fn acquire_execution_lease(
        &self,
        secrets: &[&SecretString],
    ) -> Result<RedactionLease, RedactionCapacityError> {
        if secrets.is_empty() {
            return Err(RedactionCapacityError::NoSecrets);
        }
        let mut variants = BTreeSet::new();
        let mut variant_bytes = 0usize;
        let mut seen_secrets = BTreeSet::new();
        for secret in secrets {
            if !seen_secrets.insert(secret.expose()) {
                continue;
            }
            self.append_variants(secret, &mut variants, &mut variant_bytes)?;
        }
        self.acquire_prepared_variants(variants)
    }

    /// Acquires one checked, rotating lease for the complete secret inputs and every nested JSON
    /// string value they contain.
    ///
    /// A secret beginning with an object, array, or JSON string marker is treated as structured
    /// input and must be valid JSON within the fixed nesting and corpus-derived item limits. Other
    /// inputs have exactly the same behavior as [`Self::acquire_execution_lease`]. Parsing,
    /// variant expansion, deduplication, and capacity checks all finish before the corpus lock is
    /// mutated, so callers can fail closed with no partial registration.
    pub fn acquire_json_execution_lease(
        &self,
        secrets: &[&SecretString],
    ) -> Result<RedactionLease, RedactionCapacityError> {
        if secrets.is_empty() {
            return Err(RedactionCapacityError::NoSecrets);
        }

        let mut variants = BTreeSet::new();
        let mut variant_bytes = 0usize;
        let mut seen_secrets = BTreeSet::new();
        for secret in secrets {
            if !seen_secrets.insert(secret.expose()) {
                continue;
            }
            self.append_variants(secret, &mut variants, &mut variant_bytes)?;
            if let Some(nested) =
                extract_bounded_json_string_values(secret.expose(), self.inner.limits.max_items)?
            {
                for nested_secret in &nested {
                    self.append_variants(nested_secret, &mut variants, &mut variant_bytes)?;
                }
            }
        }

        self.acquire_prepared_variants(variants)
    }

    fn append_variants(
        &self,
        secret: &SecretString,
        variants: &mut BTreeSet<PatternBytes>,
        variant_bytes: &mut usize,
    ) -> Result<(), RedactionCapacityError> {
        for variant in self.variants_for(secret, true)? {
            if variants.contains(&variant) {
                continue;
            }
            let requested_items = variants.len().saturating_add(1);
            if requested_items > self.inner.limits.max_items {
                return Err(RedactionCapacityError::ItemLimit {
                    requested: requested_items,
                    maximum: self.inner.limits.max_items,
                });
            }
            let requested_bytes = variant_bytes.saturating_add(variant.len());
            if requested_bytes > self.inner.limits.max_bytes {
                return Err(RedactionCapacityError::ByteLimit {
                    requested: requested_bytes,
                    maximum: self.inner.limits.max_bytes,
                });
            }
            *variant_bytes = requested_bytes;
            variants.insert(variant);
        }
        Ok(())
    }

    fn acquire_prepared_variants(
        &self,
        variants: BTreeSet<PatternBytes>,
    ) -> Result<RedactionLease, RedactionCapacityError> {
        let now = self.inner.clock.now();
        let mut patterns = self.inner.patterns.lock().expect("redaction patterns lock");
        patterns.prune(now);
        ensure_capacity(&patterns, &variants, self.inner.limits)?;
        let mut pattern_ids = Vec::with_capacity(variants.len());
        for variant in variants {
            if let Some(entry) = patterns
                .entries
                .iter_mut()
                .find(|entry| entry.bytes == variant)
            {
                entry.rotating_refs = entry.rotating_refs.saturating_add(1);
                entry.expires_at = None;
                pattern_ids.push(entry.id);
            } else {
                let id = patterns.next_id;
                patterns.next_id = patterns.next_id.saturating_add(1);
                patterns.entries.push(PatternEntry {
                    id,
                    bytes: variant,
                    permanent: false,
                    rotating_refs: 1,
                    expires_at: None,
                });
                pattern_ids.push(id);
            }
        }
        patterns.active_leases = patterns.active_leases.saturating_add(1);
        patterns.recompute();
        Ok(RedactionLease {
            service: self.clone(),
            pattern_ids,
            released: false,
        })
    }

    pub fn acquire_rotating(
        &self,
        secret: &SecretString,
    ) -> Result<RedactionLease, RedactionCapacityError> {
        self.acquire_execution_lease(&[secret])
    }

    pub fn corpus_stats(&self) -> RedactionCorpusStats {
        let now = self.inner.clock.now();
        let mut patterns = self.inner.patterns.lock().expect("redaction patterns lock");
        patterns.prune(now);
        patterns.stats()
    }

    pub fn ensure_safe(&self) -> Result<(), RedactionCapacityError> {
        if self
            .inner
            .patterns
            .lock()
            .expect("redaction patterns lock")
            .fail_closed
        {
            Err(RedactionCapacityError::LegacyRegistrationFailed)
        } else {
            Ok(())
        }
    }

    fn mark_fail_closed(&self) {
        self.inner
            .patterns
            .lock()
            .expect("redaction patterns lock")
            .fail_closed = true;
    }

    fn maximum_input_len(&self) -> usize {
        (self.inner.limits.max_bytes / 128).max(MIN_SECRET_LEN)
    }

    fn variants_for(
        &self,
        secret: &SecretString,
        require_minimum: bool,
    ) -> Result<Vec<PatternBytes>, RedactionCapacityError> {
        let plain = secret.expose().as_bytes();
        if plain.len() < MIN_SECRET_LEN {
            return if require_minimum {
                Err(RedactionCapacityError::SecretTooShort {
                    length: plain.len(),
                    minimum: MIN_SECRET_LEN,
                })
            } else {
                Ok(Vec::new())
            };
        }
        let maximum = self.maximum_input_len();
        if plain.len() > maximum {
            return Err(RedactionCapacityError::SecretTooLarge {
                length: plain.len(),
                maximum,
            });
        }
        let mut variants: Vec<PatternBytes> = vec![
            PatternBytes(plain.to_vec()),
            PatternBytes(base64_encode(plain).into_bytes()),
            PatternBytes(url_encode(plain, false).into_bytes()),
            PatternBytes(url_encode(plain, true).into_bytes()), // 소문자 %xx 인코더 대응
            PatternBytes(json_escape(secret.expose()).into_bytes()),
            // \uXXXX 스타일 직렬화 대응 — hex 대·소문자 각각 (codex 리뷰)
            PatternBytes(json_escape_unicode(secret.expose(), false).into_bytes()),
            PatternBytes(json_escape_unicode(secret.expose(), true).into_bytes()),
        ];
        // 스페이스를 +로 쓰는 form 인코더 대응. 방언마다 safe set이 다르다 —
        // URLSearchParams(*safe/~enc), python·go quote_plus(~safe/*enc) 등 —
        // 조합 전부를 등록한다 (dedup으로 실제 다른 것만 남는다. codex 리뷰 2건)
        for tilde_safe in [false, true] {
            for star_safe in [false, true] {
                for lowercase in [false, true] {
                    variants.push(PatternBytes(
                        form_encode(plain, lowercase, tilde_safe, star_safe).into_bytes(),
                    ));
                }
            }
        }
        variants.sort();
        variants.dedup();
        variants.retain(|variant| variant.len() >= MIN_SECRET_LEN);
        Ok(variants)
    }

    /// secret 값이 JSON이면(예: OAuth 토큰 blob — PR-18) 안의 문자열 필드들을
    /// 개별 패턴으로도 등록한다. 로그에는 blob 전체가 아니라 access token 같은
    /// 개별 값이 찍히기 때문. JSON이 아니면 아무것도 하지 않는다 (register와 병행 사용).
    pub fn register_json_fields(&self, secret: &SecretString) {
        // Bound temporary serde allocations as well as the retained corpus. The legacy void API
        // cannot report capacity, so an oversized blob switches matching to fail-closed output.
        if secret.expose().len() > self.maximum_input_len() {
            self.mark_fail_closed();
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(secret.expose()) else {
            return;
        };
        fn walk(value: serde_json::Value, service: &RedactionService) {
            match value {
                serde_json::Value::String(s) => {
                    // Move the parsed allocation into SecretString so its Drop zeroizes it.
                    service.register(&SecretString::new(s));
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, service);
                    }
                }
                serde_json::Value::Object(map) => {
                    for item in map.into_values() {
                        walk(item, service);
                    }
                }
                _ => {}
            }
        }
        walk(value, self);
    }

    pub fn stream_redactor(&self) -> StreamRedactor {
        StreamRedactor {
            service: self.clone(),
            carry: Vec::new(),
        }
    }
}

fn extract_bounded_json_string_values(
    input: &str,
    max_strings: usize,
) -> Result<Option<Vec<SecretString>>, RedactionCapacityError> {
    let Some(first) = input.bytes().find(|byte| !byte.is_ascii_whitespace()) else {
        return Ok(None);
    };
    if !matches!(first, b'{' | b'[' | b'"') {
        return Ok(None);
    }

    let mut parser = BoundedJsonParser {
        input: input.as_bytes(),
        position: 0,
        max_strings,
        string_count: 0,
        values: Vec::new(),
    };
    parser.parse_document()?;
    Ok(Some(parser.values))
}

struct BoundedJsonParser<'a> {
    input: &'a [u8],
    position: usize,
    max_strings: usize,
    string_count: usize,
    values: Vec<SecretString>,
}

impl BoundedJsonParser<'_> {
    fn parse_document(&mut self) -> Result<(), RedactionCapacityError> {
        self.skip_whitespace();
        self.parse_value(0)?;
        self.skip_whitespace();
        if self.position == self.input.len() {
            Ok(())
        } else {
            Err(RedactionCapacityError::InvalidStructuredJson)
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), RedactionCapacityError> {
        self.skip_whitespace();
        match self.input.get(self.position).copied() {
            Some(b'{') => self.parse_object(depth.saturating_add(1)),
            Some(b'[') => self.parse_array(depth.saturating_add(1)),
            Some(b'"') => {
                if let Some(value) = self.parse_string(true)? {
                    self.values.push(value);
                }
                Ok(())
            }
            Some(b't') => self.consume_literal(b"true"),
            Some(b'f') => self.consume_literal(b"false"),
            Some(b'n') => self.consume_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(RedactionCapacityError::InvalidStructuredJson),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), RedactionCapacityError> {
        self.ensure_depth(depth)?;
        self.position = self.position.saturating_add(1);
        self.skip_whitespace();
        if self.consume_byte(b'}') {
            return Ok(());
        }

        loop {
            if self.input.get(self.position) != Some(&b'"') {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            self.parse_string(false)?;
            self.skip_whitespace();
            if !self.consume_byte(b':') {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            self.parse_value(depth)?;
            self.skip_whitespace();
            if self.consume_byte(b'}') {
                return Ok(());
            }
            if !self.consume_byte(b',') {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), RedactionCapacityError> {
        self.ensure_depth(depth)?;
        self.position = self.position.saturating_add(1);
        self.skip_whitespace();
        if self.consume_byte(b']') {
            return Ok(());
        }

        loop {
            self.parse_value(depth)?;
            self.skip_whitespace();
            if self.consume_byte(b']') {
                return Ok(());
            }
            if !self.consume_byte(b',') {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            self.skip_whitespace();
        }
    }

    fn parse_string(
        &mut self,
        capture: bool,
    ) -> Result<Option<SecretString>, RedactionCapacityError> {
        if !self.consume_byte(b'"') {
            return Err(RedactionCapacityError::InvalidStructuredJson);
        }
        self.string_count = self.string_count.saturating_add(1);
        if self.string_count > self.max_strings {
            return Err(RedactionCapacityError::JsonStringLimit {
                requested: self.string_count,
                maximum: self.max_strings,
            });
        }

        let mut output = capture.then(ZeroizingBytes::default);
        loop {
            let Some(byte) = self.input.get(self.position).copied() else {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            };
            self.position = self.position.saturating_add(1);
            match byte {
                b'"' => return Ok(output.map(ZeroizingBytes::into_secret)),
                0x00..=0x1f => return Err(RedactionCapacityError::InvalidStructuredJson),
                b'\\' => {
                    let Some(escaped) = self.input.get(self.position).copied() else {
                        return Err(RedactionCapacityError::InvalidStructuredJson);
                    };
                    self.position = self.position.saturating_add(1);
                    match escaped {
                        b'"' | b'\\' | b'/' => {
                            if let Some(output) = &mut output {
                                output.0.push(escaped);
                            }
                        }
                        b'b' => push_if_captured(&mut output, b'\x08'),
                        b'f' => push_if_captured(&mut output, b'\x0c'),
                        b'n' => push_if_captured(&mut output, b'\n'),
                        b'r' => push_if_captured(&mut output, b'\r'),
                        b't' => push_if_captured(&mut output, b'\t'),
                        b'u' => {
                            let scalar = self.parse_unicode_scalar()?;
                            if let Some(output) = &mut output {
                                output.push_scalar(scalar);
                            }
                        }
                        _ => return Err(RedactionCapacityError::InvalidStructuredJson),
                    }
                }
                _ => {
                    if let Some(output) = &mut output {
                        output.0.push(byte);
                    }
                }
            }
        }
    }

    fn parse_unicode_scalar(&mut self) -> Result<char, RedactionCapacityError> {
        let high = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&high) {
            if self
                .input
                .get(self.position..self.position.saturating_add(2))
                != Some(b"\\u")
            {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            self.position = self.position.saturating_add(2);
            let low = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
            0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&high) {
            return Err(RedactionCapacityError::InvalidStructuredJson);
        } else {
            u32::from(high)
        };
        char::from_u32(scalar).ok_or(RedactionCapacityError::InvalidStructuredJson)
    }

    fn parse_hex_quad(&mut self) -> Result<u16, RedactionCapacityError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let Some(byte) = self.input.get(self.position).copied() else {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            };
            self.position = self.position.saturating_add(1);
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a') + 10,
                b'A'..=b'F' => u16::from(byte - b'A') + 10,
                _ => return Err(RedactionCapacityError::InvalidStructuredJson),
            };
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<(), RedactionCapacityError> {
        self.consume_byte(b'-');
        match self.input.get(self.position).copied() {
            Some(b'0') => self.position = self.position.saturating_add(1),
            Some(b'1'..=b'9') => {
                self.position = self.position.saturating_add(1);
                self.consume_digits();
            }
            _ => return Err(RedactionCapacityError::InvalidStructuredJson),
        }
        if self.consume_byte(b'.') && !self.consume_one_or_more_digits() {
            return Err(RedactionCapacityError::InvalidStructuredJson);
        }
        if matches!(self.input.get(self.position), Some(b'e' | b'E')) {
            self.position = self.position.saturating_add(1);
            if matches!(self.input.get(self.position), Some(b'+' | b'-')) {
                self.position = self.position.saturating_add(1);
            }
            if !self.consume_one_or_more_digits() {
                return Err(RedactionCapacityError::InvalidStructuredJson);
            }
        }
        Ok(())
    }

    fn consume_digits(&mut self) {
        while matches!(self.input.get(self.position), Some(b'0'..=b'9')) {
            self.position = self.position.saturating_add(1);
        }
    }

    fn consume_one_or_more_digits(&mut self) -> bool {
        let start = self.position;
        self.consume_digits();
        self.position > start
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), RedactionCapacityError> {
        if self
            .input
            .get(self.position..self.position.saturating_add(literal.len()))
            == Some(literal)
        {
            self.position = self.position.saturating_add(literal.len());
            Ok(())
        } else {
            Err(RedactionCapacityError::InvalidStructuredJson)
        }
    }

    fn ensure_depth(&self, depth: usize) -> Result<(), RedactionCapacityError> {
        if depth > MAX_JSON_DEPTH {
            Err(RedactionCapacityError::JsonDepthLimit {
                requested: depth,
                maximum: MAX_JSON_DEPTH,
            })
        } else {
            Ok(())
        }
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.input.get(self.position) == Some(&expected) {
            self.position = self.position.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.input.get(self.position),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            self.position = self.position.saturating_add(1);
        }
    }
}

#[derive(Default)]
struct ZeroizingBytes(Vec<u8>);

impl ZeroizingBytes {
    fn push_scalar(&mut self, scalar: char) {
        let mut encoded = [0u8; 4];
        self.0
            .extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
        for byte in &mut encoded {
            // SAFETY: `byte` is exclusively borrowed from the stack buffer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }

    fn into_secret(mut self) -> SecretString {
        let bytes = std::mem::take(&mut self.0);
        // SAFETY: raw bytes come from an input `str`; escape substitutions are ASCII or a valid
        // Unicode scalar encoded by `char::encode_utf8`.
        SecretString::new(unsafe { String::from_utf8_unchecked(bytes) })
    }
}

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: `byte` is exclusively borrowed from the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

fn push_if_captured(output: &mut Option<ZeroizingBytes>, byte: u8) {
    if let Some(output) = output {
        output.0.push(byte);
    }
}

impl Default for RedactionService {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RedactionLease {
    service: RedactionService,
    pattern_ids: Vec<u64>,
    released: bool,
}

impl std::fmt::Debug for RedactionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedactionLease(REDACTED)")
    }
}

impl RedactionLease {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        let now = self.service.inner.clock.now();
        let expires_at = now.saturating_add(self.service.inner.grace);
        let mut patterns = self
            .service
            .inner
            .patterns
            .lock()
            .expect("redaction patterns lock");
        let mut scheduled_expiry = false;
        for id in &self.pattern_ids {
            if let Some(entry) = patterns.entries.iter_mut().find(|entry| entry.id == *id) {
                entry.rotating_refs = entry.rotating_refs.saturating_sub(1);
                if entry.rotating_refs == 0 && !entry.permanent {
                    entry.expires_at = Some(expires_at);
                    scheduled_expiry = true;
                }
            }
        }
        if scheduled_expiry {
            patterns.next_expiry = Some(
                patterns
                    .next_expiry
                    .map_or(expires_at, |scheduled| scheduled.min(expires_at)),
            );
        }
        patterns.active_leases = patterns.active_leases.saturating_sub(1);
        self.released = true;
    }
}

impl Drop for RedactionLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl Drop for StreamRedactor {
    fn drop(&mut self) {
        for byte in &mut self.carry {
            // SAFETY: `byte` is an exclusively borrowed byte in the owned carry allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

fn ensure_capacity<'a>(
    patterns: &Patterns,
    variants: impl IntoIterator<Item = &'a PatternBytes>,
    limits: RedactionCorpusLimits,
) -> Result<(), RedactionCapacityError> {
    let mut new_items = 0usize;
    let mut new_bytes = 0usize;
    for variant in variants {
        if !patterns
            .entries
            .iter()
            .any(|entry| entry.bytes.as_slice() == variant.as_slice())
        {
            new_items = new_items.saturating_add(1);
            new_bytes = new_bytes.saturating_add(variant.len());
        }
    }
    let requested_items = patterns.entries.len().saturating_add(new_items);
    if requested_items > limits.max_items {
        return Err(RedactionCapacityError::ItemLimit {
            requested: requested_items,
            maximum: limits.max_items,
        });
    }
    let requested_bytes = patterns.bytes.saturating_add(new_bytes);
    if requested_bytes > limits.max_bytes {
        return Err(RedactionCapacityError::ByteLimit {
            requested: requested_bytes,
            maximum: limits.max_bytes,
        });
    }
    Ok(())
}

/// 세션 output 스트림 하나의 redaction 상태 (lookbehind carry 보유).
pub struct StreamRedactor {
    service: RedactionService,
    /// chunk 경계에 걸친 secret 대응 — 아직 배출하지 않은 원본 꼬리
    carry: Vec<u8>,
}

impl StreamRedactor {
    /// chunk를 redact해 배출 가능한 앞부분을 돌려준다.
    /// 꼬리(최대 secret 길이 − 1, stripped 기준)는 다음 chunk와 합쳐 재검사한다.
    pub fn redact_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        let (mut redacted, stripped_len, max_len) = {
            let now = self.service.inner.clock.now();
            let mut patterns = self
                .service
                .inner
                .patterns
                .lock()
                .expect("redaction patterns lock");
            patterns.prune(now);
            if patterns.entries.is_empty() && !patterns.fail_closed {
                return std::mem::take(&mut self.carry);
            }
            let (redacted, stripped_len) =
                redact_buffer(&self.carry, &patterns.entries, patterns.fail_closed);
            (redacted, stripped_len, patterns.max_len)
        };
        if max_len == 0 {
            self.carry.clear();
            return std::mem::take(&mut redacted);
        }
        // stripped 기준 max_len-1 만큼을 carry로 남긴다.
        // escape만 계속 오는 병리적 스트림으로 carry가 무한히 크지 않게 cap하되,
        // 등록된 가장 긴 secret은 반드시 담을 수 있어야 한다 (§7 lookbehind 규칙 —
        // cap이 max_len보다 작으면 긴 secret이 chunk 분할로 우회된다. codex 리뷰)
        let keep_stripped = max_len.saturating_sub(1);
        let carry_start = origin_index_for_stripped_suffix(&redacted, keep_stripped);
        const CARRY_CAP_FLOOR: usize = 16 * 1024;
        let carry_cap = CARRY_CAP_FLOOR.max(max_len * 2);
        let carry_start = carry_start.max(redacted.len().saturating_sub(carry_cap));
        let _ = stripped_len;
        self.carry = redacted.split_off(carry_start);
        redacted
    }

    /// 스트림 종료 — 남은 carry를 마지막 검사 후 배출한다.
    pub fn flush(&mut self) -> Vec<u8> {
        let now = self.service.inner.clock.now();
        let mut patterns = self
            .service
            .inner
            .patterns
            .lock()
            .expect("redaction patterns lock");
        patterns.prune(now);
        if patterns.entries.is_empty() && !patterns.fail_closed {
            return std::mem::take(&mut self.carry);
        }
        let (redacted, _) = redact_buffer(&self.carry, &patterns.entries, patterns.fail_closed);
        self.carry.clear();
        redacted
    }
}

/// buffer에서 패턴을 찾아 [REDACTED]로 치환한다.
/// 매칭은 ANSI escape를 제거한 텍스트에서 하고(escape 삽입 우회 방지 — 7장),
/// 치환은 원본 범위(escape 포함)에 적용한다. (stripped 길이도 반환)
fn redact_buffer(buffer: &[u8], patterns: &[PatternEntry], fail_closed: bool) -> (Vec<u8>, usize) {
    #[cfg(test)]
    REDACT_BUFFER_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

    let (stripped, index_map) = strip_ansi_with_map(buffer);
    if fail_closed && !stripped.is_empty() {
        return (REPLACEMENT.to_vec(), stripped.len());
    }
    if patterns.is_empty() || stripped.is_empty() {
        return (buffer.to_vec(), stripped.len());
    }
    // 원본 기준 (start, end) 치환 구간 수집
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for pattern in patterns {
        let pattern = pattern.bytes.as_slice();
        let mut from = 0;
        while let Some(pos) = find(&stripped[from..], pattern) {
            let start = from + pos;
            let end = start + pattern.len();
            // 원본 범위: 매치 첫 바이트의 원본 위치 ~ 마지막 바이트의 원본 위치+1
            spans.push((index_map[start], index_map[end - 1] + 1));
            from = end;
        }
    }
    if spans.is_empty() {
        return (buffer.to_vec(), stripped.len());
    }
    spans.sort_unstable();
    // 겹치는 구간 병합
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
            _ => merged.push(span),
        }
    }
    let mut out = Vec::with_capacity(buffer.len());
    let mut cursor = 0;
    for (start, end) in merged {
        out.extend_from_slice(&buffer[cursor..start]);
        out.extend_from_slice(REPLACEMENT);
        cursor = end;
    }
    out.extend_from_slice(&buffer[cursor..]);
    (out, stripped.len())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// ANSI escape(CSI/OSC/2byte)를 제거한 바이트열과, stripped 인덱스 → 원본 인덱스 맵.
fn strip_ansi_with_map(buffer: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut stripped = Vec::with_capacity(buffer.len());
    let mut map = Vec::with_capacity(buffer.len());
    let mut i = 0;
    while i < buffer.len() {
        if buffer[i] == 0x1b {
            match buffer.get(i + 1) {
                Some(b'[') => {
                    // CSI: 최종 바이트(0x40..=0x7e)까지
                    i += 2;
                    while i < buffer.len() && !(0x40..=0x7e).contains(&buffer[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    // OSC: 헤더/종결자(BEL 또는 ESC\)만 제거하고 payload는
                    // 매칭 대상에 포함한다 — title 등에 실린 secret도 잡아야 한다.
                    i += 2;
                    while i < buffer.len() {
                        if buffer[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if buffer[i] == 0x1b && buffer.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        stripped.push(buffer[i]);
                        map.push(i);
                        i += 1;
                    }
                }
                Some(_) => i += 2,
                None => i += 1,
            }
        } else {
            stripped.push(buffer[i]);
            map.push(i);
            i += 1;
        }
    }
    (stripped, map)
}

/// redacted(원본 좌표계)에서 stripped 기준 뒤에서 keep개에 해당하는 원본 시작 인덱스.
fn origin_index_for_stripped_suffix(buffer: &[u8], keep: usize) -> usize {
    if keep == 0 {
        return buffer.len();
    }
    let (_, map) = strip_ansi_with_map(buffer);
    if map.len() <= keep {
        0
    } else {
        map[map.len() - keep]
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn url_encode(input: &[u8], lowercase: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ if lowercase => out.push_str(&format!("%{byte:02x}")),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// application/x-www-form-urlencoded: 스페이스는 +. `~`/`*`의 safe 여부는
/// 인코더 방언마다 다르므로(URLSearchParams vs quote_plus 등) 파라미터로 받아
/// 조합 전부를 변형으로 만든다.
fn form_encode(input: &[u8], lowercase: bool, tilde_safe: bool, star_safe: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            b'~' if tilde_safe => out.push('~'),
            b'*' if star_safe => out.push('*'),
            _ if lowercase => out.push_str(&format!("%{byte:02x}")),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 모든 비 ASCII-인쇄 문자를 \uXXXX로 쓰는 JSON 직렬화 스타일
/// (일부 직렬화기는 ASCII-safe 모드로 이렇게 쓴다 — PR-18 리뷰 이연분).
/// hex 대·소문자 표기가 모두 유효하므로 양쪽 변형을 만든다.
fn json_escape_unicode(input: &str, uppercase: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                for unit in c.encode_utf16(&mut [0u16; 2]) {
                    if uppercase {
                        out.push_str(&format!("\\u{unit:04X}"));
                    } else {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Default)]
    struct ManualClock(AtomicU64);

    impl ManualClock {
        fn advance(&self, duration: Duration) {
            self.0.fetch_add(
                u64::try_from(duration.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
        }
    }

    impl RedactionClock for ManualClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    fn bounded_service(
        max_items: usize,
        max_bytes: usize,
        grace: Duration,
    ) -> (RedactionService, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::default());
        let service = RedactionService::with_clock(
            RedactionCorpusLimits {
                max_items,
                max_bytes,
            },
            grace,
            clock.clone(),
        )
        .unwrap();
        (service, clock)
    }

    fn service_with(secret: &str) -> RedactionService {
        let service = RedactionService::new();
        service.register(&SecretString::new(secret.into()));
        service
    }

    fn redact_all(redactor: &mut StreamRedactor, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(redactor.redact_chunk(chunk));
        }
        out.extend(redactor.flush());
        out
    }

    #[test]
    fn empty_safe_corpus_bypasses_pattern_and_ansi_work() {
        REDACT_BUFFER_CALLS.with(|calls| calls.set(0));
        let mut redactor = RedactionService::new().stream_redactor();
        let chunk = b"plain \x1b[31mterminal\x1b[0m output";

        assert_eq!(redactor.redact_chunk(chunk), chunk);
        assert!(redactor.flush().is_empty());
        assert_eq!(
            REDACT_BUFFER_CALLS.with(std::cell::Cell::get),
            0,
            "an empty safe corpus must not allocate stripped/index-map buffers"
        );
    }

    #[test]
    fn grace_prune_scans_only_when_the_earliest_expiry_is_due() {
        let (service, clock) = bounded_service(256, 256 * 1024, Duration::from_secs(10));
        let rotating = SecretString::new("rotating-hot-path-secret".to_owned());
        drop(service.acquire_rotating(&rotating).unwrap());
        let mut redactor = service.stream_redactor();

        for _ in 0..300 {
            let _ = redactor.redact_chunk(b"ordinary terminal output");
        }
        assert_eq!(
            service
                .inner
                .patterns
                .lock()
                .expect("redaction patterns lock")
                .prune_scans,
            0,
            "matching before grace expiry must not rescan or sort the corpus"
        );

        clock.advance(Duration::from_secs(10));
        let _ = redactor.redact_chunk(b"expiry-triggering output");
        {
            let patterns = service
                .inner
                .patterns
                .lock()
                .expect("redaction patterns lock");
            assert_eq!(patterns.prune_scans, 1);
            assert!(patterns.entries.is_empty());
            assert!(patterns.next_expiry.is_none());
        }

        for _ in 0..300 {
            let _ = redactor.redact_chunk(b"post-expiry output");
        }
        assert_eq!(
            service
                .inner
                .patterns
                .lock()
                .expect("redaction patterns lock")
                .prune_scans,
            1,
            "an empty corpus must stay on the constant-time prune gate"
        );
    }

    /// PR-22 완료 기준: redaction corpus — 각 변형에 대한 fixture.
    /// secret "pa ss+wörd\"x" 하나를 등록하고, 7장에서 요구하는 변형 각각이
    /// 스트림에서 지워지는지 fixture 단위로 확인한다.
    #[test]
    fn redaction_corpus_변형_fixture() {
        const SECRET: &str = "pa ss+w\u{00f6}rd\"x"; // 스페이스/+/비ASCII/따옴표 포함
        let service = RedactionService::new();
        service.register(&SecretString::new(SECRET.to_owned()));

        let fixtures: Vec<(&str, Vec<u8>)> = vec![
            ("plain", SECRET.as_bytes().to_vec()),
            ("base64", base64_encode(SECRET.as_bytes()).into_bytes()),
            (
                "url-encoded 대문자",
                url_encode(SECRET.as_bytes(), false).into_bytes(),
            ),
            (
                "url-encoded 소문자",
                url_encode(SECRET.as_bytes(), true).into_bytes(),
            ),
            (
                "form-encoded URLSearchParams식 (+, *safe/~enc)",
                form_encode(SECRET.as_bytes(), false, false, true).into_bytes(),
            ),
            (
                "form-encoded quote_plus식 (+, ~safe/*enc, %xx)",
                form_encode(SECRET.as_bytes(), true, true, false).into_bytes(),
            ),
            ("json-escaped", json_escape(SECRET).into_bytes()),
            (
                "json \\uxxxx",
                json_escape_unicode(SECRET, false).into_bytes(),
            ),
            (
                "json \\uXXXX",
                json_escape_unicode(SECRET, true).into_bytes(),
            ),
        ];
        for (name, encoded) in &fixtures {
            // 한 덩어리
            let mut r = service.stream_redactor();
            let mut input = b"pre ".to_vec();
            input.extend_from_slice(encoded);
            input.extend_from_slice(b" post\n");
            let out = redact_all(&mut r, &[&input]);
            assert_eq!(out, b"pre [REDACTED] post\n", "fixture 실패: {name}");

            // chunk 경계 분할 (변형 중간에서 쪼갬)
            let mid = encoded.len() / 2;
            let mut first = b"pre ".to_vec();
            first.extend_from_slice(&encoded[..mid]);
            let mut second = encoded[mid..].to_vec();
            second.extend_from_slice(b" post\n");
            let mut r = service.stream_redactor();
            let out = redact_all(&mut r, &[&first, &second]);
            assert_eq!(
                out, b"pre [REDACTED] post\n",
                "chunk 분할 fixture 실패: {name}"
            );

            // ANSI escape 삽입 (변형 중간에 SGR)
            let mut ansi = b"pre ".to_vec();
            ansi.extend_from_slice(&encoded[..mid]);
            ansi.extend_from_slice(b"\x1b[31m");
            ansi.extend_from_slice(&encoded[mid..]);
            ansi.extend_from_slice(b" post\n");
            let mut r = service.stream_redactor();
            let out = redact_all(&mut r, &[&ansi]);
            let text = String::from_utf8_lossy(&out);
            assert!(
                !text.contains("ss+w") && text.contains("[REDACTED]"),
                "ANSI 삽입 fixture 실패: {name} → {text:?}"
            );
        }
    }

    #[test]
    fn json_blob의_문자열_필드도_개별_등록() {
        // OAuth 토큰 blob (PR-18): 로그에는 blob이 아닌 개별 토큰이 찍힌다
        let service = RedactionService::new();
        let blob = r#"{"access_token":"at-secret-12345","refresh_token":"rt-secret-67890","expires_in_secs":3600}"#;
        service.register_json_fields(&SecretString::new(blob.to_owned()));
        let mut r = service.stream_redactor();
        let out = redact_all(
            &mut r,
            &[b"Authorization: Bearer at-secret-12345\nrt-secret-67890\n"],
        );
        assert_eq!(out, b"Authorization: Bearer [REDACTED]\n[REDACTED]\n");
        // JSON이 아니면 아무것도 등록하지 않는다
        let service = RedactionService::new();
        service.register_json_fields(&SecretString::new("not json at all".to_owned()));
        let mut r = service.stream_redactor();
        let out = redact_all(&mut r, &[b"not json at all\n"]);
        assert_eq!(out, b"not json at all\n");
    }

    #[test]
    fn 단순_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"key=sk-abcdef123456 done\n"]);
        assert_eq!(out, b"key=[REDACTED] done\n");
    }

    #[test]
    fn chunk_경계_분할_대응() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"key=sk-abc", b"def123456 done\n"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn ansi_escape_삽입_우회_차단() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        // secret 중간에 색상 escape 삽입
        let out = redact_all(&mut r, &[b"sk-abc\x1b[31mdef123456\x1b[0m end"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("def123456"), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn base64와_url_변형_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let b64 = base64_encode(b"sk-abcdef123456");
        let out = redact_all(&mut r, &[format!("b64={b64} end\n").as_bytes()]);
        assert!(!String::from_utf8_lossy(&out).contains(&b64));

        let mut r = service_with("sk+key/with special").stream_redactor();
        let url = url_encode(b"sk+key/with special", false);
        let out = redact_all(&mut r, &[format!("url={url} end\n").as_bytes()]);
        assert!(!String::from_utf8_lossy(&out).contains(&url));
    }

    #[test]
    fn 짧은_secret은_미등록() {
        let service = service_with("abc");
        let mut r = service.stream_redactor();
        let out = redact_all(&mut r, &[b"abc def"]);
        assert_eq!(out, b"abc def"); // 등록 안 됨 — 그대로
    }

    #[test]
    fn 등록_전_출력은_그대로_이후_출력만_치환() {
        let service = RedactionService::new();
        let mut r = service.stream_redactor();
        let before = r.redact_chunk(b"plain-before ");
        service.register(&SecretString::new("sk-abcdef123456".into()));
        let mut out = before;
        out.extend(r.redact_chunk(b"now sk-abcdef123456 leaks?"));
        out.extend(r.flush());
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
    }

    #[test]
    fn osc_payload_안의_secret도_치환() {
        let mut r = service_with("sk-abcdef123456").stream_redactor();
        let out = redact_all(&mut r, &[b"\x1b]0;title sk-abcdef123456\x07visible"]);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-abcdef123456"), "{text}");
    }

    #[test]
    fn 여러_출현과_겹침_병합() {
        let mut r = service_with("secret-value-1").stream_redactor();
        let out = redact_all(&mut r, &[b"a secret-value-1 b secret-value-1 c"]);
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text.matches("[REDACTED]").count(), 2);
        assert!(!text.contains("secret-value-1"));
    }

    #[test]
    fn rotating_lease_refcount와_grace_expiry는_match시_prune된다() {
        let (service, clock) = bounded_service(256, 256 * 1024, Duration::from_secs(10));
        let permanent = SecretString::new("permanent-secret-value".to_owned());
        service.register_permanent(&permanent).unwrap();
        let baseline = service.corpus_stats();

        let rotating = SecretString::new("rotating-secret-value".to_owned());
        let first = service.acquire_rotating(&rotating).unwrap();
        let second = service.acquire_rotating(&rotating).unwrap();
        let active = service.corpus_stats();
        assert_eq!(active.active_leases, 2);
        assert!(active.items > baseline.items);
        drop(first);
        assert_eq!(service.corpus_stats().active_leases, 1);
        drop(second);
        assert_eq!(service.corpus_stats().active_leases, 0);

        clock.advance(Duration::from_secs(9));
        let mut within_grace = service.stream_redactor();
        assert_eq!(
            redact_all(&mut within_grace, &[b"rotating-secret-value"]),
            REPLACEMENT
        );
        clock.advance(Duration::from_secs(2));
        let mut after_grace = service.stream_redactor();
        assert_eq!(
            redact_all(&mut after_grace, &[b"rotating-secret-value"]),
            b"rotating-secret-value"
        );
        assert_eq!(service.corpus_stats(), baseline);
    }

    #[test]
    fn json_execution_lease_redacts_nested_values_then_releases_them_after_grace() {
        let (service, clock) = bounded_service(512, 512 * 1024, Duration::from_secs(10));
        let blob = SecretString::new(
            r#"{"access_token":"nested-access-token","credentials":{"private_key":"nested-private-key"},"scopes":["nested-scope-value"],"escaped":"escaped-secret-\u2603","active":true,"generation":3}"#
                .to_owned(),
        );

        let lease = service.acquire_json_execution_lease(&[&blob]).unwrap();
        let active = service.corpus_stats();
        assert_eq!(active.active_leases, 1);
        assert_eq!(active.permanent_items, 0);

        let mut redactor = service.stream_redactor();
        let output = redact_all(
            &mut redactor,
            &[
                "nested-access-token nested-private-key nested-scope-value escaped-secret-☃"
                    .as_bytes(),
            ],
        );
        assert_eq!(
            output, b"[REDACTED] [REDACTED] [REDACTED] [REDACTED]",
            "every nested string value must share the operation lease"
        );

        drop(lease);
        clock.advance(Duration::from_secs(9));
        let mut within_grace = service.stream_redactor();
        assert_eq!(
            redact_all(&mut within_grace, &[b"nested-private-key"]),
            REPLACEMENT
        );

        clock.advance(Duration::from_secs(2));
        let mut after_grace = service.stream_redactor();
        assert_eq!(
            redact_all(&mut after_grace, &[b"nested-private-key"]),
            b"nested-private-key"
        );
        assert_eq!(service.corpus_stats(), RedactionCorpusStats::default());
    }

    #[test]
    fn hostile_json_is_rejected_atomically_before_corpus_growth() {
        let (small_service, _) = bounded_service(128, 4_096, Duration::ZERO);
        let oversized = SecretString::new(format!(
            r#"{{"access_token":"{}"}}"#,
            "oversized-secret-value".repeat(8)
        ));
        assert!(matches!(
            small_service.acquire_json_execution_lease(&[&oversized]),
            Err(RedactionCapacityError::SecretTooLarge { .. })
        ));
        assert_eq!(
            small_service.corpus_stats(),
            RedactionCorpusStats::default()
        );

        let (deep_service, _) = bounded_service(512, 1024 * 1024, Duration::ZERO);
        let deeply_nested = SecretString::new(format!(
            "{}\"deep-secret-value\"{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        ));
        assert!(matches!(
            deep_service.acquire_json_execution_lease(&[&deeply_nested]),
            Err(RedactionCapacityError::JsonDepthLimit {
                requested,
                maximum: MAX_JSON_DEPTH,
            }) if requested == MAX_JSON_DEPTH + 1
        ));
        assert_eq!(deep_service.corpus_stats(), RedactionCorpusStats::default());

        let (flood_service, _) = bounded_service(32, 256 * 1024, Duration::ZERO);
        let flooded = SecretString::new(format!(
            "[{}]",
            std::iter::repeat_n("\"repeated-nested-secret\"", 40)
                .collect::<Vec<_>>()
                .join(",")
        ));
        assert!(matches!(
            flood_service.acquire_json_execution_lease(&[&flooded]),
            Err(RedactionCapacityError::JsonStringLimit {
                requested: 33,
                maximum: 32,
            })
        ));
        assert_eq!(
            flood_service.corpus_stats(),
            RedactionCorpusStats::default()
        );

        let (invalid_service, _) = bounded_service(128, 256 * 1024, Duration::ZERO);
        let invalid =
            SecretString::new(r#"{"token":"already-decoded-secret","invalid":]}"#.to_owned());
        assert!(matches!(
            invalid_service.acquire_json_execution_lease(&[&invalid]),
            Err(RedactionCapacityError::InvalidStructuredJson)
        ));
        assert_eq!(
            invalid_service.corpus_stats(),
            RedactionCorpusStats::default()
        );
    }

    #[test]
    fn non_json_checked_lease_matches_normal_execution_lease() {
        let (normal_service, _) = bounded_service(256, 256 * 1024, Duration::ZERO);
        let (json_service, _) = bounded_service(256, 256 * 1024, Duration::ZERO);
        let secret = SecretString::new("ordinary-non-json-secret".to_owned());

        let normal_lease = normal_service.acquire_execution_lease(&[&secret]).unwrap();
        let json_lease = json_service
            .acquire_json_execution_lease(&[&secret])
            .unwrap();
        assert_eq!(normal_service.corpus_stats(), json_service.corpus_stats());

        let mut normal_redactor = normal_service.stream_redactor();
        let mut json_redactor = json_service.stream_redactor();
        assert_eq!(
            redact_all(&mut normal_redactor, &[secret.expose().as_bytes()]),
            redact_all(&mut json_redactor, &[secret.expose().as_bytes()])
        );

        drop(normal_lease);
        drop(json_lease);
        assert_eq!(
            normal_service.corpus_stats(),
            RedactionCorpusStats::default()
        );
        assert_eq!(json_service.corpus_stats(), RedactionCorpusStats::default());
    }

    #[test]
    fn corpus_capacity_failure_blocks_secret_backed_execution_without_partial_registration() {
        let (service, _) = bounded_service(1, 4_096, Duration::ZERO);
        let secret = SecretString::new("capacity-secret-value".to_owned());
        let result = service.acquire_execution_lease(&[&secret]);
        assert!(matches!(
            result,
            Err(RedactionCapacityError::ItemLimit { .. })
                | Err(RedactionCapacityError::ByteLimit { .. })
        ));
        assert_eq!(service.corpus_stats(), RedactionCorpusStats::default());

        let mut external_call_count = 0;
        if service.acquire_execution_lease(&[&secret]).is_ok() {
            external_call_count += 1;
        }
        assert_eq!(
            external_call_count, 0,
            "registration failure must fail closed"
        );
    }

    #[test]
    fn many_secrets_keep_temporary_variants_bounded_and_never_partially_register() {
        let (service, _) = bounded_service(32, 64 * 1024, Duration::ZERO);
        let secrets = (0..256)
            .map(|index| SecretString::new(format!("distinct-secret-value-{index}")))
            .collect::<Vec<_>>();
        let refs = secrets.iter().collect::<Vec<_>>();
        assert!(matches!(
            service.acquire_execution_lease(&refs),
            Err(RedactionCapacityError::ItemLimit {
                requested: 33,
                maximum: 32
            })
        ));
        assert_eq!(service.corpus_stats(), RedactionCorpusStats::default());

        let (duplicate_service, _) = bounded_service(32, 64 * 1024, Duration::ZERO);
        let repeated = SecretString::new("one-repeated-secret-value".to_owned());
        let duplicate_refs = vec![&repeated; 10_000];
        let lease = duplicate_service
            .acquire_execution_lease(&duplicate_refs)
            .unwrap();
        assert!(duplicate_service.corpus_stats().items <= 32);
        drop(lease);
    }

    #[test]
    fn legacy_capacity_failure_marks_service_fail_closed_and_redacts_whole_output() {
        let (service, _) = bounded_service(1, 4_096, Duration::ZERO);
        service.register(&SecretString::new("legacy-secret-value".to_owned()));
        assert!(service.ensure_safe().is_err());
        assert!(service.corpus_stats().fail_closed);
        let mut redactor = service.stream_redactor();
        assert_eq!(
            redact_all(&mut redactor, &[b"unrelated output"]),
            REPLACEMENT
        );
    }

    #[test]
    fn oversized_legacy_json_registration_fails_closed_before_parse() {
        let (service, _) = bounded_service(64, 4_096, Duration::ZERO);
        let oversized = SecretString::new(format!(
            r#"{{"access_token":"{}"}}"#,
            "sensitive-value".repeat(8)
        ));
        service.register_json_fields(&oversized);
        assert!(service.ensure_safe().is_err());
        assert!(service.corpus_stats().fail_closed);
    }

    #[test]
    fn one_hundred_token_rotations_have_zero_item_and_byte_growth() {
        let (service, clock) = bounded_service(256, 256 * 1024, Duration::ZERO);
        let dcr = SecretString::new("permanent-dcr-secret".to_owned());
        service.register_permanent(&dcr).unwrap();
        let baseline = service.corpus_stats();

        for generation in 0..100 {
            let access = SecretString::new(format!("access-token-generation-{generation}"));
            let refresh = SecretString::new(format!("refresh-token-generation-{generation}"));
            let lease = service
                .acquire_execution_lease(&[&access, &refresh, &dcr])
                .unwrap();
            assert_eq!(service.corpus_stats().active_leases, 1);
            drop(lease);
            clock.advance(Duration::from_millis(1));
            let stats = service.corpus_stats();
            assert_eq!(stats, baseline, "corpus grew at rotation {generation}");
        }
    }

    #[test]
    fn too_short_execution_secret_fails_closed() {
        let service = RedactionService::new();
        let short = SecretString::new("tiny".to_owned());
        assert!(matches!(
            service.acquire_rotating(&short),
            Err(RedactionCapacityError::SecretTooShort { .. })
        ));
        assert!(matches!(
            service.register_permanent(&short),
            Err(RedactionCapacityError::SecretTooShort { .. })
        ));
    }
}
