# Agent Launcher Technical Labels Design

## Objective

Make the Korean agent launcher preserve the provider's English technical terminology for reasoning effort, and present Claude's one-million-context Opus model with readable capitalization and spacing.

## Display contract

- The Claude model whose execution value is `opus[1m]` is displayed as `Opus [1M]`.
- In the Korean launcher, the graded effort field label is `Reasoning Effort`.
- In the Korean launcher, effort values are displayed as their English provider terms: `Low`, `Medium`, `High`, `XHigh`, `Max`, and `Ultra`.
- The boolean thinking field remains a separate concept. Its `Thinking`, `On`, and `Off` labels are unchanged by this request.
- The ordinary `Model` label remains localized as `모델`.

## Implementation boundary

- Preserve the execution value `opus[1m]`; only its `ModelChoice` label changes.
- Preserve `ReasoningEffort` values and CLI argument construction; only the Korean launcher catalog text changes.
- Do not change agent-session management screens, shortcuts, status rows, or non-Korean catalogs.
- Reuse the existing launcher combo boxes, spacing, typography, colors, focus behavior, and accessibility metadata.

## Verification

- Add a model-catalog regression proving `opus[1m]` keeps its execution value while exposing `Opus [1M]` as its display label.
- Add a Korean-catalog regression proving the launcher resolves `Reasoning Effort` and English effort values.
- Run focused launcher/i18n tests, the i18n catalog check, formatting, and diff checks.
