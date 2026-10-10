# Requests and receipts

## Publish and preflight

Agents normally do not write requests by hand. `publish` builds one:

```
agent-pr-runner publish CONFIG.json --message "fix: what changed" --summary "Why." --verify "cargo test=212 passed" --trace "Plan item 3"
```

- `files`: every path `git status` lists (changed, new, deleted, both sides of a rename), minus
  each `--exclude PATH`. An operator-only path stops it with the `--exclude` fix.
- `branch`: the current branch, or when on the base branch, `--branch` or a name made from the
  message (`fix: handle empty input` becomes `fix/handle-empty-input`).
- `expected_head`, `create_branch`, and `id` come from Git and the queue. The id is the branch
  name plus the first unused number.
- `--retry ID` reuses an earlier attempt's text (kept as `ID.sent.json` in the queue) under a new
  id, and becomes a resume when that attempt's commit already exists. After `fix_code` it needs
  new `--verify` results.

`preflight CONFIG.json REQUEST.json` checks a hand-written request the same way and queues
nothing. Both print one line per problem with its fix, for example:

```
problem operator_only: .gitignore is operator-only; the operator must change it by hand. fix: leave .gitignore out (publish: --exclude .gitignore) and ask the operator to apply it
```

Besides every field rule below, they check: an id already used, `expected_head` against HEAD, the
checkout's branch, staged files, files with no change, the remote against the config, whether the
base branch matches GitHub, commit emails that GitHub may reject as private, and whether `serve`
is running on the current config (from its heartbeat file, `serve.json` in the queue). `warning`
lines do not stop the request.

## Request

Full example: [`examples/request.example.json`](../examples/request.example.json).

| Field | Rule |
| --- | --- |
| `id` | New every time. Letters, digits, `-`, `_`; at most 80. |
| `expected_head` | Full SHA of the current HEAD. May be empty only with `create_branch: true`. |
| `branch` | Neutral name such as `fix/parser`. Not the base branch. No spaces or `..`. |
| `create_branch` | `true` to branch from an up-to-date base branch; `false` to add a commit to an existing branch. |
| `resume` | `true` to finish an already-pushed runner commit (with `files: []`). |
| `files` | Exact repository-relative paths to stage. No folders, wildcards, `..`, or protected paths. |
| `commit_message` | One line, semantic type first (`fix: ...`), at most 120 characters, no CI-skip markers. |
| `pr_title` | One line, semantic type first, at most 120 characters. |
| `summary` | At least one line. |
| `verification` | At least one `{ "check": "...", "result": "..." }` row. |
| `traceability` | At least one line. |

No field may contain co-author or "generated with" lines, AI product names (Claude, Codex,
ChatGPT, OpenAI, Anthropic, Copilot, Gemini), or the config's `blocked_words`.

## Receipt

Printed as one JSON line:

```json
{"id":"fix-parser-1","status":"needs_fix","detail":"test failure: parser::empty","pr_url":"https://github.com/owner/name/pull/12","diagnostic_log":"...","failure_kind":"test_failure","action":"fix_code","next":"C:/apr/agent-pr-runner.exe publish C:/apr/configs/app.json --retry fix-parser-1 --verify CHECK=RESULT"}
```

`action` is a stable code for what the agent does next, and `next` the exact command when there
is one. Commands use forward-slash paths, so they paste into PowerShell, cmd, and bash; a path
with spaces is quoted for PowerShell with `&`.

| `action` | From `status` | Next |
| --- | --- | --- |
| `done` | `merged` | Nothing. The working copy is back on the updated base branch. |
| `fix_code` | `needs_fix` | Fix what `detail`, `failure_kind`, and `diagnostic_log` show, rerun the gates, run `next` with real results. |
| `fix_request` | `error`, nothing committed | Fix what `detail` names, run `next`. |
| `resume` | `error`, the runner's commit exists | Run `next` unchanged. |
| `report_to_operator` | `merged_needs_refresh`, `needs_inspection`, a runner setup error, or CI blocked with local checks off | Stop and report `detail`. |
| `wait` | `queued` or `processing` (from `status`) | Run `next` again. |
| `unknown_id` | `unknown` (from `status`) | Check the id. |
| `unclassified` | an `error` receipt from an older runner | Read `detail`. |

`failure_kind` on `needs_fix` is `test_failure`, `lint`, `build`, `infra_transient`, or `unknown`.

`submit` exits with 0 only for `merged`. `status <queue> <id> --wait SECONDS` (at most 900)
checks until the receipt exists or the time is up, so an agent never holds one long call.
