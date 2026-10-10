<!--
`agent-pr-runner snippet CONFIG.json` prints the part below the line with the real paths filled in.
To do it by hand, paste everything below the line into the target repository's agent instructions
file (AGENTS.md, CLAUDE.md, GEMINI.md, .cursorrules, or similar) and replace <RUNNER>, <CONFIG>,
and <QUEUE_DIR> with this repository's installed runner, config, and queue.
-->

---

## Publishing changes (Git runner)

An operator-owned runner commits, opens the PR, waits for CI, and merges. You never run Git write
commands (`add`, `commit`, `push`, `merge`, `rebase`, `reset`, `switch -c`, `stash`, `gh pr`).
Read-only Git is fine.

1. Make one logical change. Run the repository's gates and note each real result.
2. Publish. It takes every changed file from `git status`, fills in HEAD, branch, and id, checks
   everything, and queues the request. It returns at once.

```
<RUNNER> publish <CONFIG> --message "fix: what changed" --summary "Why, in one line." --verify "cargo test=212 passed" --trace "Issue or plan item"
```

   - Repeat `--summary`, `--verify`, and `--trace` for more lines. `--exclude PATH` leaves a file
     out. `--dry-run` shows the request without queuing it.
   - Message: one line, semantic type first (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`,
     `test:`, `ci:`, `build:`, `perf:`, `style:`). No AI product names or attribution anywhere.
   - If it prints `problem` lines, apply each `fix:` and publish again. Nothing was queued.
3. Run the `next:` command it prints. It waits up to 5 minutes, then prints the receipt or
   `"action":"wait"`. Repeat it while the action is `wait`.
4. Act on the receipt's `action`. When it has a `next` command, run it (after the step below).

| `action` | What you do |
| --- | --- |
| `done` | Report the PR URL. The checkout is back on the updated base branch. |
| `fix_code` | Read `detail` and `diagnostic_log`, fix the cause, rerun the gates, then run `next` with your real `--verify` results. |
| `fix_request` | Fix what `detail` names, then run `next`. |
| `resume` | The commit exists; run `next` to finish it. Change nothing first. |
| `report_to_operator` | Stop and give the operator the `detail`. Do not retry. |
| `unknown_id` | Check the id. |

Operator-only paths (CI workflows, `.git*` files, `CODEOWNERS`, and the config's
`protected_paths`) are refused. Exclude them, write the proposed content outside the repository,
and ask the operator to apply it. To check a hand-written request without queuing it:
`<RUNNER> preflight <CONFIG> request.json`. Status of any request:
`<RUNNER> status <QUEUE_DIR> ID --wait 300`.
