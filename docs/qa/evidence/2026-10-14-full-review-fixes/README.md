# 전체 리뷰 수정 증거 — 2026-10-14 / 최종 확인 2026-10-15

결과와 한계는 [검증 보고서](../../2026-10-14-full-review-fixes.md)에 있다. Canonical은 `/Users/jr/Desktop/projects/deppy-sijo-performance`, 기준 HEAD는 `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test version은 `0.12.1`이다. 사용자용 앱 배포·재시작·커밋·푸시는 수행하지 않았다.

## 읽는 순서

1. `deppy-full-fix-final-test-counts-20261014.json`: harness별 최신 결과. 전체 **5702 PASS / 0 FAIL / 65 ignored**이며 supplemental 재검사를 더하지 않는다.
2. `deppy-full-fix-final-workspace-20261014.log`: 전체 첫 실행 **exit101**, 5679 PASS / 22 FAIL / 65 ignored. App 1개와 storage/storage-core 21개는 보고서의 두 원인이다.
3. `deppy-full-fix-final-affected-app-storage-20261014.log`: 교정 후 3개 package 전체 **exit0**, 3747 PASS / 42 ignored. 이전 실패 결과를 대체하며 중복 합산하지 않는다.
4. `deppy-full-fix-final-checks-20261014.json`과 해당 로그: strict Clippy / fmt / boundary / deps exit0, Windows offline-check exit101(미캐시 의존성으로 컴파일 전 중단).
5. `deppy-full-fix-post-lint-tests-20261014.json`과 해당 로그: 최종 스타일 정리 후 App 신규32 / SessionAux4 / 이력1 / coordinator83(2ignored), 모두 exit0.
6. `final-source-snapshot.json`과 `final-task-changes.patch`: 412개 시작 파일 대비 기존 변경25·누락0, locale5·신규module3·synthetic DER2 및 최종422개 SHA. 기존 dirty 제품6개 byte-identical 확인 포함.

각 lane의 최종 patch/manifest는 worker 산출물이다. 이후 Root의 App/runtime 연결, storage 부모 링크 보정 및 좁은 lint 정리는 **최종 task patch와 source snapshot**을 기준으로 확인한다. 개별 worker patch를 현재 tree에 다시 적용하지 않는다.

## 실제 명령

모두 canonical에서 `CARGO_BUILD_JOBS=2`로 한 번에 Cargo 한 개만 실행했다. 아래 전체 명령의 exit101과 affected 재검사의 exit0를 구분한다.

```sh
CARGO_BUILD_JOBS=2 cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo -p storage -p storage-core --no-fail-fast -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy --offline --locked --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
CARGO_BUILD_JOBS=2 cargo run --offline --locked -p xtask -- check-boundary
CARGO_BUILD_JOBS=2 cargo run --offline --locked -p xtask -- check-deps
CARGO_BUILD_JOBS=2 cargo check --offline --locked --target x86_64-pc-windows-msvc -p deppy-sijo --tests
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo full_fix_20261014 -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo session_aux:: -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo work_history_tab은_전역view가_아니라_pane보조탭이고_입력을_닫는다 -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p connector-service -- --test-threads=1
git diff --check
git apply --reverse --check docs/qa/evidence/2026-10-14-full-review-fixes/final-task-changes.patch
```

`final-task-changes.patch`는 **시작 dirty snapshot에 대한 증분 text patch**다. clean HEAD에 적용할 patch가 아니다. 이 폴더만으로 모든 기존 untracked 파일이 복원된다고 주장하지 않는다. synthetic DER fixture는 source snapshot의 원래 경로·SHA로 확인한다. 현재 checkout을 초기화하거나 기존 dirty 파일을 제거하지 않는다.

## 재현·실측 해석

- `red-*` 로그의 compile 성공 뒤 예상 보호 assertion 실패만 실제 RED로 기록했다. fixture 선택/compile/cache 오류는 보고서에서 구분했다.
- `red-documents*.log.gz`는 큰 원본을 gzip 저장했다. `manifest.json`에 원본과 저장 bytes의 SHA를 따로 기록했다. 나머지 로그는 줄 끝 공백·끝 빈 줄만 정리했으며 patch/source bytes는 보존한다.
- P01 전후 bench는 동일 production 함수의 optimized 합성 harness, 각 10회다. 32/2048: 1.313→0.163ms, 128/8192: 18.015→0.622ms, 256/16384: 70.400→1.249ms. 실제 GUI frame 시간을 측정한 결과가 아니다.
- C02 Plain baseline RED는 실행했으며 TLS baseline RED는 실행하지 않았다. 수정 후 실제 loopback TLS 포함 5회귀: receive40ms→41ms(Plain), 40ms(TLS), stop20ms(TLS), worker shutdown0ms(정수 ms). connect 중 취소는 기존 전체10초 범위다. production Relay BLOCKED 및 WebPKI 정책은 유지한다.
- native memory diagnostic은 20회×5개 private child: footprint와 `ps` RSS의 비교가 `<`19 / `=`0 / `>`1이었다. 같은 native 호출의 footprint는 resident보다20회 모두 컸다. 제품 memory metric은 바꾸지 않고 잘못된 test invariant만 계약 검사로 교정했다. Apple primary URL/SHA는 `manifest.json`이 가리키는 native evidence manifest에 있다.
- Windows cross compile/runtime, 실제 사용자 provider/GUI/PWA 연결, OS sleep 복귀는 검증되지 않았다. R03 최종 revision 검사→rename 사이에는 비협조적 writer와의 OS compare-and-swap 보장이 없다.

## 최종 증거 검사

`deppy-full-fix-final-evidence-check-20261014.log`는 저장 manifest SHA·gzip 원본 SHA·현재 source422개 SHA·상대 파일 링크·문서/로그 whitespace·tracked diff 및 증분 patch reverse check를 기록한다. PASS는 실제 실행한 검사에만 붙인다. fixture 기반 전체 검증을 모든 OS/provider에 대한 완전성 보장으로 표현하지 않는다.
