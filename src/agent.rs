//! Commands that make a request hard to get wrong. `preflight` lists every problem with a request
//! before it is queued, `publish` builds the request from Git's own state, and every receipt
//! carries a stable action code and the exact next command.

use super::*;

/// Receipt action codes. They are part of the agent-facing contract: never rename one.
pub(crate) const DONE: &str = "done";
pub(crate) const FIX_CODE: &str = "fix_code";
pub(crate) const FIX_REQUEST: &str = "fix_request";
pub(crate) const RESUME: &str = "resume";
pub(crate) const REPORT: &str = "report_to_operator";
pub(crate) const WAIT: &str = "wait";
pub(crate) const UNKNOWN_ID: &str = "unknown_id";
/// A receipt from a runner older than action codes, whose `error` can not be told apart.
pub(crate) const UNCLASSIFIED: &str = "unclassified";

/// The file `serve` keeps fresh in the queue, so `preflight` can tell whether it runs, and on
/// which version of the config.
const HEARTBEAT_FILE: &str = "serve.json";
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// Seconds after which a heartbeat means the runner has stopped.
const HEARTBEAT_STALE: u64 = 120;
/// The longest `status --wait` holds an agent's shell.
pub(crate) const MAX_STATUS_WAIT: u64 = 900;
const MAX_REQUEST: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Problems
// ---------------------------------------------------------------------------------------------

/// One thing that would stop or spoil a request, and how to fix it.
#[derive(Debug)]
pub(crate) struct Problem {
    pub(crate) code: &'static str,
    pub(crate) problem: String,
    pub(crate) fix: String,
    /// A warning is printed but does not stop the request.
    pub(crate) blocking: bool,
}

impl Problem {
    pub(crate) fn new(
        code: &'static str,
        problem: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            code,
            problem: problem.into(),
            fix: fix.into(),
            blocking: true,
        }
    }

    fn warning(code: &'static str, problem: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            blocking: false,
            ..Self::new(code, problem, fix)
        }
    }

    /// One line: what is wrong and the fix.
    fn line(&self) -> String {
        let kind = if self.blocking { "problem" } else { "warning" };
        format!("{kind} {}: {}. fix: {}", self.code, self.problem, self.fix)
    }
}

/// Prints every problem, one per line, and fails when any of them blocks.
fn report(problems: &[Problem]) -> Result<()> {
    for problem in problems {
        println!("{}", problem.line());
    }
    let blocking = problems.iter().filter(|p| p.blocking).count();
    if blocking > 0 {
        bail!("{blocking} problem(s); nothing was queued");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Commands an agent can paste
// ---------------------------------------------------------------------------------------------

/// One argument as PowerShell, cmd, and bash all read it when it is plain; otherwise quoted for
/// PowerShell, the operator's shell.
pub(crate) fn shell_arg(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,@".contains(c))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "''"))
    }
}

/// The runner program as a command starts it. A quoted path needs PowerShell's `&`; a plain
/// forward-slash path runs unchanged in PowerShell, cmd, and bash.
pub(crate) fn program(exe: &str) -> String {
    let quoted = shell_arg(exe);
    if quoted.starts_with('\'') {
        format!("& {quoted}")
    } else {
        quoted
    }
}

fn command_line(args: &[&str]) -> Result<String> {
    let exe = setup::plain(&fs::canonicalize(std::env::current_exe()?)?);
    let mut line = program(&exe);
    for arg in args {
        line.push(' ');
        line.push_str(&shell_arg(arg));
    }
    Ok(line)
}

fn plain_path(path: &Path) -> Result<String> {
    Ok(setup::plain(&fs::canonicalize(path)?))
}

pub(crate) fn status_command(queue: &Path, id: &str) -> Result<String> {
    command_line(&["status", &plain_path(queue)?, id, "--wait", "300"])
}

fn retry_command(config_path: &Path, id: &str, new_evidence: bool) -> Result<String> {
    let config = plain_path(config_path)?;
    let mut args = vec!["publish", config.as_str(), "--retry", id];
    if new_evidence {
        args.extend(["--verify", "CHECK=RESULT"]);
    }
    command_line(&args)
}

fn serve_command(config_path: &Path) -> String {
    plain_path(config_path)
        .and_then(|config| command_line(&["serve", &config]))
        .unwrap_or_else(|_| "agent-pr-runner serve CONFIG.json".into())
}

// ---------------------------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------------------------

/// Fills in a receipt's action code, when its maker did not, and the command for it.
pub(crate) fn finish_receipt(
    receipt: &mut Receipt,
    config: &Config,
    config_path: &Path,
    request: Option<&Request>,
) {
    if receipt.action.is_none() {
        let action = match receipt.status.as_str() {
            "merged" => DONE,
            "needs_fix" => FIX_CODE,
            "error" if request.is_some_and(|r| committed(config, r)) => RESUME,
            "error" => FIX_REQUEST,
            _ => REPORT,
        };
        receipt.action = Some(action.into());
    }
    receipt.next = match receipt.action.as_deref() {
        Some(FIX_CODE) => retry_command(config_path, &receipt.id, true).ok(),
        // An unreadable request can not be retried; its detail says what to fix.
        Some(FIX_REQUEST | RESUME) if request.is_some() => {
            retry_command(config_path, &receipt.id, false).ok()
        }
        _ => None,
    };
}

/// The action for a receipt an older runner wrote without one.
pub(crate) fn legacy_action(status: &str) -> &'static str {
    match status {
        "merged" => DONE,
        "needs_fix" => FIX_CODE,
        "error" => UNCLASSIFIED,
        _ => REPORT,
    }
}

