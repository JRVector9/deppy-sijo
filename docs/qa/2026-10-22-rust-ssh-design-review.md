# Rust SSH 접근 방식 및 현재 설계 검토 — 2026-10-22

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/ssh-transport/src/worker.rs | 일반 host key 진단만 해석 | 정상 host certificate 서버 연결 실패 | 첫 host fingerprint만 사용하도록 수정·검증 완료 |
| medium | crates/ssh-transport/src/worker.rs | SHA256 출력 옵션 누락 | 사용자 FingerprintHash md5 설정과 충돌 | CLI SHA256 고정·실제 OpenSSH 검증 완료 |
| medium | crates/ssh-transport/src/worker.rs | LogVerbose 설정이 진단 형식을 변경 | 정상 인증 뒤 host proof 누락 | LogVerbose=none 고정·실제 연결 검증 완료 |
| medium | crates/ssh-transport/src/worker.rs | -G와 실행의 명령 context가 다름 | Match command/sessiontype 계정 해석 불일치 | 동일 공통 옵션·명령으로 수정·검증 완료 |
| medium | scripts/dev-run.sh | askpass 빌드·helper 리소스 준비 누락 | 개발 실행에서 인증/최초 설치 실패 | build-only 준비 경로 수정·23개 검사 통과 |
| low | crates/ssh-transport/src/worker.rs | 유휴 루프가 2ms 고정 주기 | 연결 수에 비례한 wake 가능성 | 실측 후 readiness 대기 변경 검토 |

현재는 **OpenSSH나 SSH 암호화 프로토콜을 새로 구현하지 않는다**. `SshLaunchOptions::new`는 `/usr/bin/ssh`를 선택한다. Rust 어댑터는 프로세스·입출력·사용자 인증 질문·취소·범위 검증을 관리한다. Linux의 `deppy-remote-host`는 SSH 서버가 아니라 SSH stdio를 통해 PTY/파일/작업 세션을 관리하는 앱 구성요소다. 별도 공개 포트를 열지 않는다.

## 조사 방법과 비교 기준

2026-10-22 GitHub 원본 Cargo/source/API와 공식 문서를 확인했다. 별점·검색 요약을 성능 증거로 쓰지 않았다. 비교 기준은 macOS에서 기존 OpenSSH 설정/agent/ProxyJump를 유지하면서 새 crypto/native dependency와 별도 프로세스를 얼마나 추가하는지, 사용자 인증 질문과 소유 프로세스 취소를 지원하는지다. 바이너리 크기·메모리·CPU의 동일 조건 벤치마크는 수행하지 않아 절대적으로 가장 작다고 단정하지 않는다.

