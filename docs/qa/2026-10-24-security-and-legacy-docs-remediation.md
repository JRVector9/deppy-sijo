# Security dependency and legacy document remediation — 2026-10-24

Status: **Both requested fixes complete; corrected0.19.1 local bundle built, signed and verified. Local and remote security checks PASS. No app launch/restart.**

## Root causes and minimum fixes

The three failures in main's dependency-security CI were current RustSec advisories in locked dependencies; no audit or protected usage gate was changed. Local `cargo audit --json --file Cargo.lock` reproduced exit1 and3vulnerabilities before edits, then exit0 and0vulnerabilities after the three compatible patches. Exact before/after JSON and source metadata are retained under `assets/local-0.19.1-20261024/`.

| Package | Before | After | Official security reference |
| --- | --- | --- | --- |
| ctutils |0.4.2|0.4.4|[Maintainer security release](https://github.com/RustCrypto/utils/pull/1569), RUSTSEC-2026-0385 |
| rustls-webpki |0.103.15|0.103.16|[RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382.html) |
| webpki-roots |1.0.8|1.0.10|[RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381.html) |

All selected patches are non-yanked and support the repository's Rust1.96.1 (crates.io metadata checksums recorded). Complete parsed lock equality after normalizing those3version/checksum pairs and2parent reference labels confirms no other dependency update. The0.26.11 compatibility roots package now re-exports1.0.10. Audit warnings about existing unmaintained crates do not represent these fixed vulnerabilities and are not suppressed.

Source commit `de785879f57eb53cd691380144400f7da892a183` prepares patch release0.19.1 (previous shipped0.19.0), root workspace version and30inherited lock versions. No Rust behavior source was edited. Prior0.19.0 bundle is untouched; corrected0.19.1 artifact has now passed actual build/sign/package/version verification.

## Legacy worktree disposition

Original `/Users/jr/Desktop/projects/deppy-sijo` was on older `feat/fleet-one-list-and-relay-wip`, source331b78b3, with exactly8dirty document/mockup paths. That old source is NOT an ancestor of main, so it was not merged. All8original contents and binary patch were hash-verified in a private0700maintenance backup/0600archive before preservation. No source file was deleted.

- Commit `a789466bbf9def2e1791cc9abd76eafd49214368` preserves all8paths on the old branch. The original full handoff is retained byte-for-byte as a suffix after a current-source pointer. Exact normal-push remote SHA and clean old worktree were verified.
- Three unique HTML mockups and one already-superseded bridge plan copied byte-for-byte to [historical archive](../archive/legacy-fleet-202609/README.md); all286added historical handoff lines and original patch also preserved. Archive README explicitly marks these as historical, not active implementation instructions.
- The two review documents already in main are byte-identical, so main copies remain unchanged. The old AGENTS mandatory-version addition was already exactly present in main; current model/provider/restart rules were not overwritten.
- `manifest.json` maps every original path/hash/disposition and source commit. Pattern scan of the8documents found no candidate provider/GitHub/private-key/AWS credential values; no live account workflow was tested or claimed.

## Actual checks so far

- Local audit RED:3vulnerabilities, exit1. GREEN after compatible dependency patches:0vulnerabilities, exit0; repeated again after0.19.1 workspace version bump PASS.
- cargo deny --offline --locked check bans licenses sources PASS; no license/source/bans policy change.
- cargo metadata --offline --locked --no-deps PASS; provider source --check-only PASS without resealing.
-8original-path hashes,4exact historical copies,2identical review copies,286handoff lines, private backup bytes, original handoff suffix and clean/pushed original worktree actually checked PASS.
- Native cargo test --workspace actually PASS656.45s: 6641normal passes/79ignored across95suites, excluding nested child fixture summary duplicates. Full strict workspaceClippy PASS134.86s, fmt PASS2.86s, boundary PASS6.35s and native app/proxy/askpass build PASS71.20s. Native CLI0.19.1 and1173input digests verified. Source de785879 pushed to main and exact remote SHA verified. Both Linux helper architectures actually PASS:74normal tests/3ignored each,148executionsPASS/6ignored combined. Combined helper build/test duration1544.81s. All use hash-fenced1173inputs. Artifact signing/version verification PASS.

## Failed tooling checks retained

The first graph comparison classified unchanged ureq3.4.0 as a changed package because its roots version reference changed and could not preserve duplicate roots package names. It stopped before running GREEN audit. Replaced by complete lock normalization preserving both roots versions, then exact equality PASS; no extra package changes were needed. cargo-nextest is absent locally (probe only), so native verification uses built-in cargo test. No nextest execution claimed.

## Remote security confirmation

[Dependency-security run40015043988](https://github.com/JRVector9/deppy-sijo/actions/runs/40015043988) completed **SUCCESS** on source `de785879f57eb53cd691380144400f7da892a183`. Both “RustSec audit” and “Licenses, sources, and duplicate versions” jobs passed. Exact job records: `assets/local-0.19.1-20261024/security-ci-fixed.json`. This fixes the originally reported security-CI failure without disabling or suppressing checks. Native full-workspace checks passed locally as recorded; no claim about unrelated full GitHub build/test completion is implied.

## Corrected local artifact and final preservation verification

Version increase:0.19.0→0.19.1. Artifact: `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/target/bundle-0.19.1/Deppy Sijo.app`. Existing67bundle scan maximum0.19.0 verified before creating this new bundle. No old artifact overwritten.

App CLI, CFBundleVersion and CFBundleShortVersionString all0.19.1. Both included static Linux SSH helpers report0.19.1 and have verified manifests/hashes. Signed with existing Developer ID, package verifier PASS under explicit local developer trust policy; debug-profile local build, not notarized public distribution.10signed-bundle/resource file hashes and1173build input hashes match. No helper installed on a user server. Old0.16.0 app PID31144 remains running; no GUI launch/restart performed.

Final independent recheck: fresh audit0vulnerabilities; protected usage source verifier PASS without reseal; original worktree clean and exact local/remotea789466b match; all8private-backup original hashes valid;4historical copies and2duplicate main review docs still byte-identical; original full handoff is retained as a suffix and286historical additions are preserved. Source commitde785879 and old document commita789466b already pushed; this final main documentation/archive/evidence commit follows.

Evidence files in `assets/local-0.19.1-20261024/` include exact commands, before/after audit JSON, crates.io release metadata/checksums, complete dependency delta, native95suite parent counts, Linux build/test logs, successful remote security jobs, source/bundle hashes, original document manifests, final verification and SHA256 index. Logs and metadata are retained losslessly as gzip. Earlier5359test results were not reused as this task's test result;6641native passes are this task's actual fresh full-workspace run. Manual/live/browser/performance ignored tests remain excluded. No unresolved requested source fix remains.

Final documentation-format correction: staged diff check flagged the archived handoff original blank EOF line. Preserved all286lines verbatim as a hash-verified prefix and appended a clearly marked HTML end marker; original patch remains exact. No original line removed and no whitespace gate disabled.
