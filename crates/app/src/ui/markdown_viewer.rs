//! Deppy가 소유하는 Markdown 뷰어 facade.
//!
//! 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md` §5,
//! `docs/document-editor-lightweight-core-plan.md` §6(Viewer 계약)·§7(보안 정책).
//!
//! 앱의 다른 코드가 `egui_commonmark` 타입에 직접 의존하지 않게 감싼다 — 나중에
//! 갈아끼울 수 있어야 한다. leaf이므로 intent만 올린다(파일·URL을 직접 열지 않는다).
//!
//! 아직 비어 있다. 구현은 `feat/markdown-viewer`가 채운다.