/// Whether a failed request's commit exists: the checkout is on its branch, at a commit the
/// runner made with this message. The rest is then a resume, not a new commit.
fn committed(config: &Config, request: &Request) -> bool {
    let read = |args: &[&str]| {
        git(config, args, false)
            .ok()
            .filter(|out| out.success)
            .map(|out| out.stdout.trim().to_string())
    };
    let on_branch = read(&["branch", "--show-current"]).as_deref() == Some(&request.branch);
    let moved =
        request.resume || read(&["rev-parse", "HEAD"]).as_deref() != Some(&request.expected_head);
    on_branch && moved && commit_identity(config, &request.commit_message).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Heartbeat {
    pid: u32,
    /// The config file `serve` loaded, as a plain path.
    config: String,
    /// A fingerprint of that file's bytes when `serve` loaded it.
    config_hash: String,
    /// Unix seconds of the latest beat.
    beat: u64,
}

/// FNV-1a: a short, stable fingerprint. Not for security, only to notice a changed file.
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Records, now and every 30 seconds while `serve` lives, which config it runs on. A thread does
/// it, so a 30-minute CI wait does not look like a stopped runner.
pub(crate) fn start_heartbeat(config_path: &Path, config_bytes: &[u8], queue: &Path) -> Result<()> {
    let mut beat = Heartbeat {
        pid: std::process::id(),
        config: plain_path(config_path)?,
        config_hash: fingerprint(config_bytes),
        beat: unix_now(),
    };
    let path = queue.join(HEARTBEAT_FILE);
    write_json(&path, &beat)?;
    thread::spawn(move || loop {
        thread::sleep(HEARTBEAT_EVERY);
        beat.beat = unix_now();
        let _ = write_json(&path, &beat);
    });
    Ok(())
}

fn heartbeat_problems(config_path: &Path, queue: &Path) -> Vec<Problem> {
    let start = serve_command(config_path);
    let beat: Option<Heartbeat> = fs::read(queue.join(HEARTBEAT_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let Some(beat) = beat else {
        return vec![Problem::warning(
            "runner_not_seen",
            "no heartbeat from serve (it is stopped, or older than this version); the request \
             will wait in the queue until it runs",
            format!("ask the operator to start it: {start}"),
        )];
    };
    if unix_now().saturating_sub(beat.beat) > HEARTBEAT_STALE {
        return vec![Problem::warning(
            "runner_stopped",
            format!(
                "serve last reported {} seconds ago; the request will wait until it runs again",
                unix_now().saturating_sub(beat.beat)
            ),
            format!("ask the operator to start it: {start}"),
        )];
    }
    let here = plain_path(config_path).unwrap_or_default();
    let current = fs::read(config_path)
        .map(|b| fingerprint(&b))
        .unwrap_or_default();
    if !beat.config.eq_ignore_ascii_case(&here) {
        return vec![Problem::new(
            "other_config",
            format!("serve on this queue runs {}, not {here}", beat.config),
            "use the config serve runs, or ask the operator which one is current",
        )];
    }
    if beat.config_hash != current {
        return vec![Problem::new(
            "stale_config",
            "the config changed after serve started, and serve still uses the old copy",
            format!(
                "ask the operator to stop serve (process {}) and start it again: {start}",
                beat.pid
            ),
        )];
    }
    Vec::new()
}

// ---------------------------------------------------------------------------------------------
// Checking a request against the repository
// ---------------------------------------------------------------------------------------------

/// Every path `git status` reports as changed, new, or deleted, as Git spells it.
fn changed_files(config: &Config) -> Result<BTreeSet<String>> {
    let out = git(
        config,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
        false,
    )?;
    if !out.success {
        bail!("reading git status failed: {}", failure_excerpt(&out));
    }
    Ok(parse_status(&out.stdout))
}

/// Paths from `git status --porcelain=v1 -z`. A rename lists the new path, then the old one.
fn parse_status(text: &str) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let mut entries = text.split('\0');
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = entry.split_at(3);
        files.insert(path.to_string());
        if code.contains(['R', 'C']) {
            if let Some(original) = entries.next() {
                if code.contains('R') {
                    files.insert(original.to_string());
                }
            }
        }
    }
    files
}

fn read_git(config: &Config, args: &[&str], label: &str) -> Result<String> {
    checked(git(config, args, false)?, label)
}

/// What the repository, queue, and running runner say about a request that is valid on its own.
fn state_problems(
    config: &Config,
    config_path: &Path,
    request: &Request,
    changed: &BTreeSet<String>,
    warn_unlisted: bool,
) -> Result<Vec<Problem>> {
    let mut found = Vec::new();
    let queue = fs::canonicalize(&config.queue_dir).context("runner queue does not exist")?;
    if id_used(&queue, &request.id) {
        found.push(Problem::new(
            "id_used",
            format!("request id {} was already used", request.id),
            "pick a new id (publish picks one for you)",
        ));
    }
    let remote = read_git(
        config,
        &["remote", "get-url", &config.remote],
        "checking remote",
    )?;
    if !remote_matches(&remote, &config.repository) {
        found.push(Problem::new(
            "remote_mismatch",
            format!(
                "the {} remote is {remote}, but the config pins {}",
                config.remote, config.repository
            ),
            "ask the operator to rerun init --force for this repository, then restart serve",
        ));
    }
    let staged = read_git(
        config,
        &["diff", "--cached", "--name-only"],
        "checking index",
    )?;
    if !staged.is_empty() {
        found.push(Problem::new(
            "index_not_empty",
            "files are already staged; the runner refuses to mix them in",
            "ask the operator to unstage them (git restore --staged .)",
        ));
    }
    let branch = read_git(config, &["branch", "--show-current"], "checking branch")?;
    let head = read_git(config, &["rev-parse", "HEAD"], "checking HEAD")?;
    if targets_base(&request.branch, &config.base_branch) {
        found.push(Problem::new(
            "branch_is_base",
            format!("the request targets the base branch {}", config.base_branch),
            "use a feature branch such as fix/short-purpose",
        ));
    } else if request.create_branch {
        if !targets_base(&branch, &config.base_branch) {
            found.push(Problem::new(
                "not_on_base",
                format!(
                    "create_branch needs the checkout on {}, but it is on {branch}",
                    config.base_branch
                ),
                format!("set create_branch to false and branch to {branch} (publish does this)"),
            ));
        } else {
            if !request.expected_head.is_empty() && request.expected_head != head {
                found.push(stale_head(&head));
            }
            let exists = git(
                config,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", request.branch),
                ],
                false,
            )?;
            if exists.success {
                found.push(Problem::new(
                    "branch_exists",
                    format!("branch {} already exists", request.branch),
                    "pick another branch name (publish: --branch NAME)",
                ));
            }
            match remote_branch_head(config) {
                Ok(remote_head) if remote_head != head => found.push(Problem::new(
                    "base_behind",
                    format!(
                        "local {} is not at GitHub's {}",
                        config.base_branch, config.base_branch
                    ),
                    format!(
                        "ask the operator to update it (git pull --ff-only {} {})",
                        config.remote, config.base_branch
                    ),
                )),
                Ok(_) => {}
                Err(err) => found.push(Problem::warning(
                    "remote_unreachable",
                    format!(
                        "could not compare with GitHub: {}",
                        compact(&format!("{err:#}"), 200)
                    ),
                    "check the network; the runner repeats this check",
                )),
            }
        }
    } else {
        if branch != request.branch {
            found.push(Problem::new(
                "wrong_branch",
                format!("the checkout is on {branch}, not {}", request.branch),
                format!(
                    "set branch to {branch}, or start from {} with create_branch true",
                    config.base_branch
                ),
            ));
        }
        if request.expected_head != head {
            found.push(stale_head(&head));
        }
    }
    if !request.resume {
        // An operator-only path is already reported by `request_problems`.
        let policy = config.policy();
        for file in &request.files {
            if !changed.contains(file) && !protected_path(file, &policy) {
                found.push(Problem::new(
                    "not_changed",
                    format!("{file} has no change in git status"),
                    "remove it from files, or check its spelling and case",
                ));
            }
        }
        if warn_unlisted {
            let listed: BTreeSet<&String> = request.files.iter().collect();
            for file in changed.iter().filter(|f| !listed.contains(f)) {
                found.push(Problem::warning(
                    "not_listed",
                    format!("{file} is changed but not in files; it stays uncommitted"),
                    "add it to files if it belongs to this change",
                ));
            }
        }
    }
    found.extend(author_problems(config, request));
    found.extend(heartbeat_problems(config_path, &queue));
    Ok(found)
}

