# Audit Nine PR Implementation Plan

**Goal:** Fix the audited input/data/memory/UI hotpaths in9reviewable PR units, with parallel isolated agents and final orchestrator verification.

**Architecture:** Preserve existing visible PTYs, exact runtime/session identity, grants/deadlines, worker-owned I/O and bounded queues. Extend common input acceptance once and reuse for local/cloud paths. Agents own separate worktrees, root applies/reviews patches sequentially; root app/runtime integration takes precedence over convenience abstractions.

**Tech Stack:** Rust2024,egui0.36, existing bounded std channels/SQLite/runtime commands, current Cargo.lock; no new provider daemons.

Approved design: docs/reviews/2026-10-03-full-memory-performance-agent-terminal-audit.md. User explicitly authorized parallel PR implementation. No further design approval required. No restart/stop/native launch. Product delivery version updated by root only; agents keep workspace version inherited.

## Ownership / waves

1. Parallel PR1 (input reliability),PR3(library recovery/persistence),PR6(images). Every agent is in an isolated current-source snapshot worktree. No root shared-source edits before patches collected.
2. Integrate sequentially and create new exact snapshot. Parallel PR2(eligibility),PR4(palette),PR7(cloudDB), using actual PR1/3 interfaces.
3. Integrate and snapshot. Parallel PR5(session drafts),PR8(batch/log),PR9(remotepaste), using actual sender and DB interfaces. Root audits cross-boundary correctness.

## Per-PR acceptance checklist

### PR1 — Prompt delivery
Files: crates/app/src/ui/composer.rs, app.rs; crates/runtime/src/{in_process,event,protocol}.rs; input/queue code only if needed.
- [ ] RED actualdefault queue boundary: no separate CR after refused body;acceptedpastebutrefusedsubmit cannot lose actionablestate.
- [ ] RED staging rejection/stale target retains exact original draft; followup reservation removed only after acceptance.
- [ ] Implement shared operation-ID acceptance/explicit rejected/unknown lifecycle; don't claim accepted means executed/completed or blindly retryunknown.
- [ ] GREEN focused Composer/runtime tests, full affected package tests; notify root concrete API.

### PR2 — AI eligibility / draft guard
Files: fleet.rs, ui/fleet.rs, app.rs, runtime/input checks as needed.
- [ ] RED ordinaryshell/fallbackshell selected as AIbroadcast; runtime/AI generation change after selection.
- [ ] Implement default live-AI filter plus actual admission-time revalidation; preserve explicit manualshelltyping.
- [ ] Guard existing agent input draft/dialog when reliable supported evidence exists; fail closed automaticAI paths if not known.
- [ ] GREEN active/ended/unclassified/structured/exact target tests, no userPTYexecution.

### PR3 — Library recovery / asyncsave
Files: prompt_library.rs,new boundedworker module,main.rs/app.rs wiring; i18n keys if needed.
- [ ] RED corrupt and validempty library startup overwrite; boundedload errors preserve bytes.
- [ ] Missing/Loaded/Error distinction,onlymissing seed,originalpreserved,atomicwrites and UI-visibleerror.
- [ ] Bounded coalescing asyncsave with revision/order/shutdown semantics,never oldercompletion overwrite newerrevision.
- [ ] GREEN realtempfiles,read-only/oversize/conflict/order tests.

### PR4 — Palette performance
Files: ui/prompt_palette.rs,app.rs; prompt_library interface only coordinated.
- [ ] Repeatedquery/libraryrevision test or counted work proves cache needed.
- [ ] Query/revision resultreuse,virtualizedvisible rows,draftborrow,parameterpreviewcache bounded and invalidated.
- [ ] Preserve searchUnicode behavior/save/select/insert/dialog actions; don't duplicate entire normalized bodies to trade speed for unbudgeted memory.
- [ ] GREEN focused UI tests and rerun baseline100/1000promptsearch metrics with scope/allocator clearly stated.

