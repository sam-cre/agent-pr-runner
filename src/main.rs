//! Operator-owned Git and pull-request runner for AI coding agents.
//!
//! The agent never runs Git write commands. It writes a small JSON request (files to stage, commit
//! message, PR text, verification evidence) and queues it. This separate process, installed and
//! configured by the operator outside the agent's workspace, owns the GitHub login and does the
//! rest: stage exactly those files, commit with a pinned identity and no attribution trailers,
//! push, open the PR with a fixed body layout, wait for CI, and squash-merge only when CI passes.
//! When GitHub refuses to start CI for billing reasons, it runs the configured checks itself on a
//! clean checkout of the exact commit instead.

mod process;
mod setup;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CAPTURE: usize = 2 * 1024 * 1024;
/// How long `submit` waits for a receipt before telling the agent to poll with `status`.
const SUBMIT_WAIT: Duration = Duration::from_secs(4 * 3600);

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The agent's working copy. The runner commits from here.
    repo: PathBuf,
    /// Where requests and receipts live. Must be outside `repo`.
    queue_dir: PathBuf,
    git_exe: PathBuf,
    gh_exe: PathBuf,
    /// `owner/name` on github.com.
    repository: String,
    author_name: String,
    author_email: String,
    /// The account `gh` is logged in as. PRs by anyone else are never edited or merged.
    github_login: String,
    #[serde(default = "default_base_branch")]
    base_branch: String,
    #[serde(default = "default_remote")]
    remote: String,
    ci: CiConfig,
    #[serde(default)]
    local_checks: LocalChecksConfig,
    /// Paths the agent may never stage, on top of the built-in ones. An entry ending in `/` is a
    /// folder prefix; anything else is one exact path. Case does not matter.
    #[serde(default)]
    protected_paths: Vec<String>,
    /// Words that may not appear in branch names, commit messages, or PR text, on top of the
    /// built-in AI product names. Case does not matter.
    #[serde(default)]
    blocked_words: Vec<String>,
    /// The runner's build cache is cleared between requests once it is larger than this.
    #[serde(default = "default_cache_limit_gb")]
    cache_limit_gb: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CiConfig {
    /// The workflow's `name:`.
    #[serde(default = "default_workflow_name")]
    workflow_name: String,
    /// The workflow file. A run from any other file never counts.
    #[serde(default = "default_workflow_file")]
    workflow_file: String,
    /// Job names (as GitHub shows them on the PR) that must all pass before a merge.
    required_jobs: Vec<String>,
    #[serde(default = "default_ci_timeout_minutes")]
    timeout_minutes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LocalChecksConfig {
    /// Run `steps` when GitHub refuses to start CI for billing reasons. Off means such a PR stops
    /// with `needs_fix` instead of merging.
    enabled: bool,
    budget_minutes: u64,
    steps: Vec<CheckStep>,
}

impl Default for LocalChecksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            budget_minutes: 60,
            steps: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckStep {
    name: String,
    /// A program on PATH (`cargo`, `npm`, `python`) or a path, which may use `{cache}` (for a
    /// virtual environment an earlier step made). On Windows, `npm` finds `npm.cmd` via PATHEXT.
    program: String,
    #[serde(default)]
    args: Vec<String>,
    /// Folder inside the checkout to run in.
    #[serde(default = "default_step_dir")]
    dir: String,
    /// Extra environment. `{cache}` in a value or argument becomes the runner's build cache folder.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Run the step only when all of these paths exist in the checkout.
    #[serde(default)]
    only_if_exists: Vec<String>,
}

fn default_base_branch() -> String {
    "main".into()
}
fn default_remote() -> String {
    "origin".into()
}
fn default_workflow_name() -> String {
    "CI".into()
}
fn default_workflow_file() -> String {
    ".github/workflows/ci.yml".into()
}
fn default_ci_timeout_minutes() -> u64 {
    30
}
fn default_cache_limit_gb() -> u64 {
    5
}
fn default_step_dir() -> String {
    ".".into()
}

impl Config {
    fn policy(&self) -> Policy {
        Policy {
            protected: self
                .protected_paths
                .iter()
                .map(|p| p.to_ascii_lowercase())
                .collect(),
            blocked: self
                .blocked_words
                .iter()
                .map(|w| w.to_ascii_lowercase())
                .collect(),
        }
    }

    fn ci_timeout(&self) -> Duration {
        Duration::from_secs(self.ci.timeout_minutes * 60)
    }

    fn checks_budget(&self) -> Duration {
        Duration::from_secs(self.local_checks.budget_minutes * 60)
    }
}

/// The publishing rules that depend on the repository: extra protected paths and blocked words.
/// `submit` checks the built-in rules only; `serve` checks these too before touching Git.
#[derive(Debug, Default)]
struct Policy {
    protected: Vec<String>,
    blocked: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Requests and receipts
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    branch: String,
    #[serde(default)]
    create_branch: bool,
    #[serde(default)]
    expected_head: String,
    /// Finish a request that stopped after its commit (for example an `error` receipt after the
    /// push): no staging or new commit, just push if needed, then PR, CI, and merge. `files` must be
    /// empty, and `commit_message` must equal the runner-made commit at `expected_head`.
    #[serde(default)]
    resume: bool,
    #[serde(default)]
    files: Vec<String>,
    commit_message: String,
    pr_title: String,
    summary: Vec<String>,
    /// Rendered as the PR's Verification Evidence table: one row per check and its observed result.
    verification: Vec<Evidence>,
    /// What this change traces to (plan item, checklist line, issue).
    traceability: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    check: String,
    result: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    id: String,
    status: String,
    detail: String,
    pr_url: Option<String>,
    diagnostic_log: Option<String>,
    /// Set on `needs_fix`: infra_transient, test_failure, lint, build, or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_kind: Option<String>,
}

struct Capture {
    stdout: String,
    stderr: String,
    success: bool,
}

/// Conventional-commit types accepted for commit subjects and PR titles.
const SEMANTIC_TYPES: &[&str] = &[
    "feat", "fix", "chore", "docs", "refactor", "test", "ci", "build", "perf", "style",
];

fn is_semantic(text: &str) -> bool {
    let Some((head, rest)) = text.split_once(": ") else {
        return false;
    };
    let head = head.strip_suffix('!').unwrap_or(head);
    let kind = head.split_once('(').map_or(
        head,
        |(kind, scope)| if scope.ends_with(')') { kind } else { "" },
    );
    SEMANTIC_TYPES.contains(&kind) && !rest.trim().is_empty()
}

/// AI products and vendors that published text (branch, commit, PR) must never name.
const AI_NAMES: &[&str] = &[
    "claude",
    "codex",
    "chatgpt",
    "openai",
    "anthropic",
    "copilot",
    "gemini",
];

fn names_blocked_word(text: &str, policy: &Policy) -> bool {
    let lower = text.to_ascii_lowercase();
    AI_NAMES.iter().any(|name| lower.contains(name))
        || policy
            .blocked
            .iter()
            .any(|word| !word.is_empty() && lower.contains(word.as_str()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Co-author and generator trailers are refused anywhere in published text.
fn has_attribution(text: &str) -> bool {
    text.lines().any(|line| {
        let lower = line.trim().to_ascii_lowercase();
        lower.starts_with("co-authored-by:")
            || lower.starts_with("signed-off-by:")
            || lower.starts_with("generated-by:")
            || (lower.contains("generated with") && AI_NAMES.iter().any(|n| lower.contains(n)))
    })
}

/// Git metadata, CI workflows, and CODEOWNERS are always operator-only; the config adds more.
fn protected_path(file: &str, policy: &Policy) -> bool {
    let lower = file.to_ascii_lowercase();
    let first = lower.split('/').next().unwrap_or("");
    first.starts_with(".git")
        || lower.starts_with(".github/workflows/")
        || lower == "codeowners"
        || lower.ends_with("/codeowners")
        || policy.protected.iter().any(|entry| {
            if entry.ends_with('/') {
                lower.starts_with(entry.as_str())
            } else {
                lower == *entry
            }
        })
}

fn validate_request(request: &Request, policy: &Policy) -> Result<()> {
    if !valid_id(&request.id) {
        bail!("invalid request id: use 1 to 80 letters, digits, '-' or '_'");
    }
    // `git check-ref-format` runs later. A leading '-' would be read as a Git option, so it is
    // rejected here before any command sees it.
    if request.branch.is_empty()
        || request.branch.starts_with('-')
        || request
            .branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || request.branch.contains("..")
    {
        bail!("branch name is unsafe");
    }
    if (!request.expected_head.is_empty() || !request.create_branch)
        && (request.expected_head.len() != 40
            || !request.expected_head.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("expected_head must be a full Git SHA");
    }
    if request.resume {
        if request.create_branch || request.expected_head.is_empty() {
            bail!("resume needs create_branch false and the branch's expected_head");
        }
        if !request.files.is_empty() {
            bail!("resume stages nothing; files must be empty");
        }
    } else if request.files.is_empty() {
        bail!("request has no files to stage");
    }
    let mut unique = BTreeSet::new();
    for file in &request.files {
        let path = Path::new(file);
        if path.is_absolute()
            || file.chars().any(char::is_control)
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || file.contains('\\')
            || file.contains(['*', '?', '[', ']'])
            || file.starts_with(':')
            || !unique.insert(file)
        {
            bail!("unsafe or duplicate staged path: {file}");
        }
        if protected_path(file, policy) {
            bail!("{file} is operator-only; the operator must change it by hand");
        }
    }
    let message_lower = request.commit_message.to_ascii_lowercase();
    if request.commit_message.chars().any(char::is_control)
        || request.commit_message.len() > 120
        || !request.commit_message.contains(": ")
        || has_attribution(&request.commit_message)
        || [
            "[skip ci]",
            "[ci skip]",
            "[no ci]",
            "[skip actions]",
            "[actions skip]",
        ]
        .iter()
        .any(|token| message_lower.contains(token))
    {
        bail!("commit message must be one conventional-commit line without attribution trailers");
    }
    if request.pr_title.trim().is_empty()
        || request.pr_title.contains(['\n', '\r'])
        || request.pr_title.len() > 120
        || has_attribution(&request.pr_title)
    {
        bail!("invalid PR title");
    }
    if !is_semantic(&request.commit_message) || !is_semantic(&request.pr_title) {
        bail!("commit message and PR title must start with a semantic type such as feat: or fix:");
    }
    if request.summary.is_empty()
        || request.verification.is_empty()
        || request.traceability.is_empty()
    {
        bail!("PR summary, verification evidence, and traceability are required");
    }
    let evidence_cells = request
        .verification
        .iter()
        .flat_map(|row| [&row.check, &row.result]);
    for line in request
        .summary
        .iter()
        .chain(evidence_cells)
        .chain(&request.traceability)
    {
        if line.trim().is_empty()
            || line.len() > 500
            || line.contains(['\n', '\r'])
            || has_attribution(line)
            || names_blocked_word(line, policy)
        {
            bail!("PR body contains an empty, oversized, multiline, attribution, or blocked line");
        }
    }
    if [&request.branch, &request.commit_message, &request.pr_title]
        .iter()
        .any(|text| names_blocked_word(text, policy))
    {
        bail!("branch, commit message, and PR title must not name an AI product or blocked word");
    }
    Ok(())
}

/// A folder or file inside the checkout: relative, with no `..`, no root, and no drive.
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn validate_config(config: &Config) -> Result<()> {
    let repo = fs::canonicalize(&config.repo).context("repository does not exist")?;
    let queue = fs::canonicalize(&config.queue_dir).context("queue directory does not exist")?;
    if queue.starts_with(&repo) {
        bail!("queue directory must be outside the agent-writable repository");
    }
    for exe in [&config.git_exe, &config.gh_exe] {
        if !exe.is_absolute() || !exe.is_file() {
            bail!("git and gh executables must be existing absolute paths");
        }
        if fs::canonicalize(exe)?.starts_with(&repo) {
            bail!("git and gh executables must be outside the agent-writable repository");
        }
    }
    for value in [
        &config.author_name,
        &config.author_email,
        &config.github_login,
        &config.repository,
        &config.base_branch,
        &config.remote,
        &config.ci.workflow_name,
        &config.ci.workflow_file,
    ] {
        if value.trim().is_empty() || value.contains(['\n', '\r']) {
            bail!("invalid empty or multiline runner configuration");
        }
    }
    if config.repository.split('/').count() != 2 {
        bail!("repository must be owner/name");
    }
    if config.ci.required_jobs.is_empty()
        || config.ci.required_jobs.iter().any(|j| j.trim().is_empty())
    {
        bail!("ci.required_jobs must list at least one job name");
    }
    if config.ci.timeout_minutes == 0 {
        bail!("ci.timeout_minutes must be at least 1");
    }
    if config.local_checks.enabled {
        if config.local_checks.steps.is_empty() || config.local_checks.budget_minutes == 0 {
            bail!("enabled local_checks need at least one step and a budget");
        }
        for step in &config.local_checks.steps {
            if step.name.trim().is_empty() || step.program.trim().is_empty() {
                bail!("every local check needs a name and a program");
            }
            if !safe_relative(&step.dir) || !step.only_if_exists.iter().all(|p| safe_relative(p)) {
                bail!(
                    "local check {:?}: dir and only_if_exists must stay inside the checkout",
                    step.name
                );
            }
        }
    }
    Ok(())
}

fn validate_workspace_path(repo: &Path, file: &str) -> Result<()> {
    let components: Vec<_> = Path::new(file).components().collect();
    let mut current = repo.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            bail!("unsafe requested path: {file}");
        };
        let next = current.join(name);
        match fs::symlink_metadata(&next) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("symlink or reparse point in requested path: {file}");
                }
                let exact_case = fs::read_dir(&current)?
                    .filter_map(std::result::Result::ok)
                    .any(|entry| entry.file_name() == *name);
                if !exact_case {
                    bail!("requested path case does not match the filesystem: {file}");
                }
                if index + 1 < components.len() && !metadata.is_dir() {
                    bail!("requested path has a non-directory parent: {file}");
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound && index + 1 == components.len() =>
            {
                // Deleted tracked files have no final directory entry. The staged-path check
                // below still requires Git to report this exact spelling.
            }
            Err(err) => return Err(err).with_context(|| format!("checking requested path {file}")),
        }
        current = next;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Running commands
// ---------------------------------------------------------------------------------------------

/// The folder holding the installed executable. Hooks, work folders, and the build cache live here,
/// outside every repository the runner serves.
fn install_dir() -> Result<PathBuf> {
    let exe = fs::canonicalize(std::env::current_exe()?)?;
    Ok(exe
        .parent()
        .context("runner executable has no parent")?
        .to_path_buf())
}

/// An empty folder Git is pointed at as its hooks folder, so hooks in the repository never run
/// with the runner's credentials.
fn installed_hooks_dir() -> Result<PathBuf> {
    let path = install_dir()?.join("disabled-hooks");
    if !path.is_dir() || fs::read_dir(&path)?.next().is_some() {
        bail!("installed runner needs an empty disabled-hooks directory beside its executable");
    }
    Ok(path)
}

/// This repository's own work folder beside the executable: `verify` (the clean checkout for local
/// checks) and `build` (the `{cache}` the checks may use).
fn work_dir(config: &Config) -> Result<PathBuf> {
    let name: String = config
        .repository
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(install_dir()?.join("work").join(name))
}

fn tail_reader(mut input: impl Read) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        result.extend_from_slice(&chunk[..count]);
        if result.len() > MAX_CAPTURE {
            result.drain(..result.len() - MAX_CAPTURE);
        }
    }
    result
}

fn run_command(mut command: Command, timeout: Duration) -> Result<Capture> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, canceller) =
        process::spawn_contained_command(command).context("spawning contained runner command")?;
    let stdout = child.stdout.take().context("missing stdout pipe")?;
    let stderr = child.stderr.take().context("missing stderr pipe")?;
    let out_thread = thread::spawn(move || tail_reader(stdout));
    let err_thread = thread::spawn(move || tail_reader(stderr));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            canceller.cancel_force();
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            bail!(
                "runner command timed out after {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    // A helper may outlive git/gh while retaining a pipe handle. Close the process tree before
    // joining drain threads, or the reader could wait forever for EOF after the parent exited.
    canceller.cancel_force();
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Capture {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        success: status.success(),
    })
}

