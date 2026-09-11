# f46b168 에이전트 대기 상태 독립 재리뷰

대상: `investigate/agent-wait-classification`, `f46b168` 및 이어지는 기존 승인 처리.
요청: `한번더 리뷰해`. 소스를 수정하지 않는 리뷰다.

## 확인된 문제

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/storage/src/agent_attention.rs:442 | 취소된 익명 질문의 늦은 결과가 음수 개수를 만듦 | 다음 질문이 상쇄되어 표시되지 않음 | 취소된 질문의 후속 결과를 구분 |
| medium | crates/storage/src/agent_attention.rs:79 | 이력 정리 후 매칭할 수 없는 음수 개수 유지 | 해당 서버의 다음 질문도 계속 누락 | 이력 정리 시 매칭 불가능한 잔액도 정산 |
| medium | crates/mcp-proxy/src/agent_attention.rs:215 | 중단된 자식 요청 및 승인 별칭 해제 누락 | 다음 부모 턴이 끝나도 승인 대기 유지 | 자식 중단 근거와 호출 ID·승인 별칭의 연결 보존 |
| medium | crates/mcp-proxy/src/agent_attention.rs:136 | 같은 입력의 ID 없는 병렬 승인을 하나의 fingerprint로 합침 | 한 건의 결과가 다른 미승인 요청도 해제 | 실제 호출 ID와 승인 요청의 연결 또는 개수 유지 |
| medium | crates/mcp-proxy/src/agent_attention.rs:149 | 큰 결과에서 상태 메타데이터까지 폐기 | 답변 완료 후에도 질문 대기 유지 | 본문 크기를 제한하되 호출 ID와 결과 상태는 수집 |

### 1·2. 익명 질문의 음수 개수 이월

같은 잔여 개수 모델이지만 취소와 이력 정리라는 두 위치에서 각각 보완해야 한다.

- `Elicitation → StopFailure → ElicitationResult(action=cancel) → Notification(elicitation_response) → UserPromptSubmit → Elicitation`: 취소가 개수를 지운 다음 늦은 결과가 -1을 만들고, 새 질문은 0으로 상쇄된다. 현재 커밋 `pending=[]`, 부모 fc9bd03은 새 질문 1건이다. 공식 notification도 포함한 실제 normalize 경로로 재현했다.
- 먼저 저장된 익명 결과가 -1인 상태에서 다른 서버의 128쌍 시작/결과가 세션 이력 예산을 채운다. 원래의 늦은 시작은 정리된 floor 아래라 버려지고 -1은 남는다. 이후 최신 질문도 0으로 상쇄된다. 현재 waiting=false, 부모 waiting=true다. 이력 한도 밖 사건을 버리는 것과 다음 최신 질문까지 계속 누락하는 것은 구분해야 한다.

근거: `observe_anonymous`의 음수 balance 및 floor 비교, `Cancelled`의 초기화, `prune_anonymous_history`가 잔여 balance를 유지하는 조합. 원본 개수와 경계 이전 결과의 부채를 구분해야 한다.

### 3. 중단된 자식의 승인 대기 고착

자식 Bash의 `PreToolUse → PermissionRequest(ID 없음)` 뒤 사용자가 중단하면 실제 Claude의 aborted_tools 경로는 PostToolBatch/정상 Stop 전에 반환한다. 부모의 다음 UserPromptSubmit/Stop은 자식 transcript를 조회하지 않고, 부모 Completed는 자식 승인을 보존하므로 `child:…:tool:…`이 남는다.

