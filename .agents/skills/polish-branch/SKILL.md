---
name: polish-branch
description: Get a Glide branch ready for review by running an outside code review in a loop, fixing what it finds, and rewriting the branch so each fix lives in the commit it fixes and every commit builds and passes tests. Works with Git and jj. Use before creating a non-draft PR or marking a draft PR ready, after pushing fixes for review comments, or when asked to polish, tidy, or clean up a branch's commits.
---

# Polish Branch

Leave the branch with no review findings you agree with, and with a history where
every commit is atomic, builds, and passes tests. The branch doesn't need a PR.

The steps below use Git. If `jj root` succeeds, the repo uses jj: use jj
commands for every change to history, and follow the jj notes after the
workflow.

## Workflow

1. **Set up.** Read the repository instructions, especially Source Control in
   `AGENTS.md`. Find the base branch (default `origin/main`; fetch it first).
   Commit or set aside uncommitted work with a WIP commit rather than
   `git stash`, since the stash is shared with other worktrees.

2. **Review in a loop.** Ask an outside reviewer to review `BASE...TIP` for
   correctness bugs, read-only. Use the reviewer the user prefers. Otherwise
   use the first one available:
   - Copilot CLI: `copilot -p "/review the changes on this branch against BASE (git diff BASE...HEAD). Report only correctness bugs; do not edit any files." --reasoning-effort low --deny-tool write`,
     plus `--allow-tool 'shell(git:*)'` and similar for read-only tools.
   - The harness's own code-review command or skill.

   Also read any unresolved review comments on the PR, if there is one
   (`gh api repos/OWNER/REPO/pulls/N/comments`, and the latest review body).

   For each finding, check it against the code before acting. Then either fix
   it, with a test when the fix changes behavior, or decide you disagree and
   note why. A finding about code that was already on the base branch is still
   worth fixing when it's cheap, but it's a separate `fix:` commit (see below).
   Run the review again after fixing. Stop when a run has no findings you agree
   with, or after five rounds, and say which.

   Treat the reviewer's "tests pass" or "tests fail" as unverified: it may run
   in a sandbox where some tests can't pass. Run the tests yourself.

3. **Plan the history.** For each fix, decide where its changes belong:
   - It fixes a problem introduced earlier on this branch: squash it into that
     commit, and update that commit's message if the fix changes what it says.
   - It fixes a bug that's already on the base branch: keep it as its own
     `fix:` commit.
   - A fix that touches several commits' code gets split, one piece per commit.

   Also look for commits that should be split before they're reviewed, such as
   one that adds a capability to a lower layer (a `sys` wrapper, a new option
   on a shared type) and also uses it. Split the capability into its own
   `refactor:` or `internal:` commit first.

4. **Rewrite.** Note the old tip's commit hash (`git rev-parse HEAD`), and tag
   a backup: `git branch -f backup/BRANCH HEAD`. Write each fix's changes as a
   patch script, or keep the hunks, so they can be applied at a rebase stop.
   Then run the rebase non-interactively:

   ```sh
   GIT_SEQUENCE_EDITOR="cp /path/to/todo.txt" git rebase -i BASE
   ```

   In the todo file, use `edit` to stop where changes are applied by hand (amend
   with `git commit -a --amend --no-edit`, or `git reset HEAD~` to split a
   commit), `fixup` to squash, and `exec git commit --amend -F msg.txt` to
   replace a message. Leave out the commits whose changes were moved.

   When a change moved earlier conflicts with a later commit, put it somewhere
   the later commit doesn't touch, e.g. add tests at a different place in the
   test module.

5. **Verify.** Run `git diff backup/BRANCH HEAD`. It should be empty, or differ
   only in ways you meant, like where tests sit in a file; explain anything
   else. Then run `scripts/check-each-commit.sh BASE`, which checks out every
   commit in a temporary worktree and checks formatting, the build, and the
   tests. Fix any commit that fails, and say whether any commit adds build
   warnings.

   Once both checks pass, delete the backup with `git branch -D backup/BRANCH`.
   The old commits stay in the reflog, so the old tip's hash still works for
   diffing or restoring. If a check fails and you can't fix it, keep the
   backup and say so.

