# Product

## Register

product

## Users

Deppy Sijo is for developers who run multiple AI coding agents and terminal sessions across one or
more software projects. They use it as a native desktop workbench during focused development, where
fast scanning, predictable keyboard and pointer interaction, and reliable session continuity matter
more than decorative presentation.

## Product Purpose

Deppy Sijo is an AI agent workspace with a multiplexed terminal runtime. It brings project sessions,
agent launch and status, files, connectors, credentials, and remote access into one resource-conscious
desktop application while keeping the UI separated from runtime, terminal, and PTY implementation.
Success means users can understand what is running, move between projects, and launch or resume work
without losing state or paying unnecessary idle CPU and memory cost.

## Brand Personality

Focused, technical, restrained. The product should feel like a dependable professional development
tool: dense enough for real work, calm under load, and explicit about state, risk, and failure.

## Anti-references

- Card-heavy SaaS dashboards that fragment one workflow into decorative containers.
- Glassmorphism, gradients, glow, and oversized marketing typography inside the product UI.
- Interfaces that hide important actions behind ambiguous decoration or repeat the same information.
- Fake success states, placeholder metrics, or optimistic status that is not backed by runtime data.
- Consumer-style onboarding or animation that slows repeated expert workflows.

## Design Principles

1. Preserve the user’s mental model: project, session, pane, agent, and runtime ownership must remain
   explicit and stable.
2. Show only truthful state: every status, count, warning, and capability comes from an authoritative
   source and exposes uncertainty when the source is unavailable.
3. Optimize repeated work: frequent actions stay visible, compact, and predictable without removing
   safer alternatives or advanced controls.
4. Keep structure quiet: hierarchy comes from alignment, spacing, typography, and low-contrast lines;
   color is reserved for interaction, identity, and meaningful state.
5. Treat resource discipline as product quality: avoid idle work, unbounded lists, hidden processes,
   unnecessary snapshots, and render-path I/O.

## Accessibility & Inclusion

Maintain readable text contrast in both dark and light themes, keyboard focus and navigation,
AccessKit labels for interactive controls, and non-color indicators for selection and status. Keep
click targets usable in dense layouts, preserve localized strings and long-text resilience, and avoid
motion that would require a reduced-motion alternative unless it materially improves comprehension.
