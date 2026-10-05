# Requests and receipts

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
{"id":"fix-parser-1","status":"merged","detail":"CI passed; PR squash-merged and base refreshed","pr_url":"https://github.com/owner/name/pull/12","diagnostic_log":null}
```

| Status | Meaning |
| --- | --- |
| `merged` | Done. The working copy is back on the updated base branch. |
| `needs_fix` | CI or local checks failed. `failure_kind` is `test_failure`, `lint`, `build`, `infra_transient`, or `unknown`; `diagnostic_log` is the saved log. |
| `error` | Refused or failed; `detail` says why. If the commit was already pushed, resubmit with `resume: true`. |
| `merged_needs_refresh` | Merged, but returning to the base branch failed. |
| `needs_inspection` | The runner stopped mid-request. Check GitHub before doing anything else. |

`submit` exits with 0 only for `merged`. If the agent's shell times out first, the request keeps
running; `agent-pr-runner status <queue> <id>` prints the receipt when it exists, or `queued`,
`processing`, or `unknown`.