| 후보 | 확인 버전·고정 소스 | 구조 | 현재 서비스 판단 |
| --- | --- | --- | --- |
| [openssh-rust/openssh](https://github.com/openssh-rust/openssh/tree/17afe72ff6166dc4547544dbd8fd18b5c978b5d1) | 0.11.6 · 17afe72 · 2026-06-19 | 시스템 OpenSSH wrapper, 기본 process-mux | 가장 가까운 경량 참고 사례; API 그대로 대체는 부적합 |
| [rust-lang/ssh2-rs](https://github.com/rust-lang/ssh2-rs/tree/0a11bdd4eecf4525e540c30f14b15ab1c07194b7) | 0.9.6 · 0a11bdd · 2026-10-06 | libssh2 C binding | OpenSSL/zlib/native build 및 수동 config/trust 통합 부담 |
| [Eugeny/russh](https://github.com/Eugeny/russh/tree/e5d80a07dc554480b6ea3b2e14adfc190407b627) | 0.64.1 · e5d80a0 · 2026-10-05 | Tokio 기반 Rust SSH 구현 | 자체 crypto/protocol stack 도입; 현재 macOS 전용 연결보다 넓은 범위 |
| [async-ssh2-tokio](https://github.com/Miyoshi-Ryota/async-ssh2-tokio/tree/a1bc09aab309b17165ec7876dbe51814118ee9fe) | 0.13.0 · a1bc09a · 2026-07-12 | russh 상위 wrapper | 작은 호출 API가 작은 전체 dependency 구성을 뜻하지 않음 |

`openssh`의 [Cargo](https://github.com/openssh-rust/openssh/blob/17afe72ff6166dc4547544dbd8fd18b5c978b5d1/Cargo.toml)는 필수 직접 의존성6개와 optional2개를 선언한다. 현재 `ssh-transport`는 외부 `libc`와 내부 의존성 없는 `ssh-askpass`만 사용한다. 이는 선언 수 비교이며 transitive dependency나 실제 artifact 무게 측정치가 아니다.

`openssh`의 [연결 builder](https://github.com/openssh-rust/openssh/blob/17afe72ff6166dc4547544dbd8fd18b5c978b5d1/src/builder.rs#L360)는 ControlMaster와 BatchMode=yes를 사용하고 기본 ControlPersist는 무기한이다. 인증 팝업과 포그라운드 stdio의 수명 관리를 그대로 제공하지 않는다. 이를 도입해도 우리 인증/취소 어댑터가 필요하고 연결 모델도 변경된다. 현재 한 SSH stdio에서 여러 pane을 다루므로 여러 remote command용 mux가 반드시 필요한 것도 아니다.

`ssh2-rs`는 [같은 Session의 blocking read가 다른 호출도 막는 제약](https://github.com/rust-lang/ssh2-rs/blob/0a11bdd4eecf4525e540c30f14b15ab1c07194b7/src/session.rs#L110)이 있으며, known-host 검사도 호출자가 구성한다. `russh-config`의 [실제 parser](https://github.com/Eugeny/russh/blob/e5d80a07dc554480b6ea3b2e14adfc190407b627/russh-config/src/lib.rs#L222)는 OpenSSH 전체 config 동작을 대체하지 않는다. 라이브러리 교체만으로 현재 서비스 요구가 더 단순해진다는 근거는 찾지 못했다.

## 발견한 호환성 결함

OpenSSH [sshconnect.c](https://github.com/openssh/openssh-portable/blob/26b1363c7cbf5d01c730f45fc0b4abc5686beaf7/sshconnect.c#L1507)는 일반 키와 host certificate를 서로 다른 진단문으로 출력한다. 수정 전 parser는 전자만 받았다. 인증서 줄의 첫 지문은 원래 host 공개키의 지문이고, 뒤에 CA의 지문이 별도로 있다. [sshkey_fingerprint_raw](https://github.com/openssh/openssh-portable/blob/26b1363c7cbf5d01c730f45fc0b4abc5686beaf7/sshkey.c#L976)는 plain key로 계산하므로 같은 키의 인증서 재발급에 유지된다. 정확한 prefix/첫 필드/쉼표를 처리하고 CA·ID에서 다른 SHA256을 검색해 대체해서는 안 된다.

[FingerprintHash 설정](https://github.com/openssh/openssh-portable/blob/26b1363c7cbf5d01c730f45fc0b4abc5686beaf7/ssh_config.5#L874)은 md5도 허용한다. 앱의 SHA256 identity와 맞추려면 해당 프로세스의 출력 옵션만 SHA256으로 고정해야 한다. host 검증 정책을 약화하거나 사용자의 SSH 설정 파일을 바꾸는 방식은 필요하지 않다.

`LogLevel=DEBUG1`만 고정해도 사용자 `LogVerbose *`는 [진단에 source/function/line 태그를 붙인다](https://github.com/openssh/openssh-portable/blob/26b1363c7cbf5d01c730f45fc0b4abc5686beaf7/log.c#L452). 실제 CA 인증서 loopback에서 지문 누락을 재현했고 `LogVerbose=none`을 프로세스에만 적용했다. 임의 태그를 허용하도록 parser를 넓히지 않았다.

또한 최신 OpenSSH의 [Match command/sessiontype](https://github.com/openssh/openssh-portable/blob/26b1363c7cbf5d01c730f45fc0b4abc5686beaf7/readconf.c#L850)은 명령 유무에 따라 설정을 다르게 해석한다. 설치된 클라이언트 `OpenSSH_10.3p1`에서 격리 config의 `ssh -G`는 명령 없이 실행할 때와 실제 probe 명령을 넣을 때 서로 다른 account를 반환했다. production adapter의 argv 회귀도 수정 전에 실패했다. `-G`에도 동일 공통 옵션과 typed command를 전달하도록 수정했다. 단계별 명령이 의도적으로 서로 다른 서버/계정으로 라우팅되는 설정은 동일 workspace identity로 자동 병합하지 않는다.

개발 실행의 asset 누락은 별도로 실제 코드에서 확인했다. build-only 단계에서 companion과 검증된 두 Linux helper를 준비하도록 수정했다. 사용자 앱을 재실행해서 확인하는 작업은 이번 권한에 포함되지 않는다.

## 결정과 후속 검증

1. 시스템 OpenSSH 재사용을 유지한다. 새로운 SSH protocol stack이나 중복 연결 backend는 도입하지 않는다.
2. 인증서·SHA256 두 결함은 mock과 실제 소유 loopback에서 RED→GREEN을 확인했다. 추가 LogVerbose와 -G context도 실제/argv RED를 확인한 뒤 최소 수정했다. dev build asset 준비는 앱 실행 없는 23개 Python 검사에서 통과했다. 모든 최종 변경을 포함한 전체 workspace·strict Clippy·fmt·구조/의존성/i18n 10개 gate가 통과했다. App3345개와 no-default App3342개가 각각0FAIL이며, 최종 실제 Host/Linux 및 설치 동의 검사도 각각1PASS다. 기본 기능 노출 후 별도 7개 gate도 모두 통과했으며 실제 명령과 결과는 아래 검증 보고서에 기록했다.
3. 2ms 루프는 nominal 약500회/초의 대기를 요청한다는 정적 계산이다. 실제 wake·CPU는 미측정이다. readiness 대기와 명시적 cancel/write wake는 별도 실측 후 성능 PR 후보로 남긴다.
4. 기존 세션·PWA·범위·재접속·비밀 전달 테스트를 유지하고 최종 coherent source의 회귀 검사를 마친 뒤 App Cargo와 패키징의 기본 SSH 노출을 함께 활성화했다. 일반 기본 빌드 App3345개, strict Clippy 및 패키징 검사를 포함한 7개 gate가 모두 통과했다. 라이브러리를 추가하지 않았다는 사실을 성능 우월 증거로 확대하지 않는다.

앱 출시·실행·재실행·사용자 서버/키/DB 수정은 하지 않았다. 전체 기능 진행 상황과 실제 최종 결과는 [검증 보고서](ssh-workspace.md) 및 `docs/CODEX_HANDOFF.md`를 따른다.

검토는 2026-10-22에 시작했고 후속 재현·수정 검증은 한국 시간 2026-10-23까지 이어졌다. 파일명과 고정 소스 링크는 최초 조사 기준을 유지한다.
