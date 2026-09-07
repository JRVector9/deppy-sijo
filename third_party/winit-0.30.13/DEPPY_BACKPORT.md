# Deppy Sijo winit backport

This directory is the crates.io source for `winit 0.30.13`, retained under its
upstream Apache-2.0 license and selected through the workspace's
`[patch.crates-io]` entry.

Deppy carries one macOS-only Korean IME backport from:

- upstream PR: <https://github.com/rust-windowing/winit/pull/4478>
- upstream commit: `dfb23bff7d1c945a580673a9977f2699e0234d91`
- downstream reproductions:
  <https://github.com/alacritty/alacritty/issues/6942> and
  <https://github.com/alacritty/alacritty/issues/8079>

The patch clears marked text immediately after an IME commit and forwards an
ASCII character delivered later in the same `interpretKeyEvents` call. This
prevents both a swallowed first punctuation key and duplicate Commit/keyboard
text such as a doubled Space or comma.

The upstream PR targets the winit 0.31 beta line, while eframe 0.36 currently
resolves winit 0.30.13. Remove this vendor copy and `[patch.crates-io]` entry
once the application's stable eframe/winit dependency contains the equivalent
fix.
