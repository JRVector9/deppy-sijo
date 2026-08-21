//! 문서 파일(md·txt)의 유계 로드/저장 lane.
//!
//! 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md` §4·§6·§7.
//!
//! 이 모듈은 UI 타입에 의존하지 않는다 — App이 워커로 돌릴 수 있는 요청/결과 값
//! 타입만 노출한다(`dotenv_sync`, `agent_state_worker`와 같은 관례).
//!
//! 아직 비어 있다. 구현은 `feat/document-io`가 채운다.
