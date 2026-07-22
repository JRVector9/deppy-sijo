use std::sync::atomic::{Ordering, compiler_fence};

use serde_json::Value;

/// Internal non-Clone byte owner for serialized requests that may contain tool arguments.
pub(crate) struct SensitiveBytes {
    bytes: Vec<u8>,
    #[cfg(test)]
    drop_observer: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)]
    growth_wipes: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl SensitiveBytes {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            #[cfg(test)]
            drop_observer: None,
            #[cfg(test)]
            growth_wipes: None,
        }
    }

    pub(crate) fn from_json(mut value: Value) -> serde_json::Result<Self> {
        let serialized = Self::serialize_exact(&value);
        zeroize_json_value(&mut value);
        serialized
    }

    fn serialize_exact(value: &Value) -> serde_json::Result<Self> {
        let mut counter = CountingWriter::default();
        serde_json::to_writer(&mut counter, value)?;
        let mut bytes = Self::with_capacity(counter.bytes);
        serde_json::to_writer(&mut bytes, value)?;
        Ok(bytes)
    }

    pub(crate) fn copy_from_slice(bytes: &[u8]) -> Self {
        let mut owned = Self::with_capacity(bytes.len());
        owned.bytes.extend_from_slice(bytes);
        owned
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.reserve_secure(1);
        self.bytes.push(byte);
    }

    fn reserve_secure(&mut self, additional: usize) {
        if self.bytes.len().saturating_add(additional) <= self.bytes.capacity() {
            return;
        }
        let required = self.bytes.len().saturating_add(additional);
        let capacity = required.max(self.bytes.capacity().max(64).saturating_mul(2));
        let mut replacement = Vec::with_capacity(capacity);
        replacement.extend_from_slice(&self.bytes);
        volatile_zeroize(&mut self.bytes);
        #[cfg(test)]
        if let Some(observer) = &self.growth_wipes
            && self.bytes.iter().all(|byte| *byte == 0)
        {
            observer.fetch_add(1, Ordering::AcqRel);
        }
        self.bytes = replacement;
    }

    #[cfg(test)]
    pub(crate) fn observe_zeroized_drop(
        &mut self,
        observer: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.drop_observer = Some(observer);
    }

    #[cfg(test)]
    pub(crate) fn observe_growth_wipes(
        &mut self,
        observer: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        self.growth_wipes = Some(observer);
    }
}

impl std::io::Write for SensitiveBytes {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.reserve_secure(buffer.len());
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Debug for SensitiveBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SensitiveBytes(REDACTED)")
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        volatile_zeroize(&mut self.bytes);
        #[cfg(test)]
        if let Some(observer) = &self.drop_observer {
            observer.store(self.bytes.iter().all(|byte| *byte == 0), Ordering::Release);
        }
    }
}

pub(crate) fn zeroize_json_value(value: &mut Value) {
    match std::mem::take(value) {
        Value::String(mut string) => {
            // SAFETY: no references survive this function; zero bytes remain valid UTF-8.
            volatile_zeroize(unsafe { string.as_bytes_mut() });
        }
        Value::Array(mut values) => {
            for value in &mut values {
                zeroize_json_value(value);
            }
        }
        Value::Object(values) => {
            for (mut key, mut value) in values {
                // SAFETY: keys are owned and immediately dropped; zero bytes remain valid UTF-8.
                volatile_zeroize(unsafe { key.as_bytes_mut() });
                zeroize_json_value(&mut value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

pub(crate) fn volatile_zeroize(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: each byte is valid and uniquely borrowed. Volatile writes plus the fence keep
        // the wipe from being optimized away before deallocation.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}