/// Removes Git and GitHub overrides and tokens from a command's environment.
fn strip_forge_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_") || upper.starts_with("GH_") || upper.starts_with("GITHUB_") {
            command.env_remove(name);
        }
    }
}

fn strip_git_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
}

fn git(config: &Config, args: &[&str], identity: bool) -> Result<Capture> {
    let mut command = Command::new(&config.git_exe);
    let hooks = installed_hooks_dir()?;
    // Git environment overrides can redirect the index or worktree or inject config, so Git starts
    // with a clean Git-specific environment and only the pinned identity.
    strip_git_env(&mut command);
    command
        .current_dir(&config.repo)
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    if identity {
        command.env_remove("EMAIL");
        command
            .env("GIT_AUTHOR_NAME", &config.author_name)
            .env("GIT_AUTHOR_EMAIL", &config.author_email)
            .env("GIT_COMMITTER_NAME", &config.author_name)
            .env("GIT_COMMITTER_EMAIL", &config.author_email);
    }
    run_command(command, COMMAND_TIMEOUT)
}

fn gh_command(config: &Config, args: &[&str]) -> Result<Command> {
    let mut command = Command::new(&config.gh_exe);
    let hooks = installed_hooks_dir()?;
    strip_git_env(&mut command);
    command
        .current_dir(&config.repo)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GH_HOST", "github.com")
        .env_remove("GH_REPO")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", hooks);
    Ok(command)
}

