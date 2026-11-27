This is the unedited generated draft for v0.2.18..1093745b263ed68965e7f7b4bd7cef6aa7490591.

The run completed successfully with 63 calls, 654,255 input tokens (103,997 cached input tokens), and 64,195 output tokens. One investigation was reused. All 75 commits have coverage decisions; five remain unresolved.

Reviewer observations
- The new modifier support, Secure Input warning and app-launch recovery are represented.
- The alert-icon bullet is probably too minor for standalone release notes.
- Several bullets retain more layout mechanics than the preferred editorial style.
- The release boundary was verified against GitHub's latest published release and the pinned main revision. The model's request to confirm this is unnecessary.
- Commit 49ac06a only adds four comment lines in src/testing/sim/mac.rs, so its unresolved production-impact question can be dismissed.
- Parser compatibility and behavior of the previous hotkey dependency need further review before claiming a complete compatibility assessment.
- The current main advanced during generation to c87edd2, a .gitignore-only change with no release-note content. The generated packet remains pinned to the recorded source.

The architecture can produce useful notes, but investigation is expensive and noisy. Start each PR investigation with its actual first-parent diff, reserve on-demand reads for missing context, keep stable per-PR caches, and move cross-PR consolidation to the editor. Give each stage only the applicable instructions. The current investigator receives changelog integration instructions it cannot carry out.
