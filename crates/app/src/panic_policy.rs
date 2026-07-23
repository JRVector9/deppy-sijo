//! Process-wide sanitized panic diagnostics.

/// Installs the production panic policy. Panic payloads may contain raw paths, tool arguments,
/// OAuth material, or secrets, so the hook deliberately ignores the payload and emits only a
/// fixed low-cardinality diagnostic before unwinding continues.
pub(crate) fn install_sanitized_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        tracing::error!(
            kind = "panic",
            phase = "unwind",
            error_code = "panic",
            "application component panicked"
        );
    }));
}