fn stale_head(head: &str) -> Problem {
    Problem::new(
        "stale_head",
        "expected_head is not the current HEAD (a commit happened since the request was written)",
        format!("set expected_head to {head} (publish fills it in)"),
    )
}

/// Pushes fail when a commit carries a private email and the GitHub account blocks that. The
/// runner's own commits use the config's address; commits made outside it are checked here.
fn author_problems(config: &Config, request: &Request) -> Vec<Problem> {
    let mut found = Vec::new();
    if !config
        .author_email
        .to_ascii_lowercase()
        .ends_with("@users.noreply.github.com")
    {
        found.push(Problem::warning(
            "private_author_email",
            format!(
                "the config commits as {}; GitHub rejects the push (GH007) if that address is \
                 private",
                config.author_email
            ),
            "ask the operator to set author_email to the account's no-reply address \
             (ID+login@users.noreply.github.com) and restart serve",
        ));
    }
    if request.create_branch {
        return found;
    }
    let range = format!(
        "refs/remotes/{}/{}..HEAD",
        config.remote, config.base_branch
    );
    let Ok(log) = git(config, &["log", "--format=%h%x00%ae%x00%ce", &range], false) else {
        return found;
    };
    if !log.success {
        return found;
    }
    for line in log.stdout.lines() {
        let fields: Vec<&str> = line.split('\0').collect();
        if let [sha, author, committer] = fields.as_slice() {
            if *author != config.author_email || *committer != config.author_email {
                found.push(Problem::warning(
                    "outside_commit",
                    format!("commit {sha} on this branch was made outside the runner as {author}"),
                    "if the push is rejected for a private email, ask the operator to rewrite \
                     that commit with the no-reply address",
                ));
            }
        }
    }
    found
}