fn gh(config: &Config, args: &[&str]) -> Result<Capture> {
    run_command(gh_command(config, args)?, COMMAND_TIMEOUT)
}

fn checked(output: Capture, label: &str) -> Result<String> {
    if !output.success {
        bail!("{label} failed: {}", failure_excerpt(&output));
    }
    Ok(output.stdout.trim().to_string())
}

/// The redacted, compacted reason a command failed. Some commands report on stdout rather than
/// stderr (`git diff --check` lists whitespace errors there), so an empty stderr falls back to
/// stdout instead of producing an empty detail.
fn failure_excerpt(output: &Capture) -> String {
    let text = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    compact(&redact_excerpt(text.trim()), 1200)
}

fn compact(text: &str, limit: usize) -> String {
    let cleaned = text.replace(['\r', '\n'], " ");
    cleaned.chars().take(limit).collect()
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

fn repo_lock(config: &Config) -> Result<File> {
    let common = checked(
        git(config, &["rev-parse", "--git-common-dir"], false)?,
        "finding Git metadata",
    )?;
    let common = Path::new(&common);
    let common = if common.is_absolute() {
        common.to_path_buf()
    } else {
        config.repo.join(common)
    };
    let common = fs::canonicalize(common)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(common.join("agent-pr-runner.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn commit_identity(config: &Config, expected_message: &str) -> Result<()> {
    let actual = checked(
        git(
            config,
            &["log", "-1", "--format=%an%x00%ae%x00%cn%x00%ce%x00%B"],
            false,
        )?,
        "reading commit identity",
    )?;
    let fields: Vec<&str> = actual.splitn(5, '\0').collect();
    if fields.len() != 5
        || fields[0] != config.author_name
        || fields[1] != config.author_email
        || fields[2] != config.author_name
        || fields[3] != config.author_email
        || fields[4].trim() != expected_message
        || has_attribution(fields[4])
    {
        bail!("commit attribution differs from pinned runner identity; refusing to push");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------------------------

fn pr_body(request: &Request) -> String {
    let mut body = String::from("## Summary\n");
    for line in &request.summary {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }
    body.push_str("\n## Verification Evidence\n\n| Check | Result |\n| --- | --- |\n");
    for row in &request.verification {
        body.push_str(&format!(
            "| {} | {} |\n",
            table_cell(&row.check),
            table_cell(&row.result)
        ));
    }
    body.push_str("\n## Traceability\n");
    for line in &request.traceability {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }
    body
}

/// A Markdown table cell: a literal pipe would split the cell, so it is escaped.
fn table_cell(text: &str) -> String {
    text.trim().replace('|', "\\|")
}

const CODERABBIT_START: &str =
    "<!-- This is an auto-generated comment: release notes by coderabbit.ai -->";
const CODERABBIT_END: &str =
    "<!-- end of auto-generated comment: release notes by coderabbit.ai -->";

/// CodeRabbit appends a delimited release-notes block to the PR body after review. Remove exactly
/// one complete, trailing block so that edit does not look like tampering; any other change,
/// an unterminated marker, or text after the block still fails the comparison.
fn without_review_bot_summary(body: &str) -> &str {
    let Some(start) = body.find(CODERABBIT_START) else {
        return body;
    };
    match body[start..].find(CODERABBIT_END) {
        Some(end) if body[start + end + CODERABBIT_END.len()..].trim().is_empty() => &body[..start],
        _ => body,
    }
}

#[derive(Deserialize)]
struct ExistingPr {
    number: u64,
    url: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    author: PrAuthor,
    #[serde(rename = "isCrossRepository")]
    is_cross_repository: bool,
}

#[derive(Deserialize)]
struct PrAuthor {
    login: String,
}

/// GitHub updates a PR's head commit a few seconds after a push, so a stale head is re-read for
/// about 30 seconds before it counts as a mismatch.
const HEAD_SYNC_ATTEMPTS: u32 = 7;
const HEAD_SYNC_DELAY: Duration = Duration::from_secs(5);

fn open_prs(config: &Config, request: &Request) -> Result<Vec<ExistingPr>> {
    let list = checked(
        gh(
            config,
            &[
                "pr",
                "list",
                "-R",
                &config.repository,
                "--head",
                &request.branch,
                "--base",
                &config.base_branch,
                "--state",
                "open",
                "--json",
                "number,url,headRefOid,author,isCrossRepository",
            ],
        )?,
        "finding pull request",
    )?;
    Ok(serde_json::from_str(&list)?)
}

fn heads_settled(prs: &[ExistingPr], head: &str) -> bool {
    prs.iter().all(|pr| pr.head_ref_oid == head)
}

/// The branch's open PRs, re-read while GitHub still reports an older head than `head` (or, with
/// `expect_pr`, while a PR just created has not appeared yet).
fn open_prs_synced(
    config: &Config,
    request: &Request,
    head: &str,
    expect_pr: bool,
) -> Result<Vec<ExistingPr>> {
    let mut prs = open_prs(config, request)?;
    for _ in 1..HEAD_SYNC_ATTEMPTS {
        if heads_settled(&prs, head) && !(expect_pr && prs.is_empty()) {
            break;
        }
        thread::sleep(HEAD_SYNC_DELAY);
        prs = open_prs(config, request)?;
    }
    Ok(prs)
}

fn find_or_create_pr(config: &Config, request: &Request, head: &str) -> Result<ExistingPr> {
    let body = pr_body(request);
    let existing = open_prs_synced(config, request, head, false)?;
    if existing.len() > 1 {
        bail!("multiple open PRs match this branch");
    }
    if let Some(pr) = existing.into_iter().next() {
        if pr.head_ref_oid != head {
            bail!("existing PR head does not match the newly pushed commit");
        }
        if pr.is_cross_repository || pr.author.login != config.github_login {
            bail!("existing PR was not created by the pinned operator account in this repository");
        }
        checked(
            gh(
                config,
                &[
                    "pr",
                    "edit",
                    &pr.number.to_string(),
                    "-R",
                    &config.repository,
                    "--title",
                    &request.pr_title,
                    "--body",
                    &body,
                ],
            )?,
            "updating pull request",
        )?;
        return Ok(pr);
    }
    checked(
        gh(
            config,
            &[
                "pr",
                "create",
                "-R",
                &config.repository,
                "--base",
                &config.base_branch,
                "--head",
                &request.branch,
                "--title",
                &request.pr_title,
                "--body",
                &body,
            ],
        )?,
        "creating pull request",
    )?;
    let mut prs = open_prs_synced(config, request, head, true)?;
    if prs.len() != 1
        || prs[0].head_ref_oid != head
        || prs[0].is_cross_repository
        || prs[0].author.login != config.github_login
    {
        bail!("new PR head cannot be verified");
    }
    Ok(prs.remove(0))
}

// ---------------------------------------------------------------------------------------------
// CI
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    database_id: u64,
    head_sha: String,
    status: String,
    conclusion: String,
}

fn latest_run_for_head<'a>(
    runs: &'a [WorkflowRun],
    expected: &str,
) -> Result<Option<&'a WorkflowRun>> {
    let Some(latest) = runs.first() else {
        return Ok(None);
    };
    if latest.head_sha != expected {
        bail!("CI run does not match expected commit");
    }
    Ok(Some(latest))
}

/// `pass` only when the run succeeded, every job in it succeeded, and every required job is there.
fn ci_state(run: &serde_json::Value, required: &[String]) -> &'static str {
    if run["status"] != "completed" {
        return "pending";
    }
    if run["conclusion"] != "success" {
        return "fail";
    }
    let Some(jobs) = run["jobs"].as_array() else {
        return "pending";
    };
    let mut seen = BTreeSet::new();
    for job in jobs {
        if job["status"] != "completed" || job["conclusion"] != "success" {
            return "fail";
        }
        if let Some(name) = job["name"].as_str() {
            seen.insert(name);
        }
    }
    if required.iter().all(|name| seen.contains(name.as_str())) {
        "pass"
    } else {
        "pending"
    }
}

fn run_matches_pr(
    run: &serde_json::Value,
    workflow_file: &str,
    expected: &str,
    expected_base: &str,
    pr_number: u64,
) -> bool {
    let pinned = format!("{workflow_file}@");
    run["head_sha"] == expected
        && run["event"] == "pull_request"
        && run["path"]
            .as_str()
            .is_some_and(|path| path == workflow_file || path.starts_with(&pinned))
        && run["pull_requests"].as_array().is_some_and(|prs| {
            prs.is_empty()
                || prs.iter().any(|pr| {
                    pr["number"] == pr_number
                        && pr["head"]["sha"] == expected
                        && pr["base"]["sha"] == expected_base
                })
        })
}

#[derive(Deserialize)]
struct PrCheckLink {
    name: String,
    link: String,
    #[serde(default)]
    workflow: Option<String>,
}

fn checks_link_to_run(
    checks: &[PrCheckLink],
    ci: &CiConfig,
    repository: &str,
    run_id: u64,
) -> bool {
    let prefix = format!("https://github.com/{repository}/actions/runs/{run_id}/");
    ci.required_jobs.iter().all(|required| {
        checks.iter().any(|check| {
            check.workflow.as_deref() == Some(ci.workflow_name.as_str())
                && check.name == *required
                && check.link.starts_with(&prefix)
        })
    })
}

fn ci_run(
    config: &Config,
    expected: &str,
    expected_base: &str,
    pr_number: u64,
) -> Result<Option<serde_json::Value>> {
    let runs = checked(
        gh(
            config,
            &[
                "run",
                "list",
                "-R",
                &config.repository,
                "--commit",
                expected,
                "--workflow",
                &config.ci.workflow_name,
                "--event",
                "pull_request",
                "--limit",
                "5",
                "--json",
                "databaseId,headSha,status,conclusion",
            ],
        )?,
        "finding commit-bound CI run",
    )?;
    let runs: Vec<WorkflowRun> = serde_json::from_str(&runs)?;
    let Some(latest) = latest_run_for_head(&runs, expected)? else {
        return Ok(None);
    };
    if latest.status != "completed" {
        return Ok(None);
    }
    let metadata = checked(
        gh(
            config,
            &[
                "api",
                &format!(
                    "repos/{}/actions/runs/{}",
                    config.repository, latest.database_id
                ),
            ],
        )?,
        "reading CI run provenance",
    )?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
    if !run_matches_pr(
        &metadata,
        &config.ci.workflow_file,
        expected,
        expected_base,
        pr_number,
    ) {
        bail!("CI run is not from this PR, head/base commits, and pinned workflow file");
    }
    let checks = gh(
        config,
        &[
            "pr",
            "checks",
            &pr_number.to_string(),
            "-R",
            &config.repository,
            "--json",
            "name,link,workflow",
        ],
    )?;
    let checks: Vec<PrCheckLink> = match serde_json::from_str(&checks.stdout) {
        Ok(checks) => checks,
        Err(_) => return Ok(None),
    };
    if !checks_link_to_run(&checks, &config.ci, &config.repository, latest.database_id) {
        return Ok(None);
    }
    let view = checked(
        gh(
            config,
            &[
                "run",
                "view",
                &latest.database_id.to_string(),
                "-R",
                &config.repository,
                "--json",
                "headSha,status,conclusion,event,workflowName,jobs",
            ],
        )?,
        "reading commit-bound CI jobs",
    )?;
    let run: serde_json::Value = serde_json::from_str(&view)?;
    if run["headSha"] != expected
        || run["event"] != "pull_request"
        || run["workflowName"] != config.ci.workflow_name.as_str()
        || run["conclusion"] != latest.conclusion
        || run["conclusion"] != metadata["conclusion"]
    {
        bail!("CI run identity changed or does not match expected commit");
    }
    Ok(Some(run))
}

fn verify_pr_head(
    config: &Config,
    request: &Request,
    pr: &ExistingPr,
    expected: &str,
) -> Result<String> {
    let output = checked(
        gh(
            config,
            &[
                "pr",
                "view",
                &pr.number.to_string(),
                "-R",
                &config.repository,
                "--json",
                "headRefOid,state,isDraft,baseRefName,baseRefOid,title,body,author,isCrossRepository",
                "--jq",
                ".",
            ],
        )?,
        "verifying PR head",
    )?;
    let value: serde_json::Value = serde_json::from_str(&output)?;
    if value["headRefOid"] != expected
        || value["state"] != "OPEN"
        || value["isDraft"] != false
        || value["baseRefName"] != config.base_branch.as_str()
        || value["isCrossRepository"] != false
        || value["author"]["login"] != config.github_login.as_str()
        || value["title"] != request.pr_title.as_str()
        || value["body"]
            .as_str()
            .map(|body| without_review_bot_summary(body).trim_end())
            != Some(pr_body(request).trim_end())
    {
        bail!("PR changed, became draft, or targets another base; refusing automatic merge");
    }
    value["baseRefOid"]
        .as_str()
        .map(str::to_owned)
        .context("PR has no base commit SHA")
}

fn wait_for_ci(
    config: &Config,
    request: &Request,
    pr: &ExistingPr,
    expected: &str,
    expected_base: &str,
) -> Result<&'static str> {
    let start = Instant::now();
    loop {
        if verify_pr_head(config, request, pr, expected)? != expected_base {
            bail!("base branch moved while CI ran; new integration checks are required");
        }
        if let Some(run) = ci_run(config, expected, expected_base, pr.number)? {
            let state = ci_state(&run, &config.ci.required_jobs);
            if state != "pending" {
                return Ok(state);
            }
        }
        if start.elapsed() >= config.ci_timeout() {
            bail!(
                "CI did not complete within {} minutes",
                config.ci.timeout_minutes
            );
        }
        thread::sleep(Duration::from_secs(10));
    }
}

