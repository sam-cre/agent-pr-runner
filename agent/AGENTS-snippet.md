<!--
`agent-pr-runner snippet CONFIG.json` prints the part below the line with the real paths filled in.
To do it by hand, paste everything below the line into the target repository's agent instructions
file (AGENTS.md, CLAUDE.md, GEMINI.md, .cursorrules, or similar) and replace the paths in angle
brackets with this repository's installed runner and queue.
-->

---

## Publishing changes (Git runner)

This repository publishes through an operator-owned Git runner. You never run Git write commands.

- Runner: `<INSTALL_DIR>/agent-pr-runner` (`.exe` on Windows)
- Queue: `<INSTALL_DIR>/queues/<project>`

### Rules

- Never run `git add`, `commit`, `push`, `merge`, `rebase`, `reset`, `checkout -b`, `switch -c`,
  `branch -D`, `tag`, `stash`, or `gh pr create/merge`. Read-only Git (`status`, `diff`, `log`,
  `rev-parse`, `show`) is fine.
- One logical change per request, one request per PR.
- Run this repository's verification gates first, and report their real results as evidence.
- Branch names are neutral and purpose-based: `feature/...`, `fix/...`, `docs/...`, `chore/...`.
- Commit messages and PR titles are one line and start with a semantic type: `feat:`, `fix:`,
  `docs:`, `chore:`, `refactor:`, `test:`, `ci:`, `build:`, `perf:`, `style:`.
- Never put AI product names, co-author lines, "generated with" lines, or any other attribution in
  a branch name, commit message, PR title, or PR text. The runner refuses them.
- Never ask the operator to run Git commands for you. If the runner fails, read the receipt and
  follow the table below.

### Publishing a change

1. Run the gates. Note each command and its real result.
2. Read the current state: `git rev-parse HEAD` and `git branch --show-current`.
3. Write a request file **outside the repository** (a temp or scratch folder), for example
   `request.json`:

```json
{
  "id": "fix-parser-empty-input-1",
  "expected_head": "<full 40-character SHA from git rev-parse HEAD>",
  "branch": "fix/parser-empty-input",
  "create_branch": true,
  "files": ["src/parser.rs", "tests/parser.rs"],
  "commit_message": "fix: return an empty document for empty input",
  "pr_title": "fix: return an empty document for empty input",
  "summary": ["What changed and why, one point per line."],
  "verification": [{ "check": "cargo test", "result": "212 passed" }],
  "traceability": ["Issue, plan item, or request this change answers"]
}
```

   - `id`: new and unique each time (letters, digits, `-`, `_`; at most 80).
   - `create_branch: true` when starting from the base branch. To add a fix to an open PR's branch,
     use `create_branch: false`, the same `branch`, and that branch's current HEAD.
   - `files`: every path to stage, exactly as Git spells it, relative to the repository root. No
     folders, no wildcards. Deleted files are listed too.

4. Submit and wait (this blocks until a receipt arrives; allow up to an hour or more):

```
<INSTALL_DIR>/agent-pr-runner submit <QUEUE_DIR> <path to request.json>
```

   If your shell times out first, the request keeps running. Check it with:

```
<INSTALL_DIR>/agent-pr-runner status <QUEUE_DIR> <id>
```

5. Act on the receipt's `status`:

| Status | Meaning | What you do |
| --- | --- | --- |
| `merged` | PR squash-merged; checkout is back on the base branch, pulled. | Report the PR URL. Start the next change from step 1. |
| `needs_fix` | CI or the runner's local checks failed. `detail`, `failure_kind`, and `diagnostic_log` say why. | Read the log, fix the cause, rerun the gates, then submit a new request: same `branch`, `create_branch: false`, `expected_head` = current HEAD, new `id`. |
| `error` before a commit was made | The request was refused (validation, stale HEAD, protected path). | Fix the request or the cause and submit with a new `id`. |
| `error` after the push | Something failed after the commit reached GitHub (PR, CI wait, merge). | Submit the same request again with `resume: true`, `create_branch: false`, `files: []`, `expected_head` = current HEAD, and a new `id`. |
| `merged_needs_refresh` | Merged, but switching back to the base branch or pulling failed. | Report it to the operator with the detail. |
| `needs_inspection` | The runner stopped mid-request. | Stop and report to the operator. Do not resubmit. |

### Operator-only paths

The runner refuses to stage CI workflows, `.git*` files, `CODEOWNERS`, and the extra paths in its
config. If a change needs one of them, write the proposed content to a file outside the repository
and ask the operator to apply it.
