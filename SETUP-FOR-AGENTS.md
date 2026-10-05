# Setup guide for AI agents

You were asked to set up agent-pr-runner for a repository. Follow these steps in order. Do the
work yourself. Stop for the user only at steps marked **Ask** or **User step**, and keep your
messages to the user short: what is needed, the exact command, and why.

## Ground rules for setup

- Never run `gh auth login`, and never type a password, token, or code for the user. Logging in is
  a user step.
- Never change GitHub settings (email privacy, branch protection) yourself. Tell the user where
  they are.
- Before installing any software, show the user the exact command and ask whether you should run
  it or they will. Run it only after a clear yes.
- Do not commit or push in the target repository yourself during setup. The one file the user may
  need to commit by hand is the CI workflow (step 5).
- Install nothing inside the target repository. The runner refuses to start from inside one.

## 1. Find the target

- The target repository is usually your current working folder. **Ask** the user to confirm the
  path if there is any doubt.
- Note the operating system. Windows commands use PowerShell; macOS and Linux use `sh`.

## 2. Get this repository

If this repository is not already on disk, clone it to a folder **outside** the target repository:

```sh
git clone https://github.com/sam-cre/agent-pr-runner "$HOME/agent-pr-runner-src"
```

Cloning only reads from GitHub. If `git` itself is missing, go to step 3 first and come back.

## 3. Check the dependencies

From the cloned folder:

- Windows: `powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -CheckOnly`
- macOS or Linux: `sh scripts/install.sh --check-only`

It prints one line per requirement: Rust (`cargo`), a C linker (macOS and Linux), Git, the GitHub
CLI (`gh`), and whether `gh` is logged in.

- Exit code 0: everything is there. Go to step 4.
- Exit code 2: some lines say `MISSING` with an install command. Give the user a short list of
  just those lines. **Ask** whether you should run the commands or they will.
- `gh login` missing is a **User step**: the user runs `gh auth login`, picks GitHub.com, HTTPS,
  and "Login with a web browser", and uses the account that should own the pull requests.
- After installs, run the check again. On Windows the script picks up new PATH entries itself; on
  macOS and Linux, if a tool still shows as missing, open a new shell and retry. Repeat until it
  exits with 0.

## 4. Build and install

Run the same script without the check flag:

- Windows: `powershell -ExecutionPolicy Bypass -File scripts\install.ps1`
- macOS or Linux: `sh scripts/install.sh`

It builds the runner and installs it in `~/agent-pr-runner`. Note the `Installed:` path; this
guide calls it `<runner>`. On Windows a build error about `link.exe` means the Visual Studio C++
Build Tools are missing: **Ask** the user to install them (`winget install --id
Microsoft.VisualStudio.2022.BuildTools -e`, then add the "Desktop development with C++" workload
in the Visual Studio Installer), then run the script again.

## 5. Make sure the repository has CI

The runner merges only on green CI. Look in the target repository's `.github/workflows/` for a
workflow that runs on `pull_request`.

If there is none:

1. Copy the closest example from [`examples/workflows/`](examples/workflows/) (Rust, Node, or
   Python) to `.github/workflows/ci.yml` in the target repository, and adjust its commands to the
   project's real build and test commands.
2. **User step:** the runner never touches workflow files, so the user commits this one. Give them:

   ```sh
   git add .github/workflows/ci.yml
   git commit -m "ci: add CI workflow"
   git push
   ```

## 6. Two choices

**Ask** the user both, with these explanations and recommendations:

- **Local checks when GitHub Actions minutes run out?** If GitHub refuses to start CI for billing
  reasons, the runner can run the project's own build and tests on this machine instead, then
  merge. Recommended: yes if the repository is private on a free plan, otherwise optional.
  Yes means `--local-checks auto` (works for Rust, Node, and Python projects).
- **Protect the project's manifest files?** The agent then can not change files like
  `Cargo.toml`, `package.json`, or `pyproject.toml`, so it can not add dependencies or change what
  "tests pass" means on its own; the user applies those changes by hand. Recommended: yes for
  shared or important repositories. Yes means `--protect-manifests`.

## 7. Write the config