struct FailedRun {
    run_id: u64,
    attempt: u64,
    kind: FailureKind,
    detail: String,
    log_path: String,
}

fn failed_log(config: &Config, head: &str, request_id: &str) -> Result<FailedRun> {
    let runs = checked(
        gh(
            config,
            &[
                "run",
                "list",
                "-R",
                &config.repository,
                "--commit",
                head,
                "--workflow",
                &config.ci.workflow_name,
                "--event",
                "pull_request",
                "--limit",
                "5",
                "--json",
                "databaseId,status,conclusion,attempt",
            ],
        )?,
        "finding failed CI run",
    )?;
    let runs: Vec<serde_json::Value> = serde_json::from_str(&runs)?;
    let run = runs
        .iter()
        .find(|r| r["conclusion"] == "failure")
        .context("failed CI run not found")?;
    let run_number = run["databaseId"].as_u64().context("CI run has no ID")?;
    let attempt = run["attempt"].as_u64().unwrap_or(1);
    let run_id = run_number.to_string();
    let log_path = config
        .queue_dir
        .join(format!("{request_id}.ci-failed-attempt{attempt}.log"));
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)?;
    let mut command = gh_command(
        config,
        &[
            "run",
            "view",
            &run_id,
            "-R",
            &config.repository,
            "--log-failed",
        ],
    )?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let (mut child, canceller) = process::spawn_contained_command(command)?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() >= COMMAND_TIMEOUT {
            canceller.cancel_force();
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    canceller.cancel_force();
    let mut file = File::open(&log_path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(128 * 1024)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let kind = classify_failure(&text);
    Ok(FailedRun {
        run_id: run_number,
        attempt,
        detail: describe_failure(&kind, &summarize_failure(&text)),
        kind,
        log_path: log_path.display().to_string(),
    })
}

/// Rerun only the failed jobs of `run_id`, then wait until GitHub reports the new attempt so the
/// caller's CI wait cannot read the previous attempt's failure as the new result.
fn rerun_failed_jobs(config: &Config, run_id: u64, attempt: u64) -> Result<()> {
    let id = run_id.to_string();
    checked(
        gh(
            config,
            &["run", "rerun", &id, "-R", &config.repository, "--failed"],
        )?,
        "rerunning failed CI jobs",
    )?;
    let start = Instant::now();
    loop {
        let view = checked(
            gh(
                config,
                &[
                    "run",
                    "view",
                    &id,
                    "-R",
                    &config.repository,
                    "--json",
                    "attempt",
                ],
            )?,
            "reading CI rerun attempt",
        )?;
        let view: serde_json::Value = serde_json::from_str(&view)?;
        if view["attempt"].as_u64().unwrap_or(0) > attempt {
            return Ok(());
        }
        if start.elapsed() >= Duration::from_secs(120) {
            bail!("CI rerun was requested but no new attempt appeared within 2 minutes");
        }
        thread::sleep(Duration::from_secs(5));
    }
}

// ---------------------------------------------------------------------------------------------
// Local fallback when GitHub will not start CI
// ---------------------------------------------------------------------------------------------

/// GitHub's wording when it refuses to start a job because the account is out of free Actions
/// minutes or has a billing problem. Seen on every job of the run, never in a test's own output.
const BILLING_BLOCK_MARKERS: &[&str] = &[
    "spending limit needs to be increased",
    "recent account payments have failed",
];

fn is_billing_block(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    BILLING_BLOCK_MARKERS.iter().any(|m| lower.contains(m))
}

/// One CI job as the jobs API reports it: its check-run id and how many steps it ran.
#[derive(Debug, Deserialize)]
struct BlockedJobProbe {
    id: u64,
    steps: usize,
}

/// True only when every job of the run never started (no steps) and GitHub's note on each one is
/// the billing refusal. A job that ran and failed is never treated this way, so a real red run can
/// not slip through to the local checks.
fn jobs_all_billing_blocked(jobs: &[BlockedJobProbe], notes: &[Vec<String>]) -> bool {
    !jobs.is_empty()
        && jobs.len() == notes.len()
        && jobs
            .iter()
            .zip(notes)
            .all(|(job, msgs)| job.steps == 0 && msgs.iter().any(|m| is_billing_block(m)))
}

fn ci_billing_blocked(config: &Config, head: &str) -> Result<bool> {
    let runs = checked(
        gh(
            config,
            &[
                "run",
                "list",
                "-R",
                &config.repository,
                "--commit",
                head,
                "--workflow",
                &config.ci.workflow_name,
                "--event",
                "pull_request",
                "--limit",
                "5",
                "--json",
                "databaseId,headSha,conclusion",
            ],
        )?,
        "finding CI run for the billing check",
    )?;
    let runs: Vec<serde_json::Value> = serde_json::from_str(&runs)?;
    let Some(run) = runs.first() else {
        return Ok(false);
    };
    if run["headSha"] != head || run["conclusion"] != "failure" {
        return Ok(false);
    }
    let run_id = run["databaseId"].as_u64().context("CI run has no ID")?;
    let jobs = checked(
        gh(
            config,
            &[
                "api",
                &format!("repos/{}/actions/runs/{run_id}/jobs", config.repository),
                "--jq",
                "[.jobs[] | {id, steps: (.steps | length)}]",
            ],
        )?,
        "reading CI jobs for the billing check",
    )?;
    let jobs: Vec<BlockedJobProbe> = serde_json::from_str(&jobs)?;
    let mut notes = Vec::new();
    for job in &jobs {
        let messages = checked(
            gh(
                config,
                &[
                    "api",
                    &format!(
                        "repos/{}/check-runs/{}/annotations",
                        config.repository, job.id
                    ),
                    "--jq",
                    "[.[].message]",
                ],
            )?,
            "reading CI job notes for the billing check",
        )?;
        notes.push(serde_json::from_str::<Vec<String>>(&messages)?);
    }
    Ok(jobs_all_billing_blocked(&jobs, &notes))
}

/// The full path of `program`, searching PATH. On Windows a bare name also tries each PATHEXT
/// extension, so `npm` finds `npm.cmd`; an extensionless file there (often a shell script for
/// another environment) is skipped.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }
    let extensions: Vec<String> = if cfg!(windows) && path.extension().is_none() {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    let dirs = std::env::var_os("PATH")?;
    std::env::split_paths(&dirs).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|candidate| candidate.is_file())
    })
}

