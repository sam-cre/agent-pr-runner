# Security model

## What it protects against

The runner exists so an AI agent can ship changes on its own without being able to:

- push to the base branch directly, or merge without CI;
- skip CI (`[skip ci]` and similar markers are refused);
- change CI workflows, `CODEOWNERS`, Git metadata, or other operator-only paths;
- stage files it did not name (the staged set must equal the request's list exactly);
- publish attribution or product names (co-author and generator trailers, AI product names, and
  configured words are refused in every published field);
- commit under another identity (author and committer are pinned and verified before the push);
- merge a PR someone else opened, edited, retargeted, or pushed to after CI ran;
- run repository hooks with the operator's credentials (Git's hooks folder is pointed at an empty
  folder beside the binary);
- leak a token into a receipt (tokens and `token=` style assignments are redacted).

It also makes every PR look the same: a semantic title, a Summary, a Verification Evidence table,
and Traceability.

## How it does it

- **Data, not commands.** The agent writes JSON. Nothing in a request is passed to a shell. Paths
  are checked for `..`, absolute paths, wildcards, backslashes, control characters, symlinks, and
  exact case before Git sees them, and every Git call puts `--` before paths.
- **Separate install.** The binary, its config, the queue, and the `git` and `gh` executables must
  all be outside the repository. The runner refuses to start otherwise.
- **Pinned GitHub identity.** Before acting, the runner confirms `gh` is logged in as
  `github_login` and the remote is exactly `repository` on github.com.
- **Commit-bound CI.** CI must be from the configured workflow file, for the exact pushed commit,
  for this PR, with every required job green. `--match-head-commit` on the merge closes the race
  with a later push. The runner never uses `--admin` and never bypasses branch protection.
- **No replay after a crash.** A request that was in progress when `serve` stopped gets a
  `needs_inspection` receipt instead of running again.
- **Contained processes.** Every command runs in a Windows Job Object or Unix process group with a
  timeout, so a hung check can not leave processes behind.
- **Clean local checks.** The billing fallback runs on a fresh checkout of the exact commit, with
  no Git or GitHub tokens in its environment, and verifies the checkout was not modified.

## What it does not protect against

Be honest with yourself about these:

- **Same-account agents.** If the agent runs as the same operating-system user as `serve` and has
  unrestricted shell access, it can in principle edit the runner's config or binary, or call `gh`
  itself. The runner is a guardrail against mistakes and drift, not a wall against a hostile
  agent. For a hard boundary, either run `serve` under a separate OS account that owns the `gh`
  login, or confine the agent's sandbox to the repository (most agent tools support this).
- **The commit's own code.** Local checks run the commit's tests and build scripts. A change to a
  test can make the test pass. Code review is still the defense against bad changes; protect the
  files that define your checks with `protected_paths` if that matters to you.
- **GitHub settings.** Set branch protection on the base branch yourself: require a pull request
  and the required status checks, and block force pushes. The runner respects those rules; it
  does not create them.

## Reporting a problem

Open an issue without secrets or private repository details. For a sensitive report, contact the
repository owner privately through GitHub.