```sh
<runner> init --repo <target repository path> [--local-checks auto] [--protect-manifests]
```

`init` reads everything it can: the GitHub repository from the `origin` remote, the logged-in
account and its no-reply email from `gh`, the default branch, the CI workflow and its job names,
and the project kind. It writes the config and queue folder inside the install folder, then runs
`doctor`. It prints JSON.

- `"status": "ready"`: show the user the `detected` part in a few lines (repository, account,
  commit email, base branch, CI jobs, local checks) and **Ask** them to confirm it is right.
- `"status": "needs_input"`: nothing was written. Handle each entry in `problems`:

| Problem says | What you do |
| --- | --- |
| no workflow runs on pull_request | Step 5, then run `init` again. |
| several workflows run on pull_request | **Ask** which one is the main CI. Add `--workflow .github/workflows/<file>`. |
| could not tell the CI job names | **Ask** the user to open the repository's latest PR or Actions run and read the job names GitHub shows, or open one PR by hand. Add `--job "<name>"` once per job. |
| --local-checks auto found no ... | **Ask** which preset fits (`rust`, `node`, `python`) or use `off`. |

- An error that `gh` is not logged in: back to step 3.
- An error that the config already exists: the repository is set up already. Use `--force` only
  if the user wants to replace it.

Full field reference: [docs/configuration.md](docs/configuration.md). If the user wants something
`init` does not set (blocked words, extra protected paths), edit the config file it printed, then
run `<runner> doctor <config>`.

## 8. Add the publishing instructions to the target repository

```sh
<runner> snippet <config>
```

This prints the publishing rules with the real runner and queue paths. Append the output to the
target repository's agent instructions file (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, or whatever
you read). If there is none, create `AGENTS.md`.

If you support skills, also copy [`agent/skill/publish-with-runner/`](agent/skill/publish-with-runner/)
into your skills folder.

If you have a commit attribution setting (co-author lines and similar), tell the user how to turn
it off. The runner refuses attribution either way; turning it off avoids refused requests.

## 9. Start the runner

**Ask** the user how they want `serve` to run. Use the `serve` command `init` printed.

- **In a terminal for now:** the user opens a terminal, runs the `serve` command, and leaves it
  open. Do not start it as a background process of your own session; it would stop when your
  session ends.
- **Automatically at login, Windows** (you may run this after a yes):

  ```powershell
  schtasks /Create /TN "agent-pr-runner <name>" /SC ONLOGON /RL LIMITED /TR "`"<runner>`" serve `"<config>`""
  schtasks /Run /TN "agent-pr-runner <name>"
  ```

- **Automatically at login, Linux with systemd:** write
  `~/.config/systemd/user/agent-pr-runner-<name>.service`:

  ```ini
  [Unit]
  Description=agent-pr-runner for <name>

  [Service]
  ExecStart=<runner> serve <config>
  Restart=on-failure

  [Install]
  WantedBy=default.target
  ```

  then run `systemctl --user enable --now agent-pr-runner-<name>`.
- **macOS:** use the terminal option.

Only one `serve` may run per config.

## 10. GitHub settings (User step)

Tell the user to do these two things on github.com. You do not do them.

- Settings, Emails: turn on **Keep my email addresses private**. The runner commits with the
  no-reply address; this setting keeps the squash-merge commits on it too.
- The repository's Settings, Branches (or Rules): add a rule for the base branch that requires a
  pull request, requires the status checks `init` found, and blocks force pushes. Recommended, not
  required.

## 11. First publish

Publish the instructions file change from step 8 through the runner, following the instructions
you just added. This tests everything at once.

- `merged`: setup is done. Give the user the PR link.
- An `error` that CI did not complete in time while GitHub shows every job green: the job names in
  the config do not match. Run `init` again with `--force` and `--job` for each name GitHub shows.
- Anything else: [docs/troubleshooting.md](docs/troubleshooting.md).

## 12. Tell the user

Finish with a short summary:

- where the runner, config, and queue are;
- how `serve` runs, and how to stop it;
- that updating means pulling this repository and running the install script again;
- any user steps still open (GitHub settings, protected-file changes).