/// `{cache}` in a check's argument or environment value becomes the runner's build cache folder.
fn expand(text: &str, cache: &Path) -> String {
    text.replace("{cache}", &cache.display().to_string())
}

/// Whether a step applies to this checkout (all of its `only_if_exists` paths are there).
fn step_applies(step: &CheckStep, checkout: &Path) -> bool {
    step.only_if_exists
        .iter()
        .all(|path| checkout.join(path).exists())
}

/// The command for one check. It runs the commit's own code, so it gets no Git or GitHub overrides
/// and no GitHub token from the runner's environment.
fn check_command(step: &CheckStep, checkout: &Path, cache: &Path) -> Result<Command> {
    let wanted = expand(&step.program, cache);
    let program = resolve_program(&wanted)
        .with_context(|| format!("local check {:?}: {wanted} not found", step.name))?;
    let mut command = Command::new(program);
    strip_forge_env(&mut command);
    command
        .current_dir(checkout.join(&step.dir))
        .args(step.args.iter().map(|arg| expand(arg, cache)));
    for (name, value) in &step.env {
        command.env(name, expand(value, cache));
    }
    Ok(command)
}

struct LocalChecks {
    failed: Option<String>,
    ran: Vec<String>,
    kind: FailureKind,
    excerpt: String,
    log_path: String,
}

/// Run the configured checks on a clean, detached checkout of exactly `head`, outside the agent's
/// working tree, so uncommitted edits there can not change the result. The checkout is removed
/// after.
fn run_local_checks(config: &Config, request: &Request, head: &str) -> Result<LocalChecks> {
    let work = work_dir(config)?;
    fs::create_dir_all(&work)?;
    let dir = work.join("verify");
    let cache = work.join("build");
    fs::create_dir_all(&cache)?;
    let dir_arg = dir.display().to_string();
    let _ = git(config, &["worktree", "remove", "--force", &dir_arg], false);
    if dir.exists() {
        fs::remove_dir_all(&dir).context("clearing the previous verification checkout")?;
    }
    let _ = git(config, &["worktree", "prune"], false);
    checked(
        git(
            config,
            &["worktree", "add", "--detach", &dir_arg, head],
            false,
        )?,
        "creating a clean checkout for local checks",
    )?;
    let log_path = config
        .queue_dir
        .join(format!("{}.local-checks.log", request.id));
    let result = (|| -> Result<LocalChecks> {
        let mut log = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&log_path)?;
        let start = Instant::now();
        let mut text = String::new();
        let mut ran = Vec::new();
        for step in &config.local_checks.steps {
            if !step_applies(step, &dir) {
                continue;
            }
            let left = config.checks_budget().saturating_sub(start.elapsed());
            if left.is_zero() {
                bail!(
                    "local checks took longer than {} minutes",
                    config.local_checks.budget_minutes
                );
            }
            let out = run_command(check_command(step, &dir, &cache)?, left)?;
            let section = format!("== {} ==\n{}\n{}\n", step.name, out.stdout, out.stderr);
            log.write_all(section.as_bytes())?;
            text.push_str(&section);
            ran.push(step.name.clone());
            if !out.success {
                return Ok(LocalChecks {
                    failed: Some(step.name.clone()),
                    ran,
                    kind: classify_failure(&text),
                    excerpt: summarize_failure(&text),
                    log_path: log_path.display().to_string(),
                });
            }
        }
        if ran.is_empty() {
            bail!("no local check applied to this commit; not merging without checks");
        }
        // The checked tree must still be exactly the commit: nothing edited it while it ran.
        let dirty = checked(
            git(config, &["-C", &dir_arg, "status", "--porcelain"], false)?,
            "checking the verification checkout",
        )?;
        let at = checked(
            git(config, &["-C", &dir_arg, "rev-parse", "HEAD"], false)?,
            "reading the verification checkout",
        )?;
        if !dirty.is_empty() || at != head {
            bail!("the verification checkout changed while local checks ran; not merging");
        }
        Ok(LocalChecks {
            failed: None,
            ran,
            kind: FailureKind::Unknown,
            excerpt: String::new(),
            log_path: log_path.display().to_string(),
        })
    })();
    let _ = git(config, &["worktree", "remove", "--force", &dir_arg], false);
    result
}

/// The operating system name used in the PR note and receipt.
fn os_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// The note the runner leaves on a PR it merged without GitHub CI, so the PR page says so.
fn local_checks_comment(ran: &[String]) -> String {
    let list = ran
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "GitHub could not start CI for this commit (the account is out of free Actions minutes or \
         has a billing problem). The runner ran these checks itself on a clean checkout of this \
         exact commit on the operator's {} machine: {list}. All passed. Other CI platforms were \
         not checked.",
        os_name()
    )
}

/// The author flag for the squash merge. GitHub refuses its own no-reply address there ("Invalid
/// email address"), so with one the flag is left out and GitHub uses the account's default commit
/// email, which is the no-reply address when the account keeps its email private.
fn merge_author_args(email: &str) -> Vec<&str> {
    if email
        .to_ascii_lowercase()
        .ends_with("@users.noreply.github.com")
    {
        Vec::new()
    } else {
        vec!["--author-email", email]
    }
}

// ---------------------------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------------------------

/// What a failed CI run most likely needs. Only `InfraTransient` is retried automatically, once;
/// a misclassification costs at most one rerun because merging still requires every job green.
#[derive(Debug, PartialEq)]
enum FailureKind {
    InfraTransient,
    Test(Vec<String>),
    Lint,
    Build,
    Unknown,
}

impl FailureKind {
    fn label(&self) -> &'static str {
        match self {
            FailureKind::InfraTransient => "infra_transient",
            FailureKind::Test(_) => "test_failure",
            FailureKind::Lint => "lint",
            FailureKind::Build => "build",
            FailureKind::Unknown => "unknown",
        }
    }
}

const INFRA_PATTERNS: &[&str] = &[
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway",
    "service unavailable",
    "internal server error",
    "connection reset",
    "connection refused",
    "could not resolve host",
    "temporary failure in name resolution",
    "failed to download",
    "spurious network error",
    "network failure",
    "operation timed out",
    "rate limit exceeded",
    "the runner has received a shutdown signal",
    "lost communication with the server",
    "econnreset",
    "etimedout",
];

const SETUP_STEPS: &[&str] = &[
    "set up job",
    "checkout",
    "install ",
    "cache ",
    "run actions/",
];

