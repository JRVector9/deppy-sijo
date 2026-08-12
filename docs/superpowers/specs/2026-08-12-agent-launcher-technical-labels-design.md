# Agent Launcher Technical Labels Design

## Objective

Make the Korean agent launcher preserve the provider's English technical terminology for reasoning effort, and present Claude's one-million-context Opus model with readable capitalization and spacing.

## Display contract

- The Claude model whose execution value is `opus[1m]` is displayed as `Opus [1M]`.
- In the Korean launcher, Claude, Codex, and Kimi graded-effort fields use the label `Reasoning Effort`.
- In the Korean launcher, all graded effort values are displayed as their English provider terms: `Low`, `Medium`, `High`, `XHigh`, `Max`, and `Ultra`.
- Kimi boolean-thinking models remain a separate concept and use the English labels `Thinking`, `On`, and `Off`.
- The ordinary `Model` label remains localized as `모델`.

## Implementation boundary

- Preserve the execution value `opus[1m]`; only its `ModelChoice` label changes.
- Preserve `ReasoningEffort` values and provider-specific CLI/environment argument construction; only the Korean launcher catalog text changes. The shared launcher keys intentionally apply the approved English terms to Claude, Codex, Kimi, and any other launcher provider exposing the same capability.
- Do not change agent-session management screens, shortcuts, status rows, or non-Korean catalogs.
- Reuse the existing launcher combo boxes, spacing, typography, colors, focus behavior, and accessibility metadata.

## Verification

- Add a model-catalog regression proving `opus[1m]` keeps its execution value while exposing `Opus [1M]` as its display label.
- Add a Korean-catalog regression proving the launcher resolves `Reasoning Effort`, all English graded values, and `Thinking`/`On`/`Off`.
- Run focused launcher/i18n tests, the i18n catalog check, formatting, and diff checks.