fn read_request(path: &Path) -> Result<Request> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > MAX_REQUEST {
        bail!("request exceeds 64 KiB");
    }
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// `preflight CONFIG REQUEST`: every problem `serve` would hit, before anything is queued.
pub(crate) fn preflight(config_path: &Path, request_path: &Path) -> Result<()> {
    let config = load_installed_config(config_path)?;
    let request = read_request(request_path)?;
    let mut problems = request_problems(&request, &config.policy());
    let changed = changed_files(&config)?;
    problems.extend(state_problems(
        &config,
        config_path,
        &request,
        &changed,
        true,
    )?);
    report(&problems)?;
    let queue = plain_path(&config.queue_dir)?;
    let request_text = plain_path(request_path)?;
    println!("ok: ready to queue");
    println!(
        "next: {}",
        command_line(&["submit", &queue, &request_text])?
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PublishOptions {
    message: Option<String>,
    title: Option<String>,
    summary: Vec<String>,
    verification: Vec<Evidence>,
    traceability: Vec<String>,
    branch: Option<String>,
    exclude: BTreeSet<String>,
    id: Option<String>,
    retry: Option<String>,
    dry_run: bool,
}

fn parse_publish(args: &[String]) -> Result<PublishOptions> {
    let mut options = PublishOptions::default();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        if flag == "--dry-run" {
            options.dry_run = true;
            continue;
        }
        let value = iter
            .next()
            .with_context(|| format!("{flag} needs a value"))?
            .clone();
        match flag.as_str() {
            "--message" => options.message = Some(value),
            "--title" => options.title = Some(value),
            "--summary" => options.summary.push(value),
            "--summary-file" => {
                let text = fs::read_to_string(&value)
                    .with_context(|| format!("reading --summary-file {value}"))?;
                options.summary.extend(
                    text.lines()
                        .map(|line| line.trim().trim_start_matches("- ").trim())
                        .filter(|line| !line.is_empty())
                        .map(String::from),
                );
            }
            "--verify" => options.verification.push(parse_evidence(&value)?),
            "--trace" => options.traceability.push(value),
            "--branch" => options.branch = Some(value),
            "--exclude" => {
                options.exclude.insert(value.replace('\\', "/"));
            }
            "--id" => options.id = Some(value),
            "--retry" => options.retry = Some(value),
            _ => bail!(
                "unknown publish option {flag}; run agent-pr-runner with no arguments for usage"
            ),
        }
    }
    Ok(options)
}

/// `"cargo test=212 passed"`: the check, then the observed result after the last `=`.
fn parse_evidence(value: &str) -> Result<Evidence> {
    value
        .rsplit_once('=')
        .map(|(check, result)| (check.trim(), result.trim()))
        .filter(|(check, result)| !check.is_empty() && !result.is_empty())
        .map(|(check, result)| Evidence {
            check: check.into(),
            result: result.into(),
        })
        .with_context(|| format!("--verify needs \"check=result\", got {value:?}"))
}

/// `fix: handle empty input` becomes `fix/handle-empty-input`; `feat` becomes `feature/`.
fn branch_from_message(message: &str) -> String {
    let (head, rest) = message.split_once(": ").unwrap_or(("chore", message));
    let kind = head.trim_end_matches('!').split('(').next().unwrap_or("");
    let prefix = match kind {
        "feat" => "feature",
        kind if !kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase()) => kind,
        _ => "chore",
    };
    let mut slug = String::new();
    for c in rest.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    if slug.len() > 40 {
        slug.truncate(40);
        if let Some(cut) = slug.rfind('-').filter(|&cut| cut > 10) {
            slug.truncate(cut);
        }
    }
    let slug = slug.trim_matches('-');
    format!("{prefix}/{}", if slug.is_empty() { "change" } else { slug })
}

