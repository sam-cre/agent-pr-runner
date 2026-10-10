---
name: publish-with-runner
description: Publish a finished change through the operator-owned agent-pr-runner (stage, commit, PR, CI, squash-merge) without running Git write commands. Use whenever a change is ready to commit, push, open as a pull request, or merge in a repository that uses agent-pr-runner.
---

# Publish with the runner

The repository's agent instructions name the runner executable, its config, and its queue. If
they do not, stop and ask the operator. Never fall back to Git write commands.

## Publish

1. Make sure the change is complete and is one logical change.
2. Run every verification gate the repository's instructions list. Record each command and its
   real result. Do not publish with a failing gate.
3. Run `<runner> publish <config>` with:
   - `--message "fix: what changed"`: one line, semantic type first, at most 120 characters.
   - `--summary "..."`: what changed and why, one per point (or `--summary-file FILE`).
   - `--verify "check=result"`: one per gate from step 2, with observed results.
   - `--trace "..."`: what the change answers (issue, plan item, owner request).
   - `--exclude PATH` for each changed file that is not part of this change.
   Publish takes the files from `git status` and fills in HEAD, branch, and id itself.
4. If it prints `problem` lines, apply each `fix:` and run it again. Nothing was queued.
   `warning` lines do not stop it, but mention them if the request later fails.

Never include co-author lines, "generated with" lines, AI product names, or any attribution in
any text. The runner refuses them.

## Wait

Run the `next:` command publish printed (`status ... --wait 300`). Repeat it while the receipt's
`action` is `wait`. Each call returns within five minutes.

## Act on the receipt's `action`

- `done`: report the PR URL.
- `fix_code`: read `detail` and `diagnostic_log`, fix the cause, rerun the gates, then run `next`
  with the new `--verify` results.
- `fix_request`: fix what `detail` names, then run `next`.
- `resume`: run `next` unchanged. The commit already exists.
- `report_to_operator`: stop and report `detail`. Do not retry.

## Operator-only files

If the change needs a CI workflow, `.git*` file, `CODEOWNERS`, or a path the config protects,
exclude it, write the proposed file outside the repository, and ask the operator to apply it.
