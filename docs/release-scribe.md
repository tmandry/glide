# Release notes evaluation

The manual **Release notes evaluation** workflow maintains a second draft PR on
`release-scribe/evaluation`. Do not merge that PR. The existing release-please and
packaging workflows continue to prepare and publish releases normally.

Before running, add `OPENAI_API_KEY` in repository Settings → Secrets and variables
→ Actions. The existing `RELEASE_PLEASE_TOKEN` must have read access to the private
`tmandry/release-scribe` repository so Actions can check out the pinned tool.
No key belongs in configuration files, pull requests, comments, or Git history.

After this workflow is merged to main, run it from the Actions tab:

1. Leave **apply** unchecked to collect and validate the evidence without a model
   call or GitHub writes. The run log reports the pinned source and cache key.
2. Select **apply** for one Sol call on a cache miss and an update to the isolated
   PR. Exact hits reuse the validated response from `release-scribe-state-evaluation`,
   including on a fresh runner. No automatic paid retry or fallback is enabled.
3. Review the prose in the PR body, `.release-scribe/proposed.md`, coverage, and
   reviewer questions. Comments and reviews are considered on the next manual run.

The model is `gpt-6.1-sol` through the OpenAI Responses API, using high reasoning
effort and a 24,000 output token limit. It receives the selected source and PR
discussion. This uses OpenAI API billing, not Copilot quota. The config lives in
`.github/release-scribe/config.json`; editorial rules live in `release-notes.md`.

The PR starts as a draft. Automation preserves its readiness state and never
merges it. Human changes to either the managed body block or `notes.md` are kept;
new wording goes in `proposed.md` for review. Preserve the marker comments and
state file. Unrelated changes on the evaluation branch cause an error rather than
being overwritten. A stale source or new feedback leaves generation cached and
aborts the PR update.

The evaluation guard intentionally fails. Its status must be required in repository
rules for enforcement beyond the draft state; an optional check cannot prevent an
administrator from merging. No release labels, version changes, changelog edits,
tags, releases, or source-PR comments are produced by this evaluation.

The initial preview is seeded from the reviewed Sol draft, not a fresh key-backed
API call. The first applied workflow run will generate against current main.
