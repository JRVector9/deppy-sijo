Upstream: crates.io portable-pty 0.9.0, copied from the existing local Cargo cache.
License: LICENSE.md (MIT).

Local change: Unix CommandBuilder accepts an optional owned directory descriptor.
The fork child calls fchdir before the existing close_random_fds pass. Failure
aborts spawn. The descriptor is skipped by serde and does not survive exec.
Path cwd and Windows behavior are unchanged when no descriptor is supplied.

Regression fixtures live in crates/pty/src/directory_cwd_tests.rs to use the
workspace dependency lock without downloading upstream example-only dev crates.
