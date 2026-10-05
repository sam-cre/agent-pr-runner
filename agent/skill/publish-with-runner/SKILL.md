---
name: publish-with-runner
description: Publish a finished change through the operator-owned agent-pr-runner (stage, commit, PR, CI, squash-merge) without running Git write commands. Use whenever a change is ready to commit, push, open as a pull request, or merge in a repository that uses agent-pr-runner.
---

# Publish with the runner

The repository's agent instructions name the runner executable and queue folder. If they do not,
stop and ask the operator for both paths. Never fall back to Git write commands.

## Before submitting

1. Make sure the change is complete and is one logical change.
2. Run every verification gate the repository's instructions list. Record each command and its
   real result. Do not submit with a failing gate.
3. Collect the exact list of changed paths for this change only:
   `git status --porcelain` and `git diff --name-only`. Leave out unrelated files, scratch files,
   and anything the operator owns.
4. Read `git rev-parse HEAD` (full SHA) and `git branch --show-current`.

## Write the request

Write JSON to a file outside the repository:

- `id`: new, unique, descriptive, such as `fix-login-timeout-1`. Never reuse one.
- `expected_head`: the full SHA from step 4.
- `branch`: neutral and purpose-based, such as `fix/login-timeout`. No AI product names.
- `create_branch`: `true` from the base branch; `false` to add to an existing PR branch.
- `files`: the paths from step 3, relative to the root, exactly as Git prints them.
- `commit_message` and `pr_title`: one line, semantic type first (`fix: ...`), at most 120
  characters, the same text in both unless there is a reason to differ.
- `summary`: what changed and why, one short point per entry.
- `verification`: one `{ "check", "result" }` row per gate from step 2, with observed results.
- `traceability`: what the change answers (issue, plan item, owner request, doc section).

Never include co-author lines, "generated with" lines, AI product names, or any attribution in
any field. The runner refuses them and nothing is published.

## Submit

Run `<runner> submit <queue> <request.json>` with the longest timeout your shell allows. It blocks
until the runner writes a receipt (CI can take 30 minutes or more). If the shell times out, poll
with `<runner> status <queue> <id>` every few minutes until the status is final.

## Read the receipt

- `merged`: report the PR URL. The checkout is on the updated base branch.
- `needs_fix`: open `diagnostic_log`, fix the cause, rerun the gates, and submit a new request with
  the same branch, `create_branch: false`, the current HEAD, and a new id.
- `error`: if no commit was made, fix the cause and resubmit with a new id. If the commit was
  already pushed (the branch exists on GitHub or `git log` shows the runner's commit), resubmit
  with `resume: true`, `files: []`, `create_branch: false`, current HEAD, and a new id.
- `merged_needs_refresh` or `needs_inspection`: stop and report to the operator with the detail.

## Operator-only files

If the change needs a CI workflow, `.git*` file, `CODEOWNERS`, or a path the runner config
protects, write the proposed file outside the repository and ask the operator to apply it.