/// `gh run view --log-failed` lines are `job<TAB>step<TAB>timestamp text`, and color codes can
/// arrive either as raw escapes or as literal `^[[..m` text. Returns (step, text).
fn split_log_line(ansi: &Regex, line: &str) -> (String, String) {
    let mut parts = line.splitn(3, '\t');
    let (step, text) = match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(step), Some(text)) => (step, text),
        _ => ("", line),
    };
    let text = text
        .split_once(' ')
        .filter(|(stamp, _)| stamp.len() >= 20 && stamp.ends_with('Z') && stamp.contains('T'))
        .map_or(text, |(_, rest)| rest);
    (
        step.to_ascii_lowercase(),
        ansi.replace_all(text, "").into_owned(),
    )
}

/// Classifies Rust (rustc, clippy, libtest, nextest), npm, and pytest output. Anything else falls
/// back to the step name and then to `Unknown`.
fn classify_failure(log: &str) -> FailureKind {
    let nextest = Regex::new(r"\bFAIL \[\s*[\d.]+s\]\s+(?:\(\s*\d+/\s*\d+\)\s+)?\S+\s+(\S+)")
        .expect("valid nextest pattern");
    let libtest = Regex::new(r"^\s*test (\S+) \.\.\. FAILED").expect("valid libtest pattern");
    let pytest = Regex::new(r"^FAILED (\S+::\S+)").expect("valid pytest pattern");
    let rustc_code = Regex::new(r"error\[E\d{4}\]").expect("valid rustc pattern");
    let ts_code = Regex::new(r"error TS\d{4}").expect("valid TypeScript pattern");
    let ansi = Regex::new(r"(?:\x1b|\^\[)\[[0-9;]*[A-Za-z]").expect("valid ANSI pattern");
    let mut tests: Vec<String> = Vec::new();
    let (mut compile, mut code, mut lint, mut infra, mut setup, mut test_step) =
        (false, false, false, false, false, false);
    for line in log.lines() {
        let (step, text) = split_log_line(&ansi, line);
        let lower = text.to_ascii_lowercase();
        for caps in nextest
            .captures_iter(&text)
            .chain(libtest.captures_iter(&text))
            .chain(pytest.captures_iter(&text))
        {
            let name = caps[1].to_string();
            if !tests.contains(&name) && tests.len() < 10 {
                tests.push(name);
            }
        }
        compile |= lower.contains("could not compile");
        code |= rustc_code.is_match(&text) || ts_code.is_match(&text);
        lint |= step.contains("clippy")
            || step.contains("lint")
            || lower.contains("rust-clippy")
            || lower.contains("clippy::");
        infra |= INFRA_PATTERNS.iter().any(|p| lower.contains(p));
        test_step |= step.contains("test");
        setup |= lower.contains("##[error]") && SETUP_STEPS.iter().any(|s| step.starts_with(s));
    }
    if !tests.is_empty() {
        FailureKind::Test(tests)
    } else if code {
        FailureKind::Build
    } else if compile && lint {
        FailureKind::Lint
    } else if compile {
        FailureKind::Build
    } else if infra || setup {
        FailureKind::InfraTransient
    } else if lint {
        FailureKind::Lint
    } else if test_step {
        FailureKind::Test(Vec::new())
    } else {
        FailureKind::Unknown
    }
}

fn describe_failure(kind: &FailureKind, excerpt: &str) -> String {
    let lead = match kind {
        FailureKind::InfraTransient => {
            "CI infrastructure failure (network, runner, or setup)".into()
        }
        FailureKind::Test(names) if names.is_empty() => "test failure".into(),
        FailureKind::Test(names) => format!("test failure: {}", names.join(", ")),
        FailureKind::Lint => "lint failure".into(),
        FailureKind::Build => "build failure (compile or type error)".into(),
        FailureKind::Unknown => "CI failure of unknown type".into(),
    };
    format!("{lead}; {excerpt}")
}

fn summarize_failure(log: &str) -> String {
    let lines: Vec<String> = log
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error[")
                || lower.contains("error:")
                || lower.contains("error ts")
                || lower.contains("##[error]")
                || lower.contains("test result: failed")
                || lower.contains("failed to compile")
                || lower.starts_with("failed ")
        })
        .take(8)
        .map(|line| compact(line, 300))
        .collect();
    if lines.is_empty() {
        "CI failed; see the saved failed-step log".into()
    } else {
        redact_excerpt(&lines.join(" | "))
    }
}

fn redact_excerpt(text: &str) -> String {
    let tokens = Regex::new(
        r"(?:gh[oprsu]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{16,})",
    )
    .expect("valid token pattern");
    let assignments = Regex::new(r"(?i)((?:api[_-]?key|secret|token|password)\s*[:=]\s*)\S+")
        .expect("valid assignment pattern");
    let scrubbed = tokens.replace_all(text, "[redacted]");
    assignments
        .replace_all(&scrubbed, "${1}[redacted]")
        .into_owned()
}

// ---------------------------------------------------------------------------------------------
// The publishing flow
// ---------------------------------------------------------------------------------------------

fn remote_matches(actual: &str, repository: &str) -> bool {
    let tail = repository.trim_end_matches(".git");
    [
        format!("https://github.com/{tail}"),
        format!("https://github.com/{tail}.git"),
        format!("git@github.com:{tail}"),
        format!("git@github.com:{tail}.git"),
        format!("ssh://git@github.com/{tail}"),
        format!("ssh://git@github.com/{tail}.git"),
    ]
    .iter()
    .any(|allowed| actual == allowed)
}

fn remote_branch_head(config: &Config) -> Result<String> {
    let ref_name = format!("refs/heads/{}", config.base_branch);
    let response = checked(
        git(
            config,
            &["ls-remote", "--exit-code", &config.remote, &ref_name],
            false,
        )?,
        "checking remote base branch",
    )?;
    let mut fields = response.split_whitespace();
    let sha = fields.next().context("remote base branch has no SHA")?;
    if sha.len() != 40
        || !sha.bytes().all(|b| b.is_ascii_hexdigit())
        || fields.next() != Some(ref_name.as_str())
        || fields.next().is_some()
    {
        bail!("unexpected remote base branch response");
    }
    Ok(sha.to_string())
}

fn targets_base(branch: &str, base: &str) -> bool {
    // Windows refs are case-insensitive on disk, so `Main` would alias `main`.
    branch.eq_ignore_ascii_case(base)
}

fn needs_fix(
    request: &Request,
    pr_url: String,
    detail: String,
    log: Option<String>,
    kind: Option<String>,
) -> Receipt {
    Receipt {
        id: request.id.clone(),
        status: "needs_fix".into(),
        detail: compact(&detail, 1200),
        pr_url: Some(pr_url),
        diagnostic_log: log,
        failure_kind: kind,
    }
}