### PR5 — Draft sessions / persistence / budget
Files: ui/composer.rs,new worker/store ifneeded,app.rs context/lifecycle wiring.
- [ ] RED sameworkspace differentterminaldraftcrosses;recoveryafterrestart with actualfixture;typing bypasses limit.
- [ ] Session stable key includes actual persistentidentity/runtimegeneration handling; boundedload/save,aggregatebudget,dirtydraftnever silentlyevicted.
- [ ] Preserve hiddenworkspace drafts; prune only confirmeddelete,not incompleteboundedprojection.
- [ ] GREEN sessionswitch,closed/restored target,lateattachment,limitUnicode,savefailure tests.

### PR6 — Markdown images
Files: ui/markdown_viewer.rs,image worker/loader module ifneeded,App ownershipwiring asneeded.
- [ ] RED file grows afterstat;changingtext rereads identicalimage;totaldocumentbyte/pixelbudget exceed.
- [ ] Handle-based limit+1read/type/pathvalidation,bounded asyncI/O,docgeneration checks,fileidentitycache,bytes/pixels/workercaps.
- [ ] GREEN localfixture50images,newrevision/closedoc/stalecompletion andresourceforget tests.

### PR7 — Cloud history worker
Files: cloud_agent.rs,agent-mcp/src/history.rs,new boundedworker/module/App pumpwiring.
- [ ] RED exclusiveSQLite fixture blocksApppath; claim ordering/expiry/revoke whilepending.
- [ ] Async claim/finish/list with bounded admission andcompletionwake;durableclaimbeforeinput;allgrant/token/deadline/exacttargetrechecksaftercompletion.
- [ ] Preserve at-most-once tombstones,unknown results not retried,notifyoriginalsession answers;nohiddenagents.
- [ ] GREEN MCP/App tests including actuallockedfixture whileUIpollprogresses.

### PR8 — Batch wake / logs
Files: app.rs batchpump,storage/src/logs.rs,runtime loggingcallsite onlyifmeasurementjustifies.
- [ ] RED busyworker immediate repeatedrepaint;metadata/chunkcountedloglength fixture.
- [ ] Completion/deferredwake with promptsharing;writer-owned lengthaccounting/limitedbatch beforedecidingnewlogthread.
- [ ] Preserve redaction,ANSIboundary/order/tailcap,shutdownflush,errorhandling.
- [ ] GREEN busyfixtureprogress/repaints andlogappend/compaction/resumecounters.

### PR9 — Remote paste
Files: agent-mcp/src/{lib,tests}.rs,cloud_agent.rs,sharedsender/contracts ascoordinated.
- [ ] RED explicitpaste multiline/CRLF/Korean/emoji sizebounds andoneSubmit atomicadmission.
- [ ] Preserve send_text8KiB/control rejectioncontract,add explicitboundedpaste/submit tool using shared encoding/provider/admission.
- [ ] Visibleexactoriginalsession,permissions/deadline/revoke/existingdraftguard;never spawnhiddenagent. Turnread/wait only if actualprovider-boundary support can preserveboundedtrustedoutput;latest-screen remains accuratelynonlossless.
- [ ] GREEN protocol/controlsanitization/operationid/unknown/retrytests andguide.

## Orchestrator final gates
- [ ] All9 PR-specific reports actualcommands/RED/GREEN/failures/remaining recorded; source reviewed, not just summaries.
- [ ] Sequential integration against originalsnapshot, allpreexistingdirtychangespreserved; scopedcommit/patchmanifest perPR.
- [ ] Final independent read-only Codex CLI source review;fix substantivefindings and rerunaffectedgates.
- [ ] Fresh actualaffectedpackage/fullApp/runtime/PTY/storage/MCP tests,format/diffchecks,meaningfulperformanceprobes;no mirroredtests/nativeappstart.
- [ ] Minor version increase ifnewuser-visiblefeatures delivered;verify latestlocalversions and canonical/lock/compiled/bundlefields beforedelivering rebuild.
- [ ] Release rebuild/package withoutlaunch, sourcehash/version/artifactverification, handoffandchecklistfinal.

Exact test pattern: `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo <filter> -- --test-threads=1`;runtime/pty/storage/agent-mcp same command withoutbinaryselector. Root finalgate maybatchindependentpackages;agentsuse isolatedworktrees/sharingtargetonlywithrootbuildscheduler to avoidrebuildcontention. No scripts/dev-run.sh.