6. **Publish, if asked.** Only push if the user asked for it, or asked to update
   a PR. The history was rewritten, so use
   `git push --force-with-lease=BRANCH:OLD_SHA`, where `OLD_SHA` is the commit
   the remote was at. If there is a PR, update its description when the fixes
   change what it says, following the repository's PR style. Don't reply to
   review comments or post anything on the PR unless the user asks.

7. **Report.** List the new commits, what each fix was and where it went, the
   findings you disagreed with and why, how many review rounds ran and how the
   last one ended, and the result of the per-commit check. Give the old tip's
   commit hash, so the user can compare against it (`git diff OLD_TIP HEAD`)
   or restore it (`git reset --hard OLD_TIP`).

## With jj

Change history with jj, not Git, so jj rebases descendants and keeps each
change's identity. Read-only Git commands are fine, including the per-commit
check script.

Any jj command that rewrites `@-` leaves Git's `HEAD` detached, and a Git
worktree that has the branch checked out is detached at the branch's old
commit. Tools that assume Git still work while detached, but a `git commit`
then lands on no branch. The last step below puts Git back on the branch.

- **Names:** jj revsets spell the base `main@origin`; Git commands, including
  the per-commit check script, still take `origin/main`. Below, `BASE` means
  whichever the command needs.
- **Set up:** there's nothing to set aside, since the working copy is a commit.
  Give it a description, or start a new empty change with `jj new`. The branch
  is `BASE..@`; `TIP` is `@`.
- **Review:** reviewers that run `git diff BASE...HEAD` miss the working-copy
  commit, because Git's `HEAD` is its parent. Commit the work first
  (`jj new`), so `@-` holds it.
- **Backup:** record the operation ID (`jj op log -n 1 --no-graph -T 'id.short()'`)
  and the tip's commit ID. `jj op restore OP_ID` undoes the whole rewrite.
  There's no backup branch to delete; report both IDs instead.
- **Squash fixes:** try `jj absorb --from FIX` first. It moves each hunk into
  the nearest ancestor that last changed those lines, which is usually the
  commit the fix belongs to; check where each hunk went with `jj log -p`.
  Move what's left with `jj squash --from FIX --into TARGET PATHS...`, or
  apply it in a new change on the target (`jj new TARGET`, edit,
  `jj squash`). Don't use the interactive `-i` forms.
- **Split:** `jj split -r REV -m MESSAGE PATHS...` puts those paths in the first
  commit. To split within a file, make a new change before `REV`
  (`jj new --insert-before REV`), apply the part that goes first, and the
  later commit keeps the rest.
- **Messages:** `jj describe -r REV -m MESSAGE`.
- **Conflicts:** jj rebases descendants automatically and records conflicts in
  the commits instead of stopping. After each step, check
  `jj log -r 'BASE..@ & conflicts()'` and resolve any before going on.
- **Verify:** `jj diff --from OLD_TIP --to @`, then
  `scripts/check-each-commit.sh BASE "$(jj log -r @ --no-graph -T commit_id)"`.
- **Publish, if asked:** `jj git push -b BOOKMARK`. Its safety checks act like
  `--force-with-lease`. Bookmarks follow their commits when they're rewritten,
  but check that the bookmark is on the new tip.
- **Hand back to Git:** start an empty change on the bookmark and attach Git to
  it: `jj new BOOKMARK && git switch BOOKMARK`. This changes no files, since
  `HEAD` is already at the bookmark's commit. In the report, name any Git
  worktree that had the branch checked out (`git worktree list`); running
  `git switch BRANCH` there moves it to the new history if its working tree is
  clean.
