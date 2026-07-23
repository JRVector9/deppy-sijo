# 자식 프로세스 폭주 대응 로드맵 (확정)

2026-07-23 사고(터미널 세션 내 next dev가 5,417개 프로세스 + 957% CPU로 폭주 →
시스템 OOM → 앱 무대응 사망) 대응. 정책 확정: **자동 개입 없음 — 경고만 하고
행동(동결/종료)은 항상 사용자가 트리거한다.**

## 확정 PR 목록과 순서

```
A1 (즉시)
B1 ∥ C1 (병렬 — 둘 다 wire 무변경)
C2 (RuntimeCommand append 1번째)
B2 (wire 무변경, 공용 에피소드 게이트 신설)
C3 (B2의 게이트 재사용)
B3 (RuntimeCommand/RuntimeEvent append 2번째 — C2 리베이스 후)
C4 (최종 E2E 게이트)
A2 (보류 — A1 배포 후 재판단)
```

| PR | 규모 | 내용 |
|----|------|------|
| A1 | S | 자식 RSS 집계를 `ps` RSS 합산 → pid별 `proc_pid_rusage` phys_footprint로 교체 (실패 시 ps 값 폴백). 263GiB 허수 표시 수정 |
| B1 | S | 폭주 판정 상태머신 (App 레이어 순수 로직). 트리거: **프로세스 수 ≥300, 6초 지속, 단독 신호** (CPU는 제외 — 아래 검토 반영 2). 히스테리시스로 해소 판정 |
| B2 | M | 타이틀바 아래 배너 + Activity "폭주" 뱃지 + 알림센터 통지(에피소드당 1회, `platform::notify` 경유 — notify-rust 직접 사용 금지). i18n 5로케일 |
| B3 | M | [동결](SIGSTOP)/[재개](SIGCONT)/[종료](기존 KillSession). wire enum append, 원격 클라이언트 허용은 KillSession과 일관되게 허용 |
| C1 | S | dispatch2 `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` 구독 (신호만) |
| C2 | M | 압박 warning 시 `EmergencyPersistFlush` 커맨드 → 기존 `PersistPipe::flush_async_writes()` 즉시 실행. 유일한 유실 창인 DbWriteWorker 50ms 배치를 닫는다 (SQLite WAL 커밋분은 SIGKILL에도 안전 — checkpoint 불필요) |
| C3 | S | "메모리 압박 — 최대 사용 세션: X" 1회 통지 (B2 게이트 재사용) |
| C4 | M | SIGKILL 시뮬레이션 E2E 복구 검증 + 보장/비보장 범위 문서화. audit/mcp-store 쓰기 경로 즉시-커밋 재확인 포함 |
| A2 | M | (보류) 근사치 투명성 표시 + high_rss 임계 재보정 |

## 검토 반영 사항 (2회 교차 검토)

1. **wire append 순서 고정**: C2 → B3. postcard variant 순서 민감성 때문에 병렬
   랜딩 금지, 뒤 PR은 앞 PR 리베이스 후 append.
2. **B1 트리거에서 CPU 제외**: 정상 빌드(cargo/webpack)가 수백% CPU를 수 분
   유지하므로 CPU-단독 트리거는 오경보 양산. 프로세스 수 단독. CPU만 높은
   경우는 기존 "High" 뱃지, 소수 프로세스 메모리 누수형은 C1 압박 감지가 커버.
3. **캡처 결측 ≠ 해소**: `ps` 실패/타임아웃/16,384행 초과 시 빈 목록이 "전부
   0"으로 emit되어 폭주 절정에 배너가 자동 소멸하는 결함. 빈 행 목록은 정상
   시스템에서 불가능(최소 앱 자신 존재)하므로 실패 sentinel로 간주, 세션 usage
   emit 생략(마지막 상태 유지). runtime 1줄 예외 변경.
4. **알림 게이트 공용화**: B2에서 에피소드 게이트 헬퍼 신설, C3 재사용 (중복
   구현 드리프트 방지).
5. **동결 중 앱 사망 시 잔류 정지 프로세스**: POSIX 고아 프로세스그룹 규칙
   (SIGHUP+SIGCONT 자동 수신)으로 해소되는지 B3 테스트에 포함.
6. **A1 전제 실증 완료**: 같은 uid 타 프로세스 `proc_pid_rusage(RUSAGE_INFO_V4)`
   특권 없이 성공 확인 (2026-07-23 실측).
7. **복원 의미 명확화**: 재시작 시 폭주 프로세스는 재개되지 않음(fresh 셸
   spawn 원칙). 무손실 = 레이아웃/세션 메타/출력 로그의 마지막 순간까지 복원.
8. **앱 자체 메모리 폭식 경로는 기존 바운드로 차단 확인**: 스크롤백 10,000줄
   (비가시 1,000줄), ANSI 로그 16MiB tail-bounded.

## C4 — OOM 복구 보장/비보장 범위 (구현 완료 기준)

무손실의 의미와 한계를 명시한다(기획 오해 방지).

**보장**
- SQLite로 이미 `tx.commit()`된 데이터는 WAL 모드라 SIGKILL(=OOM-kill)에도
  안전하다. WAL checkpoint는 crash-safety 수단이 아니라 파일 크기 관리용이라
  비상 플러시 대상이 아니다.
- 유일한 유실 창이던 `DbWriteWorker`의 50ms debounce 배치(세션 status /
  last_log_offset)는 C2의 `EmergencyPersistFlush → flush_async_writes()`가 압박
  격상 시 즉시 커밋한다. `Drop`은 SIGKILL에서 실행되지 않으므로 이 경로가
  Drop-독립적 내구성을 만든다 — `비상_플러시는_drop_없이_배치를_커밋한다`
  테스트가 실증(대조군: flush 전 미반영 → flush 후 커밋).
- 창 레이아웃 / 세션 spawn 행은 각 변경 시 동기 auto-commit이라 원래 내구적.
- 재시작 시 `reconcile_orphan_sessions` + `validate_log_offset`이 orphan 세션을
  exited로 정리하고 partial-write offset을 마지막 완전한 줄로 되돌린다.

**비보장 (문서화된 한계 — 이번 범위에서 고치지 않음)**
- 복원은 항상 fresh 셸만 spawn한다(기존 안전 원칙). **폭주하던 프로세스 자체는
  재개되지 않는다.** 무손실 = 레이아웃/세션 메타/출력 로그의 마지막 순간까지
  복원이지, 죽어가던 프로세스의 재개가 아니다.
- 압박 warning부터 실제 SIGKILL까지가 매우 짧으면(수십 ms) non-blocking 커맨드가
  워커에 도달·커밋되기 전에 죽을 수 있다. "무손실"이 아니라 "손실 창을 50ms
  배치 주기에서 신호~커밋 지연으로 좁히는" 완화다.
- `send_command`가 큐 포화로 `try_send` 실패하면 그 플러시는 유실된다(재시도
  없음 — 압박 상황에 부하를 더하지 않기 위한 의도적 선택).
- audit / mcp-store 등 다른 쓰기 경로는 즉시 커밋이라 이 배치 유실 창에
  해당하지 않음(C4 확인 항목).
