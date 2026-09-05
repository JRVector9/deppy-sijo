# 인계 프롬프트 — 다음 에이전트에게 그대로 붙여넣기

아래 블록을 통째로 복사해 새 세션 첫 메시지로 넣는다.

---

```
deppy-sijo(/Users/jr/Desktop/projects/deppy-sijo) 작업을 이어받아라.

먼저 이 세 개를 읽어라. 이게 현재 상태의 전부다.
1. docs/handoff/2026-09-06-session-handoff.md  ← 인계 문서, 여기부터
2. CLAUDE.md
3. git log -1 && git status --short && gh pr view 146

지금 상태를 요약하면: 브랜치 feat/fleet-one-list-and-relay-wip의 커밋 53f2a31에
이번 세션 UI 작업과 미완성 Relay 작업이 함께 담겨 PR #146으로 올라가 있다. 머지하지
않았다. 작업 트리는 깨끗하고, 새 빌드로 앱이 떠 있다. 게이트는 fmt·clippy·i18n·
diff --check 통과, 테스트 2103건 통과다.

할 일을 이 순서로 진행해라.

[1] 화면 확인부터 받아라
작업(fleet) 화면을 한 목록으로 개편했는데 사용자가 아직 화면으로 확인하지 않았다.
사용자에게 아래 네 가지를 봐 달라고 요청하고, 피드백이 오면 그것부터 고쳐라.
 - 막힌 항목이 맨 위에 하나만 펼쳐지는가
 - 펼쳐진 항목의 세션이 카드로 한 번 더 보이지는 않는가
 - 세션을 전부 닫고 승인만 남았을 때 펼친 카드와 「세션 없음」 안내가 함께 나오는가
 - 창을 좁혔을 때 펼친 카드가 화면 안에 들어오는가
피드백을 고칠 때 절대 깨뜨리면 안 되는 계약 두 개가 있다. 인계 문서 2절에 적어 뒀다.
큐가 비어도 waiting_ui.render를 매 프레임 호출해야 하고(stale 버퍼 정리),
세션 0 + 승인 1건일 때 펼친 카드와 빈 상태 안내가 둘 다 보여야 한다. 둘 다 회귀
테스트가 걸려 있으니 지우지 마라.

[2] PR #146을 어떻게 할지 사용자에게 물어라
Relay 갈래가 미완성이라 그대로 머지할지, Relay를 빼고 UI만 추릴지 판단이 필요하다.
파일 경계로는 나눌 수 없다는 점을 먼저 알려라 — app.rs 44개 hunk 중 relay가 385줄이고
settings.rs·로케일 5개·핸드오프 문서에서도 hunk 단위로 얽혀 있다. 나누려면 hunk 수술이
필요하고 중간 커밋은 빌드되지 않는다. 네가 임의로 rebase하거나 force-push하지 마라.

[3] 미사용 i18n 키 정리 여부를 물어라
로케일 5개의 fleet.hero.now / next / clear / sessions는 코드 참조가 0이다.
fleet.hero.skip의 참조 1건은 "버튼이 없어야 한다"를 확인하는 테스트다.
게이트는 통과하므로 남겨도 되고, 지우려면 5개 로케일에서 함께 지워라.
fleet.hero.approval / needs_input과 fleet.blocked_for는 여전히 쓰니 건드리지 마라.

[4] 그다음은 Relay Task 2 재개다
docs/CODEX_HANDOFF.md의 "STOP HANDOFF — production Relay Task 2 partial
implementation (2026-08-28)" 절에 잔여 작업과 재개 명령이 그대로 있다.
docs/superpowers/plans/2026-08-28-production-relay.md도 함께 읽어라.
DNS·TLS·배포 자격증명이 없다. 배포 검증은 BLOCKED로 기록하고 절대 PASS로 적지 마라.

지킬 규칙:
- 한국어로 답해라. 코드 주석도 한국어다.
- 재빌드·재실행은 승인받고 해라. 재기동은 사용자가 보고 있는 앱을 죽인다.
- UI 변경은 테스트가 아니라 화면으로 검증해라(CLAUDE.md). 빌드+재기동이 첫 액션이고
  게이트는 커밋 직전에 한 번만 돌린다. 화면 밖 로직·storage에는 이 예외를 적용하지
  마라 — 그건 기존대로 테스트 먼저다.
- 서브에이전트는 opus / high가 기본값이다.
- force-push 같은 파괴적 단계는 확인 후 진행해라.

이 기계의 함정(인계 문서 6절):
- 스왑이 포화라 rustc가 CPU 0%로 멎을 수 있다. 빌드가 멈춘 것 같으면 ps부터 봐라.
- pkill -f "cargo ..." 쓰지 마라. 래퍼 셸이 그 패턴을 포함해 자기 자신을 죽인다. pid로 죽여라.
- cargo를 죽여도 파이프 뒤의 tail이 살아남아 체인이 멈춘다. tail도 같이 죽여라.
- cargo fmt가 앵커를 재배치해 문자열 치환이 조용히 실패한 적이 있다. 편집 후 테스트로 확인해라.
- app_file_tree_watcher_submit_replace…는 부하 중에만 실패하는 플레이크다.
  실패를 보면 먼저 단독 실행으로 재확인해라.

먼저 [1]부터 시작하고, 사용자에게 물어야 할 것([2],[3])은 한 번에 모아서 물어라.
```
