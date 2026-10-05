# Configuration reference

One JSON file per repository. Keep it outside the repository (the runner refuses to start
otherwise). Unknown fields are rejected, so a typo fails loudly instead of being ignored.

`agent-pr-runner init --repo PATH` writes this file for you. To write one by hand, start from
[`config.example.json`](../config.example.json), then run `agent-pr-runner doctor CONFIG.json`.

## init options

`init` reads the GitHub repository from the `origin` remote, the login and no-reply email from
`gh`, the default branch, and the CI workflow and its job names. It writes
`configs/<name>.json` and `queues/<name>/` in the install folder, then runs `doctor`. When it can
not work something out, it writes nothing and prints `"status": "needs_input"` with what to add.

| Option | Effect |
| --- | --- |
| `--local-checks auto` | Turn on the billing fallback with the preset for the project (Rust, Node, or Python). Or name one: `rust`, `node`, `python`. Default `off`. |
| `--protect-manifests` | Make the project's manifest files operator-only (for example `Cargo.toml` or `package.json`). |
| `--workflow FILE` | Pick the CI workflow when several run on `pull_request`. |
| `--job NAME` | Set a required job name; repeat per job. Needed only when `init` can not tell (matrix `include`, reusable workflows, names built from other expressions). |
| `--name NAME` | Config and queue name. Default: the repository folder name. |
| `--force` | Replace an existing config of the same name. |

## Top-level fields

| Field | Required | Default | Meaning |
| --- | --- | --- | --- |
| `repo` | yes | | Absolute path of the agent's working copy. The runner stages and commits here. |
| `queue_dir` | yes | | Absolute path of this repository's queue folder. Must exist and be outside `repo`. |
| `git_exe` | yes | | Absolute path to `git` (`git.exe`). Must be outside `repo`. |
| `gh_exe` | yes | | Absolute path to the GitHub CLI (`gh.exe`). Must be outside `repo`. |
| `repository` | yes | | `owner/name` on github.com. The `remote` URL must point exactly here. |
| `author_name` | yes | | Name on every commit the runner makes (author and committer). |
| `author_email` | yes | | Email on every commit. Use your GitHub no-reply address (see below). |
| `github_login` | yes | | The account `gh` is logged in as. The runner only edits and merges PRs this account opened. |
| `base_branch` | no | `main` | The branch PRs target and the runner returns to after a merge. |
| `remote` | no | `origin` | The Git remote to push to. |
| `ci` | yes | | See [CI](#ci). |
| `local_checks` | no | off | See [Local checks](#local-checks). |
| `protected_paths` | no | `[]` | Extra paths the agent may never stage. See [Protected paths](#protected-paths). |
| `blocked_words` | no | `[]` | Extra words refused in branch names, commit messages, and PR text (case-insensitive). |
| `cache_limit_gb` | no | `5` | Between requests, the local-check build cache is cleared once it is larger than this. |

### Finding your no-reply email

GitHub, Settings, Emails. The address looks like `12345678+your-account@users.noreply.github.com`.
Also turn on **Keep my email addresses private** there. The runner leaves the author flag off the
squash merge for a no-reply address (GitHub rejects it there), so the squashed commit on the base
branch uses the account's default commit email, which is the no-reply address only while that
setting is on.

## CI

| Field | Default | Meaning |
| --- | --- | --- |
| `workflow_name` | `CI` | The workflow's `name:` line. |
| `workflow_file` | `.github/workflows/ci.yml` | The workflow file. A run from any other file never counts. |
| `required_jobs` | (required) | Every job name that must pass, exactly as GitHub shows it on the PR. A matrix job `name: lint + test (${{ matrix.os }})` shows as `lint + test (ubuntu-latest)` and so on. |
| `timeout_minutes` | `30` | How long to wait for CI before giving up with an `error` receipt. |

The runner merges only when the newest run of that workflow for the exact pushed commit is from a
`pull_request` event for this PR, every job in it succeeded, and every required job is there.

An infrastructure-looking failure (network errors, runner shutdowns, setup steps) is rerun once
automatically. Anything else returns `needs_fix`.

## Local checks

When GitHub refuses to start CI for billing reasons (free Actions minutes used up, or a payment
problem), every job ends with zero steps, at least one with a billing note and any other with that note or GitHub's "was not acquired by runner" note (a job that waited about 15 minutes for a runner that never came). A run where no job mentions billing is treated as an outage, not billing. Only then, and only if
`local_checks.enabled` is true, the runner:

1. makes a clean, detached checkout of the exact pushed commit in its own work folder;
2. runs each step in order, stopping at the first failure;
3. confirms the checkout was not modified while the checks ran;
4. comments on the PR listing what it ran and that other CI platforms were not checked;
5. checks again that CI is still billing-blocked, then squash-merges.

A run where any job actually started is never treated this way, so a real red build can not slip
through. With local checks off, a billing-blocked PR stops with `needs_fix`.

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Turn the fallback on. |
| `budget_minutes` | `60` | Total time for all steps together. |
| `steps` | `[]` | The checks, in order. |

Each step:

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | (required) | Shown in logs, receipts, and the PR comment. |
| `program` | (required) | A program on PATH (`cargo`, `npm`, `python`) or a path. On Windows, `npm` finds `npm.cmd` through PATHEXT. May use `{cache}`. |
| `args` | `[]` | Arguments. `{cache}` is replaced. |
| `dir` | `.` | Folder inside the checkout to run in. |
| `env` | `{}` | Extra environment variables. `{cache}` is replaced. |
| `only_if_exists` | `[]` | Run the step only when all of these paths exist in the checkout. |

`{cache}` is the runner's persistent build folder for this repository,
`<install dir>/work/<owner>_<name>/build`. Point build output there (for example
`CARGO_TARGET_DIR`) so repeated checks are fast and the agent's own build folders are untouched.

Every step runs without the runner's Git and GitHub environment variables or tokens.

Ready-made step lists: [`examples/checks/`](../examples/checks/) for Rust, Node, Python, and a
Rust workspace plus a Tauri app. Paste one as the `local_checks` value.

Keep the steps the same as your CI and your agent's verification gates, so all three agree on what
"passing" means.

## Protected paths

Always operator-only, whatever the config says:

- anything whose first path segment starts with `.git` (`.git/`, `.github/`, `.gitignore`,
  `.gitattributes`); the `.github/workflows/` folder in particular
- `CODEOWNERS` anywhere

Add more with `protected_paths`. An entry ending in `/` protects a folder and everything in it;
any other entry protects that one exact path from the repository root. Case does not matter.

Good candidates are files that change what "passing" means or how the build runs:

- Rust: `Cargo.toml` (root), `build.rs`, `.cargo/`, `rust-toolchain.toml`
- Node: `package.json` (its `scripts` define `npm test`), `.npmrc`
- Python: `pyproject.toml`, `setup.cfg`, `tox.ini`, `noxfile.py`

Protecting a manifest means the agent can not add dependencies on its own. That is a trade-off:
safer, but the operator applies those changes by hand.

## One binary, several repositories

Install the binary once. For each repository, add a config and a queue folder, and run one
`serve` process per config. Each repository gets its own work folder, and each queue allows only
one `serve` process at a time.
