# Launcher height transition

User authorized a natural launcher modal size transition during the requested rebuild.

Design: keep the dedicated 620pt two-column layout and fixed-size controls. Measure natural body/error height independently of animated allocation. Animate only the allocated body height over 200ms using existing egui interpolation, clip content to the visible body, keep the footer outside the clip. Reset on each open; request frames only while the target changes or the animation runs. No worker, timer, dependency, execution or selection-policy changes.

Alternatives considered: fixed maximum height removes resizing but leaves excess space; min-height-only interpolation still jumps on growth. Selected measured-and-clipped allocation supports both directions and interrupted transitions.

- [x] Reproduce the instantaneous resize with actual headless Modal geometry.
- [x] Add bounded transition and reopening reset.
- [x] Verify actual growth/shrink/rapid retarget/footer/width and existing launcher tests.
- [x] Review diff, fmt/Clippy/boundary; record evidence and rebuild 0.18.0 without launch.

Executed final launcher28tests PASS, fullApp3777unit+39integration PASS, strictClippy5affectedpackages/alltargets/fmt/boundary PASS. Independent review exposed an actualcachedArea centerjump onreopen (RED27PASS1FAIL). Forcedsizingpass introduced3ptnatural-heightdifference (hidden152vsnormal155); removed it, kept oneopening-onlyrequest_discard and allheight/center/footer assertions. No addedtimer/dependency/selectionorlaunchpolicychange. 0.18.0native/Linuxbuild+package/signature/version/resource verificationPASS; noGUIlaunch. Pixelcharacter target remains a separatependingclarification, not implemented orclaimed.


Subsequent user correction (2026-10-23): pixel-character request canceled; no character code was implemented. Earlier pending-clarification statements are historical and superseded.
