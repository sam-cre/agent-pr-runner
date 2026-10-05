# Troubleshooting

Receipts carry a `detail` line. Find its wording below.

## Setup and doctor

| Message | Cause and fix |
| --- | --- |
| Install script lists `MISSING` lines and exits with 2 | Install what it lists, open a new terminal so PATH updates, and run it again. |
| `cargo build` fails with `link.exe not found` (Windows) | Install the Visual Studio C++ Build Tools (`winget install --id Microsoft.VisualStudio.2022.BuildTools -e`, then the "Desktop development with C++" workload), and run the install script again. |
| `gh is not logged in` (from `init`) | Run `gh auth login` yourself, then `init` again. |
| `... already exists; pass --force to replace it` | That repository already has a config. Use `--force` to replace it or `--name` for a second one. |
| `pass the repository root as --repo` | `--repo` pointed at a subfolder. Use the folder it names. |
| `origin is not a github.com repository URL` | The `origin` remote must be a github.com HTTPS or SSH URL. |
| `"status": "needs_input"` from `init` | Nothing was written. Each entry in `problems` says what to add (`--workflow`, `--job`, a CI workflow, or a `--local-checks` preset). |
| `installed runner needs an empty disabled-hooks directory beside its executable` | Create an empty `disabled-hooks` folder next to the binary. Re-running the install script does this. Keep it empty. |
| `queue directory must be outside the agent-writable repository` | Move the queue folder out of the repository and update `queue_dir`. |
| `runner binary and config must be installed outside the agent-writable repository` | Install with the script, which uses a folder in your home directory, and keep configs there. |
| `git and gh executables must be existing absolute paths` | Find them with `where git` and `where gh` (Windows) or `which git gh`, and use the full paths. |
| `gh login differs from pinned operator account` | Run `gh auth status`. Log in as `github_login` with `gh auth login`, or fix the config. |
| `Git remote differs from pinned repository` | `git remote get-url origin` must be exactly `https://github.com/owner/name(.git)` or the SSH form. |
| `local check ... needs ... on this account's PATH` | Install the program for the account that runs `serve`, or use an absolute path in `program`. |
| `another runner is already serving this queue` | One `serve` per queue. Stop the other one first. |

## Requests refused before anything happens

| Message | Fix |
| --- | --- |
| `invalid request id` | 1 to 80 letters, digits, `-`, `_`. |
| `request id already exists; choose a new id` | Every submission needs a new id, including retries. |
| `expected_head must be a full Git SHA` | Use the full 40-character output of `git rev-parse HEAD`. |
| `... is operator-only` | The file is protected. The operator applies it by hand. |
| `commit message and PR title must start with a semantic type` | Start with `feat: `, `fix: `, `docs: `, `chore: `, and so on (colon and space). |
| `... must not name an AI product or blocked word` | Remove the product name or configured word from the branch, commit, title, or PR text. |
| `PR summary, verification evidence, and traceability are required` | All three lists need at least one entry. |

## Errors during the run

| Message | Cause and fix |
| --- | --- |
| `branch creation requires the checkout to be on the base branch` | Switch the working copy to the base branch first (the operator, or a read-only check of where it is). After a `merged` receipt the runner leaves it there. |
| `local base branch is not up to date with the remote` | The operator pulls the base branch, then resubmit with the new HEAD. |
| `HEAD changed before branch creation` / `branch or HEAD changed before staging` | `expected_head` is stale. Read HEAD again and resubmit. |
| `index already contains staged changes` | Something was staged by hand. Unstage it (the operator), then resubmit. |
| `staged paths do not match request` | A listed path is ignored by `.gitignore`, misspelled, or unchanged. The index is left for inspection; the operator unstages. |
| `checking staged diff failed: ... trailing whitespace` | Fix the whitespace error the detail names and resubmit. |
| `CI did not complete within N minutes` | CI is slow or stuck. Resubmit with `resume: true` once it finishes, or raise `ci.timeout_minutes`. If GitHub shows every job green, `ci.required_jobs` does not match the names GitHub shows: run `init --force` with `--job` per name, or fix the config. |
| `base branch moved while CI ran` | Another PR merged first. Update the branch (the operator merges or rebases the base in), then resume. |
| `PR changed, became draft, or targets another base` | Someone edited the PR on GitHub. Undo the edit or resubmit so the runner rewrites title and body. |
| `squash-merging failed: ... Invalid email address` | Old runner versions passed a no-reply address to the merge. Current versions leave it out. Update the binary. |

## The local fallback

| Symptom | Fix |
| --- | --- |
| `GitHub could not start CI (billing) and local checks are off` | Either restore Actions minutes or billing, or set `local_checks.enabled` to true with steps. |
| `no local check applied to this commit` | Every step had `only_if_exists` paths that are missing. Add a step that always runs. |
| `local checks took longer than N minutes` | Raise `budget_minutes`, or reduce the steps. |
| A check fails only on the runner, from stale build output | Delete `<install>/work/<owner>_<name>/build` while no request is running; it is rebuilt. |
| npm fails with `ENOENT` on Windows | Use `npm` as `program` (the runner finds `npm.cmd`), not `npm.ps1`. |
| Paths too long on Windows | Install the runner in a short folder such as `C:\apr`, or enable long paths in Git (`git config --system core.longpaths true`, operator). |

## Keeping `serve` running

`serve` runs until it is stopped. It processes one request at a time and polls the queue every two
seconds. If it stops mid-request, the next start writes `needs_inspection` for that request.
Check the branch, the commit, and the PR on GitHub before resubmitting with a new id.