fn execute(config: &Config, request: &Request) -> Result<Receipt> {
    validate_request(request, &config.policy())?;
    if targets_base(&request.branch, &config.base_branch) {
        bail!("request branch must not be the base branch");
    }
    let login = checked(
        gh(config, &["api", "user", "--jq", ".login"])?,
        "checking GitHub login",
    )?;
    if login != config.github_login {
        bail!("gh is authenticated as {login}, not the pinned operator account");
    }
    let remote = checked(
        git(config, &["remote", "get-url", &config.remote], false)?,
        "checking remote",
    )?;
    if !remote_matches(&remote, &config.repository) {
        bail!("Git remote does not match the pinned GitHub repository");
    }

    let head = {
        let _lock = repo_lock(config)?;
        checked(
            git(
                config,
                &["check-ref-format", "--branch", &request.branch],
                false,
            )?,
            "checking branch name",
        )?;
        let current_branch = checked(
            git(config, &["branch", "--show-current"], false)?,
            "checking branch",
        )?;
        let current_head = checked(git(config, &["rev-parse", "HEAD"], false)?, "checking HEAD")?;
        let staged = checked(
            git(config, &["diff", "--cached", "--name-only"], false)?,
            "checking index",
        )?;
        if !staged.is_empty() {
            bail!("index already contains staged changes; refusing to mix operator work");
        }
        if request.resume {
            if current_branch != request.branch || current_head != request.expected_head {
                bail!("resume requires the checkout on the request branch at expected_head");
            }
            // Only a commit the runner itself made (pinned identity, this exact message) resumes.
            commit_identity(config, &request.commit_message)?;
            current_head
        } else {
            if request.create_branch {
                if current_branch != config.base_branch {
                    bail!("branch creation requires the checkout to be on the base branch");
                }
                if !request.expected_head.is_empty() && current_head != request.expected_head {
                    bail!("HEAD changed before branch creation");
                }
                if current_head != remote_branch_head(config)? {
                    bail!("local base branch is not up to date with the remote");
                }
                checked(
                    git(config, &["switch", "-c", &request.branch], false)?,
                    "creating feature branch",
                )?;
            } else if current_branch != request.branch || current_head != request.expected_head {
                bail!("branch or HEAD changed before staging");
            }

            for file in &request.files {
                validate_workspace_path(&config.repo, file)?;
                if config.repo.join(file).is_dir() {
                    bail!("requested stage path is a directory: {file}");
                }
            }
            let mut args = vec!["add", "--"];
            args.extend(request.files.iter().map(String::as_str));
            checked(git(config, &args, false)?, "staging requested paths")?;
            let staged = checked(
                git(config, &["diff", "--cached", "--name-only", "-z"], false)?,
                "checking staged paths",
            )?;
            let staged: BTreeSet<&str> = staged.split('\0').filter(|s| !s.is_empty()).collect();
            let allowed: BTreeSet<&str> = request.files.iter().map(String::as_str).collect();
            if staged.is_empty() || !staged.is_subset(&allowed) {
                bail!("staged paths do not match request; index left for inspection");
            }
            checked(
                git(config, &["diff", "--cached", "--check"], false)?,
                "checking staged diff",
            )?;
            checked(
                git(config, &["commit", "-m", &request.commit_message], true)?,
                "committing",
            )?;
            commit_identity(config, &request.commit_message)?;
            checked(
                git(config, &["rev-parse", "HEAD"], false)?,
                "reading new HEAD",
            )?
        }
    };

    // A failed attribution check above stops before any push. The runner never rewrites a commit
    // to hide provenance; the operator can inspect and repair a rejected local commit explicitly.
    {
        let _lock = repo_lock(config)?;
        let local = checked(
            git(config, &["rev-parse", "HEAD"], false)?,
            "checking HEAD before push",
        )?;
        if local != head {
            bail!("local HEAD changed before push");
        }
        checked(
            git(
                config,
                &["push", "-u", &config.remote, &request.branch],
                false,
            )?,
            "pushing",
        )?;
    }
    let pr = find_or_create_pr(config, request, &head)?;
    let expected_base = verify_pr_head(config, request, &pr, &head)?;
    // Wait for CI; an infrastructure-looking failure gets exactly one rerun of the failed jobs.
    // When GitHub refused to start the jobs at all for billing reasons, the runner runs the
    // configured checks itself on a clean checkout of this commit instead.
    let mut reran = false;
    let mut local_ran: Option<Vec<String>> = None;
    let failure = loop {
        if wait_for_ci(config, request, &pr, &head, &expected_base)? == "pass" {
            break None;
        }
        if ci_billing_blocked(config, &head)? {
            if !config.local_checks.enabled {
                return Ok(needs_fix(
                    request,
                    pr.url,
                    "GitHub could not start CI (billing) and local checks are off in the runner \
                     config; the operator must restore CI or enable local_checks"
                        .into(),
                    None,
                    Some("infra_transient".into()),
                ));
            }
            let checks = run_local_checks(config, request, &head)?;
            if let Some(step) = checks.failed {
                return Ok(needs_fix(
                    request,
                    pr.url,
                    format!(
                        "GitHub could not start CI (billing), so the runner checked locally; \
                         {step} failed: {}",
                        describe_failure(&checks.kind, &checks.excerpt)
                    ),
                    Some(checks.log_path),
                    Some(checks.kind.label().to_string()),
                ));
            }
            local_ran = Some(checks.ran);
            break None;
        }
        match failed_log(config, &head, &request.id) {
            Ok(run) if run.kind == FailureKind::InfraTransient && !reran => {
                rerun_failed_jobs(config, run.run_id, run.attempt)?;
                reran = true;
            }
            other => break Some(other),
        }
    };
    if let Some(result) = failure {
        let (detail, path, kind) = match result {
            Ok(run) => {
                let detail = if reran {
                    format!("{} (after one automatic rerun)", run.detail)
                } else {
                    run.detail
                };
                (
                    detail,
                    Some(run.log_path),
                    Some(run.kind.label().to_string()),
                )
            }
            Err(err) => (
                format!("CI failed; log retrieval failed: {err}"),
                None,
                None,
            ),
        };
        return Ok(needs_fix(request, pr.url, detail, path, kind));
    }

    let _lock = repo_lock(config)?;
    let local = checked(
        git(config, &["rev-parse", "HEAD"], false)?,
        "checking HEAD before merge",
    )?;
    let current_branch = checked(
        git(config, &["branch", "--show-current"], false)?,
        "checking branch before merge",
    )?;
    if local != head || current_branch != request.branch {
        bail!("local branch changed while CI ran; refusing automatic merge");
    }
    if verify_pr_head(config, request, &pr, &head)? != expected_base {
        bail!("base branch moved after CI; refusing automatic merge");
    }
    // Recheck immediately before the write. --match-head-commit closes the race with a later push,
    // and the runner deliberately does not use --admin or bypass branch protection.
    if let Some(ran) = &local_ran {
        if !ci_billing_blocked(config, &head)? {
            bail!("CI for this commit changed after the local checks; refusing to merge");
        }
        checked(
            gh(
                config,
                &[
                    "pr",
                    "comment",
                    &pr.number.to_string(),
                    "-R",
                    &config.repository,
                    "--body",
                    &local_checks_comment(ran),
                ],
            )?,
            "noting the local checks on the PR",
        )?;
    } else {
        let run = ci_run(config, &head, &expected_base, pr.number)?;
        if run
            .as_ref()
            .map(|run| ci_state(run, &config.ci.required_jobs))
            != Some("pass")
        {
            bail!("CI changed before merge; refusing to merge");
        }
    }
    let number = pr.number.to_string();
    let mut merge_args = vec![
        "pr",
        "merge",
        &number,
        "-R",
        &config.repository,
        "--squash",
        "--delete-branch",
        "--match-head-commit",
        &head,
        "--subject",
        &request.pr_title,
        "--body",
        &request.commit_message,
    ];
    merge_args.extend(merge_author_args(&config.author_email));
    let merge = checked(gh(config, &merge_args)?, "squash-merging");
    if let Err(err) = merge {
        // GitHub might have merged remotely but failed local branch cleanup. Never claim the PR is
        // unmerged merely from the CLI exit code.
        let state = gh(
            config,
            &[
                "pr",
                "view",
                &pr.number.to_string(),
                "-R",
                &config.repository,
                "--json",
                "state",
                "--jq",
                ".state",
            ],
        )?;
        if state.stdout.trim() != "MERGED" {
            return Err(err);
        }
    }
    let refresh = (|| -> Result<()> {
        checked(
            git(config, &["switch", &config.base_branch], false)?,
            "switching to base",
        )?;
        checked(
            git(
                config,
                &["pull", "--ff-only", &config.remote, &config.base_branch],
                false,
            )?,
            "refreshing base",
        )?;
        checked(
            git(config, &["fetch", "--prune", &config.remote], false)?,
            "pruning deleted remote branches",
        )?;
        // `gh pr merge -R` deletes only the remote branch. Remove the local one too, but only while
        // it still points at the exact commit that was reviewed and merged.
        let local = git(
            config,
            &["rev-parse", "--verify", "--quiet", &request.branch],
            false,
        )?;
        if local.success && local.stdout.trim() == head {
            let _ = git(config, &["branch", "-D", "--", &request.branch], false)?;
        }
        Ok(())
    })();
    if let Err(err) = refresh {
        return Ok(Receipt {
            id: request.id.clone(),
            status: "merged_needs_refresh".into(),
            detail: format!("PR merged; local base refresh needs attention: {err}"),
            pr_url: Some(pr.url),
            diagnostic_log: None,
            failure_kind: None,
        });
    }
    Ok(Receipt {
        id: request.id.clone(),
        status: "merged".into(),
        detail: if local_ran.is_some() {
            format!(
                "GitHub could not start CI (billing); the runner's local checks passed on a clean \
                 checkout ({} only); PR squash-merged and base refreshed",
                os_name()
            )
        } else {
            "CI passed; PR squash-merged and base refreshed".into()
        },
        pr_url: Some(pr.url),
        diagnostic_log: None,
        failure_kind: None,
    })
}

// ---------------------------------------------------------------------------------------------
// Queue, service, and commands
// ---------------------------------------------------------------------------------------------

fn queue_path(queue: &Path, id: &str, suffix: &str) -> PathBuf {
    queue.join(format!("{id}.{suffix}.json"))
}

