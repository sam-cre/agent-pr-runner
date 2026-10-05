# agent-pr-runner

An operator-owned Git and pull-request runner for AI coding agents.

Your agent never runs `git commit`, `git push`, or `gh pr merge`. It writes a small JSON request basically saying which files to publish and how to describe them. The runner, a separate program you install outside the repository, does the rest:

1. stages exactly those files and commits them under your pinned identity, with no co-author or "generated with" trailers;
2. pushes a branch and opens a PR with a fixed layout: Summary, Verification Evidence table, Traceability;
3. waits for your CI, and reruns it once if the failure looks like a network or runner hiccup;
4. squash-merges only when every required job passed, deletes the branch, and pulls the base branch;
5. when GitHub refuses to start CI because of billing (out of free Actions minutes), runs your checks itself on a clean checkout of the exact commit, notes that on the PR, then merges.

The agent gets back a one-line JSON receipt (`merged`, `needs_fix`, `error`, ...) that tells it what to do next.

* Works with any agent that can run a shell command and write a file, and with any language.
* Stops the usual problems: merges without CI or with `[skip ci]`, edited CI workflows, stray files from `git add .`, AI attribution in your history, and PRs that claim "tests pass" with no evidence.
* Written in Rust. Targets Windows, macOS, and Linux (developed and used on Windows).

## Setup: let your agent do it

Open your agent in the repository you want to use this with, and tell it:

> Set up agent-pr-runner (https://github.com/sam-cre/agent-pr-runner) for this repository. Follow its SETUP-FOR-AGENTS.md.

The agent checks for Rust, Git, and the GitHub CLI and tells you how to install anything missing, then builds the runner, writes the config, adds the publishing rules to your agent instructions, starts the runner, and publishes a first PR as a test. It asks you two questions along the way.

What stays with you: installing software when asked, logging in with `gh auth login`, committing a CI workflow if the repository has none, and two GitHub settings (email privacy, branch protection). The agent tells you when and how.

## Setup by hand

You need Rust, Git, the GitHub CLI (`gh`), and a GitHub repository with a CI workflow that runs on `pull_request` (examples in [`examples/workflows/`](examples/workflows/); commit it yourself, the runner never touches workflow files).

1. Clone this repository anywhere and run the installer. It checks the requirements first and prints the install command for anything missing.

   ```sh
   sh scripts/install.sh                                         # macOS or Linux
   powershell -ExecutionPolicy Bypass -File scripts\install.ps1  # Windows
   ```

   It installs to `~/agent-pr-runner` (pass another folder if you like; never inside a repository).

2. Log in as the account that should own the PRs: `gh auth login`. In GitHub, open Settings, Emails, and turn on **Keep my email addresses private** (the runner commits with your no-reply address).

3. Write the config. `init` reads your repository, account, no-reply email, default branch, and CI job names, then checks everything:

   ```sh
   ~/agent-pr-runner/agent-pr-runner init --repo /path/to/your/repository
   ```

   Options (billing fallback checks, protected manifest files, and more) are in [docs/configuration.md](docs/configuration.md#init-options).

4. Start the runner and leave it running (one per repository):

   ```sh
   ~/agent-pr-runner/agent-pr-runner serve ~/agent-pr-runner/configs/<project>.json
   ```

   To start it at login, see step 9 of [SETUP-FOR-AGENTS.md](SETUP-FOR-AGENTS.md#9-start-the-runner).

5. Print the publishing rules with your paths filled in, and paste them into your agent instructions file (`AGENTS.md`, `CLAUDE.md`, or similar):

   ```sh
   ~/agent-pr-runner/agent-pr-runner snippet ~/agent-pr-runner/configs/<project>.json
   ```

   If your agent supports skills, also copy [`agent/skill/publish-with-runner/`](agent/skill/publish-with-runner/). Turn off your agent's own commit attribution setting if it has one.

6. Recommended: in GitHub, protect the base branch (require a pull request and your CI jobs, block force pushes).

On Windows, the binary is `agent-pr-runner.exe` in `%USERPROFILE%\agent-pr-runner`.

## Day to day

The agent makes a change, runs your checks, writes a request, and runs `agent-pr-runner submit <queue> <request.json>`. It reports the PR link, or fixes and resubmits based on the receipt. You keep `serve` running, review merged PRs, and apply changes to protected files when the agent asks.

Request and receipt fields: [docs/requests.md](docs/requests.md).

**Update:** pull this repository, run the install script again, and restart `serve`. Configs and queues are kept.
**Remove:** stop `serve` (and any scheduled task or service), then delete the install folder.

## Limits

* GitHub only, squash merges only, one request at a time per repository.
* The local fallback checks only the machine it runs on.
* It is a guardrail, not a sandbox. An agent with unrestricted shell access as your user could work around it; see [docs/security-model.md](docs/security-model.md).

## More

[Configuration](docs/configuration.md), [requests and receipts](docs/requests.md), [security model](docs/security-model.md), [troubleshooting](docs/troubleshooting.md), [agent setup guide](SETUP-FOR-AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