자식 SubagentStop과 정확한 tool_result를 나중에 주는 대조에서도 승인 fingerprint가 남았다. 현재 transcript 보완은 원래 tool_use_id만 해제하고 승인 별칭과의 관계를 모르기 때문이다. 실제 설치 Claude 코드의 중단 경로와 [공식 결과 훅 계약](https://code.claude.com/docs/en/hooks#posttoolusefailure)을 대조했다. 부모 fc9bd03은 자식 승인을 아예 관찰하지 않았으므로 이 고착은 없지만, 그 방식으로 롤백하면 자식 승인 감지 결함이 되살아난다.

### 4. 동일 입력의 병렬 승인 충돌

`q1`과 `q2`가 같은 Bash 입력으로 PreToolUse와 PermissionRequest를 보내면 두 승인이 같은 fingerprint를 공유한다. q1에만 PostToolUse를 보내도 q2의 결과 없이 waiting=0이다. 입력이 다른 대조군은 waiting=1을 유지했다.

[공식 PermissionRequest 계약](https://code.claude.com/docs/en/hooks#permissionrequest)에 tool_use_id가 없으며, permission_prompt notification은 약 6초 뒤에만 발생한다. 따라서 즉시 알림이 이 충돌을 가려준다고 전제할 수 없다. 이는 f46b168에서 새로 도입된 부분만의 회귀가 아니라 기존 fingerprint 기반 승인 처리에도 남은 문제다. 화면 패턴 감지가 보완할 수 있으나 직접 상태 신호는 누락된다.

### 5. 큰 답변 결과를 읽지 못함

`run_hooks`는 전체 payload가 256KiB를 넘으면 메타데이터까지 버린다(`main.rs:305`). Stop에서 보완하려 해도 `claude_finished_requests`는 64KiB를 넘는 JSONL 행을 버린다. 합성 300KiB tool_result를 가진 질문은 Stop 뒤에도 정확한 질문 ID `q`가 남았다. 작은 결과를 넣은 대조군은 해제됐다.

합성 데이터로 크기 경계를 검증한 것이며 실제 CLI에 300KiB 답변을 제출한 화면 검증은 아니다. 크기 제한은 유지하면서 본문과 호출 ID·결과 상태를 분리하는 방향이 필요하다. [공식 PostToolBatch 설명](https://code.claude.com/docs/en/hooks#posttoolbatch)도 결과가 클 수 있음을 명시한다.

## 검증과 범위

- 실제 Codex CLI 리뷰는 exit 0으로 완료했다. 6개 변경 Rust 소스에서 P2 3건을 보고했으며 현재·부모 storage/proxy 소스를 별도 임시 하네스에 연결해 비교했다. 자체 검토의 추가 2건을 합쳐 최종 5건이다. 로그 `/tmp/deppy-attention-f46b168-review.log`.
- 독립 재현: `/private/tmp/deppy-f46b-independent.vDGj5o/run.sh`. 취소·실제 provider 정규화·이력 정리·자식 중단·늦은 자식 결과의 결함 동작을 확인한다. assertion 성공은 결함 재현 성공이며 수정 PASS가 아니다.
- 자체 하네스: `/tmp/deppy-f46-review-harness-path`에 위치 기록. `cargo test --manifest-path <하네스>/Cargo.toml repros:: -- --nocapture --test-threads=1`.
- 자체 로그 `/tmp/deppy-f46-review-repros-final.log`: 정상 기대 3건 실패, 정상 대조군 2건 통과. 자식 별칭 실패는 독립 리뷰와 중복이라 하나로 합쳤다.
- 이전 단계 501 PASS/1 ignored 및 정적 검사 PASS는 당시 결과다. 이번에는 전체 suite/앱 빌드/정적 gate를 반복하지 않고 빠진 경계를 재현했다.
- 저장소 소스·Cargo.lock, 운영 DB, 사용자 설정, 실행 앱 PID 25095 및 release bundle은 변경하지 않았다. 문서만 변경했다. 실제 GUI/실제 CLI 대화 검증은 하지 않았다.

## 후속 수정 결과

사용자가 코드 수정·검증·재빌드를 요청해 다음 구현으로 5건을 반영했다.

- 익명 질문: 발생 시각으로 정렬된 제한 이력과 경계 이전의 양수 잔여로 계산한다. 취소 이후 결과나 이미 정리된 결과가 이후 질문을 상쇄하지 않는다. 재정렬/재전송은 기존 회귀와 함께 확인했다.
- 실행/승인 연결: 요청에는 실제 호출 ID, 소유자, 도구 그룹, 입력 해시만 저장한다. ID 없는 동일 입력의 병렬 승인은 해당 후보들의 마지막 결과까지 보존한다. 입력이 다른 자동 실행은 승인과 분리한다. 알려진 자식은 부모의 후속 훅에서도 정확한 자식 transcript 결과 ID를 확인한다. 정상 부모 Stop만으로 자식 대기를 지우지 않는다.
- 큰 결과: hook은 결과 본문을 IgnoredAny로 소비하고 식별자를 보존한다. 스캔 8MiB, 결과 메타데이터 256KiB, 배열 64개 상한이다. 입력 해시의 깊이 32/객체 필드 64 상한을 넘으면 해시 없이 도구 ID 그룹으로 보완한다. Claude transcript는 기존 512KiB tail에서 64KiB 행 폐기를 없애고 결과 메타데이터만 추출한다. 300KiB 결과는 합성 데이터로 검증했다.
- serde 직접 의존성 추가 외 의존성 버전 변경 없음. SQLite 테이블 추가 변경 없이 v42 JSON의 기본값 필드로 확장했다.

### 구현 후 독립 Codex CLI 리뷰

실제 소스 diff 리뷰는 exit 0으로 완료했으며 P2 2건을 지적했다. 두 건 모두 반영했다.

1. 취소된 턴의 늦은 Working이 공통 등록으로 부활하는 경계: 종료 검사를 모든 실행 등록 전에 적용했다.
2. 자동 승인된 다른 입력의 동일 도구가 승인 대기를 붙잡는 경계: 입력 구조 해시와 실제 실행 ID를 함께 대조하고, 구체적 승인이 끝나면 연결된 notification도 해제한다.

추가 자체 검토에서 결과 뒤 저장되는 과거 승인, Grok cwd 메타데이터, 해시의 객체 키 순서/파싱 경로 일치도 확인했다. 실패 후 수정한 회귀는 인계 문서에 RED/GREEN 로그를 기록했다.

최신 관련 테스트는 **512 PASS, 1 ignored**: storage 364 + session 68 + proxy 80. 로그 `deppy-attention-fix5-final-related.log`. 최종 gate 및 앱 빌드 결과는 인계 문서의 후속 완료 기록을 확인한다.

### 검증 범위

ID 없는 승인에서 다른 훅이 입력을 변경해 해시도 일치하지 않으면, 동일 도구의 실행 후보가 끝날 때까지 보수적으로 보존한다. 메타데이터·tail 한도 밖의 임의 크기/모든 CLI 버전 지원을 뜻하지 않는다. 실제 GUI 및 실제 Claude/Codex/Grok 대화 화면은 재실행 후 사용자가 확인해야 한다. 이번 요청은 재빌드까지이며 main 머지나 앱 재실행은 포함하지 않는다.