fn submit(queue: &Path, request_path: &Path) -> Result<()> {
    let queue = fs::canonicalize(queue).context("runner queue does not exist")?;
    let bytes = fs::read(request_path)?;
    if bytes.len() > 64 * 1024 {
        bail!("request exceeds 64 KiB");
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    validate_request(&request, &Policy::default())?;
    let target = queue_path(&queue, &request.id, "request");
    let result = queue_path(&queue, &request.id, "result");
    {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(queue.join("submit.lock"))?;
        lock.lock_exclusive()?;
        if target.exists()
            || result.exists()
            || queue_path(&queue, &request.id, "processing").exists()
        {
            bail!("request id already exists; choose a new id");
        }
        write_json(&target, &request)?;
    }
    let start = Instant::now();
    loop {
        if let Ok(content) = fs::read(&result) {
            let receipt: Receipt = serde_json::from_slice(&content)?;
            println!("{}", serde_json::to_string(&receipt)?);
            if receipt.status == "merged" {
                return Ok(());
            }
            bail!("runner returned {}: {}", receipt.status, receipt.detail);
        }
        if start.elapsed() >= SUBMIT_WAIT {
            bail!(
                "no receipt yet; the request is still queued. Check later with: status QUEUE_DIR {}",
                request.id
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Prints the receipt for `id`, or where the request is when it has none yet. Lets an agent whose
/// shell times out before `submit` returns pick the result up later.
fn status(queue: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("invalid request id");
    }
    let queue = fs::canonicalize(queue).context("runner queue does not exist")?;
    if let Ok(content) = fs::read(queue_path(&queue, id, "result")) {
        let receipt: Receipt = serde_json::from_slice(&content)?;
        println!("{}", serde_json::to_string(&receipt)?);
        return Ok(());
    }
    let state = if queue_path(&queue, id, "processing").exists() {
        "processing"
    } else if queue_path(&queue, id, "request").exists() {
        "queued"
    } else {
        "unknown"
    };
    println!("{}", serde_json::json!({"id": id, "status": state}));
    Ok(())
}

fn load_installed_config(config_path: &Path) -> Result<Config> {
    let config: Config = serde_json::from_slice(&fs::read(config_path)?)
        .with_context(|| format!("reading {}", config_path.display()))?;
    validate_config(&config)?;
    let repo = fs::canonicalize(&config.repo)?;
    if fs::canonicalize(config_path)?.starts_with(&repo)
        || fs::canonicalize(std::env::current_exe()?)?.starts_with(&repo)
    {
        bail!("runner binary and config must be installed outside the agent-writable repository");
    }
    installed_hooks_dir()?;
    Ok(config)
}

fn doctor(config_path: &Path) -> Result<()> {
    println!("{}", doctor_report(config_path)?);
    Ok(())
}

/// What `doctor` checks: the config, the install folders, the `gh` login, the remote, and the
/// programs the local checks need.
fn doctor_report(config_path: &Path) -> Result<serde_json::Value> {
    let config = load_installed_config(config_path)?;
    let login = checked(
        gh(&config, &["api", "user", "--jq", ".login"])?,
        "checking GitHub login",
    )?;
    if login != config.github_login {
        bail!("gh login differs from pinned operator account");
    }
    let remote = checked(
        git(&config, &["remote", "get-url", &config.remote], false)?,
        "checking remote",
    )?;
    if !remote_matches(&remote, &config.repository) {
        bail!("Git remote differs from pinned repository");
    }
    let mut programs = BTreeSet::new();
    if config.local_checks.enabled {
        // A program inside `{cache}` is made by an earlier step, so it can not be checked yet.
        for step in config
            .local_checks
            .steps
            .iter()
            .filter(|step| !step.program.contains("{cache}"))
        {
            if resolve_program(&step.program).is_none() {
                bail!(
                    "local check {:?} needs {} on this account's PATH",
                    step.name,
                    step.program
                );
            }
            programs.insert(step.program.as_str());
        }
    }
    Ok(serde_json::json!({
        "status": "ready",
        "repository": config.repository,
        "login": login,
        "required_jobs": config.ci.required_jobs,
        "local_checks": config.local_checks.enabled,
        "check_programs": programs,
    }))
}

/// Between requests, clear this repository's build cache once it passes the size limit, so local
/// checks can not fill the disk over time. Only the runner's own `work/<repo>/build` folder is ever
/// removed, never a link, and only while no request is waiting. What happened is appended to
/// `build-cache-cleanup.log` in the queue.
fn tidy_build_cache(config: &Config) {
    let Ok(work) = work_dir(config) else {
        return;
    };
    let cache = work.join("build");
    let Ok(meta) = fs::symlink_metadata(&cache) else {
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return;
    }
    let size = size_of(&cache);
    if size <= config.cache_limit_gb * 1024 * 1024 * 1024 || queue_busy(&config.queue_dir) {
        return;
    }
    let outcome = match fs::remove_dir_all(&cache) {
        Ok(()) => format!("cleared {} MB", size / (1024 * 1024)),
        Err(err) => format!("could not clear: {err}"),
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.queue_dir.join("build-cache-cleanup.log"))
    {
        let _ = writeln!(log, "{stamp} {}: {outcome}", cache.display());
    }
}

fn queue_busy(queue: &Path) -> bool {
    fs::read_dir(queue).is_ok_and(|read| {
        read.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".request.json") || name.ends_with(".processing.json")
        })
    })
}

/// The total size of the files under `path`, not following links.
fn size_of(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Holds an exclusive lock for the life of one `serve` process. A second instance would mark the
/// first one's in-flight request as `needs_inspection` and race it for queued requests.
fn serve_lock(queue: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(queue.join("serve.lock"))?;
    if file.try_lock_exclusive().is_err() {
        bail!("another runner is already serving this queue");
    }
    Ok(file)
}

fn serve(config_path: &Path) -> Result<()> {
    let config = load_installed_config(config_path)?;
    let _serve_lock = serve_lock(&config.queue_dir)?;
    // A crash after taking a request might already have committed or pushed it. Never replay that
    // request automatically. Return an inspection-required receipt instead.
    for entry in fs::read_dir(&config.queue_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".processing.json") else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        let result = queue_path(&config.queue_dir, id, "result");
        if !result.exists() {
            write_json(
                &result,
                &Receipt {
                    id: id.into(),
                    status: "needs_inspection".into(),
                    detail: "runner stopped mid-request; inspect branch, commit, and PR before \
                             resubmitting with a new id"
                        .into(),
                    pr_url: None,
                    diagnostic_log: None,
                    failure_kind: None,
                },
            )?;
        }
    }
    eprintln!(
        "agent-pr-runner serving {} from queue {}",
        config.repository,
        config.queue_dir.display()
    );
    loop {
        let mut pending = Vec::new();
        for entry in fs::read_dir(&config.queue_dir)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".request.json"))
            {
                pending.push(path);
            }
        }
        pending.sort();
        for path in pending {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .context("invalid request filename")?;
            let id = name
                .strip_suffix(".request.json")
                .context("invalid request suffix")?;
            if !valid_id(id) {
                continue;
            }
            let processing = queue_path(&config.queue_dir, id, "processing");
            fs::rename(&path, &processing)?;
            let outcome = (|| -> Result<Receipt> {
                let bytes = fs::read(&processing)?;
                let request: Request = serde_json::from_slice(&bytes)?;
                if request.id != id {
                    bail!("request filename and id differ");
                }
                execute(&config, &request)
            })();
            let receipt = outcome.unwrap_or_else(|err| Receipt {
                id: id.into(),
                status: "error".into(),
                detail: compact(&redact_excerpt(&format!("{err:#}")), 1200),
                pr_url: None,
                diagnostic_log: None,
                failure_kind: None,
            });
            eprintln!("{id}: {}", receipt.status);
            write_json(&queue_path(&config.queue_dir, id, "result"), &receipt)?;
            fs::remove_file(&processing)?;
            tidy_build_cache(&config);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

const USAGE: &str = "usage:
  agent-pr-runner init --repo PATH [OPTIONS]    write a config for a repository (see below)
  agent-pr-runner snippet CONFIG.json           print the agent instructions with real paths
  agent-pr-runner doctor CONFIG.json            check the install, login, remote, and check programs
  agent-pr-runner serve CONFIG.json             process queued requests until stopped
  agent-pr-runner submit QUEUE_DIR REQUEST.json queue a request and wait for its receipt
  agent-pr-runner status QUEUE_DIR ID           print a request's receipt or where it is

init options:
  --name NAME                       config and queue name (default: the repository folder name)
  --workflow FILE                   CI workflow, when several run on pull_request
  --job NAME                        a required CI job name; repeat for each (default: read from
                                    the workflow file, or its latest GitHub run)
  --local-checks off|auto|rust|node|python
                                    billing fallback checks (default: off)
  --protect-manifests               make the project's manifest files operator-only
  --force                           replace an existing config of the same name";

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.as_slice() {
        [_, mode, config] if mode == "serve" => serve(Path::new(config)),
        [_, mode, config] if mode == "doctor" => doctor(Path::new(config)),
        [_, mode, config] if mode == "snippet" => setup::snippet(Path::new(config)),
        [_, mode, rest @ ..] if mode == "init" => {
            let rest: Vec<String> = rest
                .iter()
                .map(|arg| {
                    arg.to_str()
                        .map(String::from)
                        .context("arguments must be UTF-8")
                })
                .collect::<Result<_>>()?;
            setup::init(&rest)
        }
        [_, mode, queue, request] if mode == "submit" => {
            submit(Path::new(queue), Path::new(request))
        }
        [_, mode, queue, id] if mode == "status" => status(Path::new(queue), &id.to_string_lossy()),
        [_, mode] if mode == "--version" || mode == "version" => {
            println!("agent-pr-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            bail!("unrecognized arguments")
        }
    }
}

#[cfg(test)]
mod tests;
