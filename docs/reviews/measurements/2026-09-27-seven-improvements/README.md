# 7개 개선의 재현 자료

`manifest.json`은 소스·바이너리·폰트 hash와 측정 범위를, `summary.json`은 전후3회 raw sample/중앙값을 보존한다. `before/`, `after/`는 최종 교차 실행이고 `stage6/`와 `regressions/`는 단계별 효과 및 RED→GREEN 증거다. `tests/summary.json`은 실제 명령과 exit code를 남긴다. Workspace 기존 실패2건은 의도적으로 보존했다.

## headless 재현

```bash
# 저장소 root에서: Deppy GUI를 실행하지 않는 코드/컴포넌트 검사
python3 docs/reviews/measurements/2026-09-27-seven-improvements/run_tests.py

CARGO_TARGET_DIR="$PWD/target" cargo build --release --manifest-path docs/reviews/measurements/2026-09-27-seven-improvements/probe/Cargo.toml
CARGO_TARGET_DIR="$PWD/target" cargo build --release --manifest-path docs/reviews/measurements/2026-09-27-seven-improvements/renderer-probe/Cargo.toml

# manifest와 일치하는 보관 바이너리 사용. cargo/zig 빌드 종료 후 실행.
python3 docs/reviews/measurements/2026-09-27-seven-improvements/run_comparison.py --binaries /tmp/deppy-seven-comparison-20260927
```

새 환경에서는 별도 detached `78f054b9` 작업 트리에서 같은 probe를 먼저 빌드한다. 해당 트리의 별도 `target`과 path dependencies를 사용한다. 원본과 수정의 target을 공유하지 않는다. 보관된 바이너리가 사라지면 두 source에서 다시 빌드하고 hash manifest도 갱신해야 한다.

측정은 System allocator와 실제 backend/egui API를 사용한다. Snapshot 비용은 PTY feed를 제외한다. native Deppy GUI·macOS IME·앱 RSS/GPU 계측은 실행하지 않았다. GUI runner는 명시적 허락 후에만 사용한다.

Git whitespace 검사에 맞춰 일부 로그의 끝 빈 줄만 정리했다. 내용 행은 변경하지 않았고, 해당 로그의 원본/기록 SHA256을 manifest에 남겼다. 원본은 `/tmp`에 보존한다.