/// The first unused `<branch>-<n>` id in the queue.
fn fresh_id(queue: &Path, branch: &str) -> String {
    let base: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(70)
        .collect();
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !id_used(queue, id))
        .expect("an unused id exists")
}

/// The request and receipt of an earlier attempt, for `--retry`.
fn previous_attempt(queue: &Path, id: &str) -> Result<(Request, String)> {
    if !valid_id(id) {
        bail!("invalid --retry id");
    }
    let receipt: Receipt = match fs::read(queue_path(queue, id, "result")) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(_) => bail!(
            "{id} has no receipt yet; wait for it with: {}",
            status_command(queue, id)?
        ),
    };
    let action = receipt
        .action
        .clone()
        .unwrap_or_else(|| legacy_action(&receipt.status).into());
    match action.as_str() {
        DONE => bail!("{id} is already merged; start the next change with publish --message"),
        REPORT => bail!("{id} needs the operator, not a retry: {}", receipt.detail),
        _ => {}
    }
    let request = read_request(&queue_path(queue, id, "sent")).with_context(|| {
        format!(
            "{id} was sent by an older runner that kept no copy; publish it again with --message"
        )
    })?;
    Ok((request, action))
}

/// `publish CONFIG [options]`: build the request from Git's state, check it, and queue it.
pub(crate) fn publish(config_path: &Path, args: &[String]) -> Result<()> {
    let options = parse_publish(args)?;
    let config = load_installed_config(config_path)?;
    let queue = fs::canonicalize(&config.queue_dir).context("runner queue does not exist")?;
    let previous = options
        .retry
        .as_deref()
        .map(|id| previous_attempt(&queue, id))
        .transpose()?;
    let (old, action) = match &previous {
        Some((request, action)) => (Some(request), action.as_str()),
        None => (None, ""),
    };
    let resume = action == RESUME;
    let mut problems = Vec::new();

    let commit_message = match (&options.message, old) {
        (Some(message), Some(old)) if resume && *message != old.commit_message => {
            bail!("a resume finishes the existing commit; leave out --message")
        }
        (Some(message), _) => message.clone(),
        (None, Some(old)) => old.commit_message.clone(),
        (None, None) => bail!("--message is required, for example --message \"fix: what changed\""),
    };
    let pr_title = options
        .title
        .clone()
        .or_else(|| {
            options
                .message
                .is_none()
                .then(|| old.map(|o| o.pr_title.clone()))
                .flatten()
        })
        .unwrap_or_else(|| commit_message.clone());
    let pick = |given: &Vec<String>, earlier: Option<&Vec<String>>| {
        if given.is_empty() {
            earlier.cloned().unwrap_or_default()
        } else {
            given.clone()
        }
    };
    let summary = pick(&options.summary, old.map(|o| &o.summary));
    let traceability = pick(&options.traceability, old.map(|o| &o.traceability));
    let verification = if !options.verification.is_empty() {
        options.verification
    } else {
        if action == FIX_CODE {
            problems.push(Problem::new(
                "evidence_needed",
                "the earlier evidence predates the fix",
                "rerun the gates and pass each result with --verify \"check=result\"",
            ));
        }
        old.map(|o| {
            o.verification
                .iter()
                .map(|row| Evidence {
                    check: row.check.clone(),
                    result: row.result.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
    };

    let current = read_git(&config, &["branch", "--show-current"], "reading the branch")?;
    if current.is_empty() {
        bail!("the checkout is not on a branch (detached HEAD); ask the operator to switch to one");
    }
    let head = read_git(&config, &["rev-parse", "HEAD"], "reading HEAD")?;
    let create_branch = targets_base(&current, &config.base_branch);
    let branch = if create_branch {
        options
            .branch
            .clone()
            .or_else(|| old.map(|o| o.branch.clone()))
            .unwrap_or_else(|| branch_from_message(&commit_message))
    } else {
        if options.branch.as_ref().is_some_and(|b| *b != current) {
            bail!(
                "--branch only applies when starting on {}; the checkout is on {current}",
                config.base_branch
            );
        }
        current.clone()
    };

    let changed = changed_files(&config)?;
    for path in options.exclude.iter().filter(|p| !changed.contains(*p)) {
        problems.push(Problem::warning(
            "exclude_unused",
            format!("--exclude {path} matches no changed file"),
            "check the spelling against git status",
        ));
    }
    let files = if resume {
        Vec::new()
    } else {
        changed
            .iter()
            .filter(|f| !options.exclude.contains(*f))
            .cloned()
            .collect()
    };
    let id = match &options.id {
        Some(id) => id.clone(),
        None => fresh_id(&queue, &branch),
    };
    let request = Request {
        id,
        branch,
        create_branch,
        expected_head: head,
        resume,
        files,
        commit_message,
        pr_title,
        summary,
        verification,
        traceability,
    };
    problems.extend(request_problems(&request, &config.policy()));
    problems.extend(state_problems(
        &config,
        config_path,
        &request,
        &changed,
        false,
    )?);
    if options.dry_run {
        println!("{}", serde_json::to_string_pretty(&request)?);
        report(&problems)?;
        println!("dry run: nothing was queued");
        return Ok(());
    }
    report(&problems)?;
    enqueue(&queue, &request)?;
    let what = if request.resume {
        "resume of the existing commit".to_string()
    } else {
        format!(
            "{} file(s): {}",
            request.files.len(),
            request.files.join(", ")
        )
    };
    let start = if request.create_branch {
        "new branch"
    } else {
        "branch"
    };
    println!(
        "queued {} on {start} {}: {what}",
        request.id, request.branch
    );
    println!("next: {}", status_command(&queue, &request.id)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_come_from_the_message() {
        assert_eq!(
            branch_from_message("fix: handle empty input"),
            "fix/handle-empty-input"
        );
        assert_eq!(
            branch_from_message("feat(cli)!: Add publish"),
            "feature/add-publish"
        );
        assert_eq!(branch_from_message("docs: ..."), "docs/change");
        let long =
            branch_from_message("fix: a very long subject line that goes on and on past forty");
        assert!(
            long.len() <= "fix/".len() + 40 && !long.ends_with('-'),
            "{long}"
        );
    }

    #[test]
    fn evidence_splits_at_the_last_equals_sign() {
        let row = parse_evidence("cargo test --features=x = 212 passed").unwrap();
        assert_eq!(
            (row.check.as_str(), row.result.as_str()),
            ("cargo test --features=x", "212 passed")
        );
        assert!(parse_evidence("cargo test").is_err());
        assert!(parse_evidence("cargo test=").is_err());
    }

    #[test]
    fn status_lists_new_changed_deleted_and_both_sides_of_a_rename() {
        let text = " M src/a.rs\0?? new file.txt\0 D gone.rs\0R  b.rs\0old b.rs\0";
        let files: Vec<_> = parse_status(text).into_iter().collect();
        assert_eq!(
            files,
            ["b.rs", "gone.rs", "new file.txt", "old b.rs", "src/a.rs"]
        );
    }

    #[test]
    fn commands_paste_into_every_shell_when_plain_and_powershell_otherwise() {
        assert_eq!(
            program("C:/apr/agent-pr-runner.exe"),
            "C:/apr/agent-pr-runner.exe"
        );
        assert_eq!(program("C:/My Tools/apr.exe"), "& 'C:/My Tools/apr.exe'");
        assert_eq!(shell_arg("it's"), "'it''s'");
        assert_eq!(shell_arg("fix-1"), "fix-1");
    }

    #[test]
    fn every_request_problem_is_listed_with_a_fix() {
        let mut value = crate::tests::request();
        value.id = "bad id".into();
        value.files = vec![".gitignore".into(), ".github/workflows/ci.yml".into()];
        value.commit_message = "no type".into();
        let problems = request_problems(&value, &Policy::default());
        let codes: Vec<_> = problems.iter().map(|p| p.code).collect();
        assert_eq!(
            codes,
            [
                "bad_id",
                "operator_only",
                "operator_only",
                "bad_commit_message",
                "not_semantic"
            ]
        );
        assert!(problems.iter().all(|p| p.blocking && !p.fix.is_empty()));
        assert!(problems[1].line().contains("--exclude .gitignore"));
    }

    #[test]
    fn receipts_name_an_action_and_old_receipts_still_parse() {
        let old = r#"{"id":"a","status":"error","detail":"x","pr_url":null,"diagnostic_log":null}"#;
        let receipt: Receipt = serde_json::from_str(old).unwrap();
        assert!(receipt.action.is_none());
        assert_eq!(legacy_action(&receipt.status), UNCLASSIFIED);
        assert_eq!(legacy_action("merged"), DONE);
        assert_eq!(legacy_action("needs_inspection"), REPORT);
    }

    #[test]
    fn a_fresh_id_skips_every_used_one() {
        let dir = std::env::temp_dir().join(format!("agent-pr-runner-ids-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(fresh_id(&dir, "fix/a b"), "fix-a-b-1");
        fs::write(queue_path(&dir, "fix-a-b-1", "sent"), "{}").unwrap();
        fs::write(queue_path(&dir, "fix-a-b-2", "result"), "{}").unwrap();
        assert_eq!(fresh_id(&dir, "fix/a b"), "fix-a-b-3");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprints_notice_any_change() {
        assert_eq!(fingerprint(b"{}"), fingerprint(b"{}"));
        assert_ne!(fingerprint(b"{\"a\":1}"), fingerprint(b"{\"a\":2}"));
    }
}
