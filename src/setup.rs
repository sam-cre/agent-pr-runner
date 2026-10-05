//! Setup without hand editing. `init` writes a repository's config from what Git, GitHub, and the
//! repository itself already say, and `snippet` prints the agent instructions with this install's
//! paths filled in.

use super::*;

/// Local check presets `init` can paste in, keyed by project kind.
const CHECK_PRESETS: &[(&str, &str)] = &[
    ("rust", include_str!("../examples/checks/rust.json")),
    ("node", include_str!("../examples/checks/node.json")),
    ("python", include_str!("../examples/checks/python.json")),
];

/// Files that change what "passing" means for each project kind, for `--protect-manifests`.
const MANIFESTS: &[(&str, &[&str])] = &[
    (
        "rust",
        &["Cargo.toml", "build.rs", ".cargo/", "rust-toolchain.toml"],
    ),
    ("node", &["package.json", ".npmrc"]),
    (
        "python",
        &["pyproject.toml", "setup.cfg", "tox.ini", "noxfile.py"],
    ),
];

const SNIPPET: &str = include_str!("../agent/AGENTS-snippet.md");

#[derive(Debug, Default)]
struct InitOptions {
    repo: Option<PathBuf>,
    name: Option<String>,
    workflow: Option<String>,
    jobs: Vec<String>,
    local_checks: String,
    protect_manifests: bool,
    force: bool,
}

fn parse_init(args: &[String]) -> Result<InitOptions> {
    let mut options = InitOptions {
        local_checks: "off".into(),
        ..InitOptions::default()
    };
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--repo" => options.repo = Some(PathBuf::from(value()?)),
            "--name" => options.name = Some(value()?),
            "--workflow" => options.workflow = Some(value()?),
            "--job" => options.jobs.push(value()?),
            "--local-checks" => options.local_checks = value()?,
            "--protect-manifests" => options.protect_manifests = true,
            "--force" => options.force = true,
            other => bail!("unknown init option {other}"),
        }
    }
    if !["off", "auto", "rust", "node", "python"].contains(&options.local_checks.as_str()) {
        bail!("--local-checks must be off, auto, rust, node, or python");
    }
    if options.repo.is_none() {
        bail!("init needs --repo PATH (the repository's working copy)");
    }
    Ok(options)
}

/// The kind of project at the repository root, used to pick check presets and manifests.
fn project_kind(repo: &Path) -> Option<&'static str> {
    [
        ("Cargo.toml", "rust"),
        ("package.json", "node"),
        ("pyproject.toml", "python"),
    ]
    .iter()
    .find(|(file, _)| repo.join(file).is_file())
    .map(|(_, kind)| *kind)
}

/// A path as people write it: no `\\?\` prefix, and forward slashes on Windows.
fn plain(path: &Path) -> String {
    let text = path.display().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_string()
    }
}

/// `owner/name` from a github.com remote URL.
fn repository_from_remote(url: &str) -> Option<String> {
    let rest = [
        "https://github.com/",
        "git@github.com:",
        "ssh://git@github.com/",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

/// A config name made of safe characters, from `--name` or the repository folder.
fn config_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.trim_matches(['-', '.']).to_string()
}

// ---------------------------------------------------------------------------------------------
// Reading a workflow file
//
// A small reader for the parts of GitHub Actions YAML that decide job names: the top-level
// `name:`, the `jobs:` map, each job's `name:`, and a simple `strategy.matrix`. Anything it can not
// predict (matrix `include`, expressions other than `matrix.x`, reusable workflows) is reported
// instead of guessed.
// ---------------------------------------------------------------------------------------------

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn meaningful(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

fn scalar(raw: &str) -> String {
    let raw = raw.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = raw.strip_prefix(quote) {
            if let Some(end) = inner.find(quote) {
                return inner[..end].to_string();
            }
        }
    }
    raw.split(" #").next().unwrap_or("").trim().to_string()
}

/// The lines indented under `lines[start]`.
fn block<'a, 'b>(lines: &'b [&'a str], start: usize) -> &'b [&'a str] {
    let base = indent(lines[start]);
    let end = lines[start + 1..]
        .iter()
        .position(|line| meaningful(line) && indent(line) <= base)
        .map_or(lines.len(), |offset| start + 1 + offset);
    &lines[start + 1..end]
}

/// The direct `key: value` children of a block, with their positions in it.
fn children<'a>(body: &[&'a str]) -> Vec<(usize, String, &'a str)> {
    let Some(first) = body.iter().find(|line| meaningful(line)) else {
        return Vec::new();
    };
    let level = indent(first);
    body.iter()
        .enumerate()
        .filter(|(_, line)| meaningful(line) && indent(line) == level)
        .filter_map(|(index, line)| {
            let (key, value) = line.trim().split_once(':')?;
            (!key.starts_with('-')).then(|| (index, scalar(key), value))
        })
        .collect()
}

/// A list written inline (`[a, b]`) or as `- item` lines under its key.
fn list_value(body: &[&str], index: usize, value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return Some(
            inner
                .split(',')
                .map(scalar)
                .filter(|item| !item.is_empty())
                .collect(),
        );
    }
    if !value.is_empty() {
        return None;
    }
    let items: Vec<String> = block(body, index)
        .iter()
        .filter(|line| meaningful(line))
        .map(|line| line.trim().strip_prefix("- ").map(scalar))
        .collect::<Option<_>>()?;
    (!items.is_empty()).then_some(items)
}

/// The workflow's display name, which is its file path when it has no top-level `name:`.
fn workflow_display_name(text: &str, file: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("name:"))
        .map(scalar)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| file.to_string())
}

fn triggers_on_pull_request(text: &str) -> bool {
    Regex::new(r"(?m)\bpull_request([^_\w]|$)")
        .expect("valid pattern")
        .is_match(text)
}

/// Every combination of the matrix values, in the matrix's key order.
fn combinations(vars: &[(String, Vec<String>)]) -> Vec<Vec<(String, String)>> {
    let mut result = vec![Vec::new()];
    for (key, values) in vars {
        result = result
            .into_iter()
            .flat_map(|combo: Vec<(String, String)>| {
                values.iter().map(move |value| {
                    let mut next = combo.clone();
                    next.push((key.clone(), value.clone()));
                    next
                })
            })
            .collect();
    }
    result
}

/// The job names GitHub will show for this workflow, or why they can not be known in advance.
fn job_names(text: &str) -> std::result::Result<Vec<String>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_end() == "jobs:")
        .ok_or("the workflow has no top-level jobs: section")?;
    let jobs = block(&lines, start);
    let template = Regex::new(r"\$\{\{\s*matrix\.([A-Za-z0-9_-]+)\s*\}\}").expect("valid pattern");
    let mut names = BTreeSet::new();
    for (index, id, _) in children(jobs) {
        let body = block(jobs, index);
        let props = children(body);
        let prop = |key: &str| props.iter().find(|(_, k, _)| k == key);
        if prop("uses").is_some() {
            return Err(format!(
                "job {id} calls a reusable workflow, whose job names GitHub builds at run time"
            ));
        }
        let name = prop("name")
            .map(|(_, _, value)| scalar(value))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.clone());
        let mut vars = Vec::new();
        if let Some((strategy, _, _)) = prop("strategy") {
            let strategy_body = block(body, *strategy);
            if let Some((matrix, _, value)) = children(strategy_body)
                .into_iter()
                .find(|(_, key, _)| key == "matrix")
            {
                if !value.trim().is_empty() {
                    return Err(format!("job {id} builds its matrix at run time"));
                }
                let matrix_body = block(strategy_body, matrix);
                for (entry, key, value) in children(matrix_body) {
                    if key == "include" || key == "exclude" {
                        return Err(format!("job {id} uses matrix {key}"));
                    }
                    let values = list_value(matrix_body, entry, value)
                        .ok_or_else(|| format!("job {id} has a matrix value that is not a list"))?;
                    vars.push((key, values));
                }
            }
        }
        let templated = name.contains("${{");
        for combo in combinations(&vars) {
            let shown = if templated {
                let filled = template.replace_all(&name, |caps: &regex::Captures| {
                    combo
                        .iter()
                        .find(|(key, _)| *key == caps[1])
                        .map_or_else(|| caps[0].to_string(), |(_, value)| value.clone())
                });
                if filled.contains("${{") {
                    return Err(format!(
                        "job {id} has a name GitHub only fills in at run time"
                    ));
                }
                filled.into_owned()
            } else if combo.is_empty() {
                name.clone()
            } else {
                let values: Vec<&str> = combo.iter().map(|(_, value)| value.as_str()).collect();
                format!("{name} ({})", values.join(", "))
            };
            names.insert(shown);
        }
    }
    if names.is_empty() {
        return Err("the workflow's jobs: section lists no jobs".into());
    }
    Ok(names.into_iter().collect())
}

// ---------------------------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------------------------

/// A config holding only what the read-only probes need, before the rest is known.
fn probe_config(repo: &Path, queue: &Path, git_exe: PathBuf, gh_exe: PathBuf) -> Config {
    Config {
        repo: repo.to_path_buf(),
        queue_dir: queue.to_path_buf(),
        git_exe,
        gh_exe,
        repository: String::new(),
        author_name: String::new(),
        author_email: String::new(),
        github_login: String::new(),
        base_branch: default_base_branch(),
        remote: default_remote(),
        ci: CiConfig {
            workflow_name: default_workflow_name(),
            workflow_file: default_workflow_file(),
            required_jobs: Vec::new(),
            timeout_minutes: default_ci_timeout_minutes(),
        },
        local_checks: LocalChecksConfig::default(),
        protected_paths: Vec::new(),
        blocked_words: Vec::new(),
        cache_limit_gb: default_cache_limit_gb(),
    }
}

/// Workflow files under `.github/workflows` that run on pull requests, as (path, text).
fn pull_request_workflows(repo: &Path) -> Vec<(String, String)> {
    let Ok(read) = fs::read_dir(repo.join(".github/workflows")) else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> = read
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !(file.ends_with(".yml") || file.ends_with(".yaml")) {
                return None;
            }
            let text = fs::read_to_string(entry.path()).ok()?;
            triggers_on_pull_request(&text).then(|| (format!(".github/workflows/{file}"), text))
        })
        .collect();
    found.sort();
    found
}

/// Job names from the newest GitHub run of the workflow, when the file alone does not say.
fn jobs_from_latest_run(probe: &Config, repository: &str, workflow_file: &str) -> Vec<String> {
    let file = workflow_file.rsplit('/').next().unwrap_or(workflow_file);
    let Ok(id) = gh(
        probe,
        &[
            "run",
            "list",
            "--repo",
            repository,
            "--workflow",
            file,
            "--limit",
            "1",
            "--json",
            "databaseId",
            "--jq",
            ".[0].databaseId",
        ],
    )
    .and_then(|out| checked(out, "listing runs")) else {
        return Vec::new();
    };
    if id.is_empty() || id == "null" {
        return Vec::new();
    }
    gh(
        probe,
        &[
            "run",
            "view",
            &id,
            "--repo",
            repository,
            "--json",
            "jobs",
            "--jq",
            ".jobs[].name",
        ],
    )
    .and_then(|out| checked(out, "reading run jobs"))
    .map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    })
    .unwrap_or_default()
}

pub(crate) fn init(args: &[String]) -> Result<()> {
    let options = parse_init(args)?;
    let given = options.repo.clone().unwrap_or_default();
    let repo = fs::canonicalize(&given)
        .with_context(|| format!("repository folder {} does not exist", given.display()))?;
    let git_exe = resolve_program("git")
        .context("git is not on PATH; install it (see SETUP-FOR-AGENTS.md) and try again")?;
    let gh_exe = resolve_program("gh").context(
        "gh is not on PATH; install the GitHub CLI (see SETUP-FOR-AGENTS.md) and try again",
    )?;
    let install = install_dir()?;
    if install.starts_with(&repo) {
        bail!("the runner is installed inside the repository; install it elsewhere first");
    }
    installed_hooks_dir()?;
    let name = config_name(
        options
            .name
            .as_deref()
            .or_else(|| repo.file_name().and_then(|n| n.to_str()))
            .unwrap_or("project"),
    );
    if name.is_empty() {
        bail!("pass --name with letters or digits");
    }
    let config_path = install.join("configs").join(format!("{name}.json"));
    if config_path.exists() && !options.force {
        bail!(
            "{} already exists; pass --force to replace it, or --name for a second config",
            plain(&config_path)
        );
    }
    let queue = install.join("queues").join(&name);
    let probe = probe_config(&repo, &queue, git_exe.clone(), gh_exe.clone());

    let top = checked(
        git(&probe, &["rev-parse", "--show-toplevel"], false)?,
        "checking the repository",
    )?;
    if fs::canonicalize(&top)? != repo {
        bail!("pass the repository root as --repo: {top}");
    }
    let remote_url = checked(
        git(&probe, &["remote", "get-url", &probe.remote], false)?,
        "reading the origin remote",
    )?;
    let repository = repository_from_remote(&remote_url)
        .filter(|r| remote_matches(&remote_url, r))
        .with_context(|| format!("origin is not a github.com repository URL: {remote_url}"))?;
    let user = gh(
        &probe,
        &[
            "api",
            "user",
            "--jq",
            "[.login, (.id|tostring)] | join(\" \")",
        ],
    )?;
    if !user.success {
        bail!(
            "gh is not logged in. The user runs `gh auth login` themselves (it opens a browser), \
             then init runs again"
        );
    }
    let user = user.stdout.trim().to_string();
    let (login, id) = user
        .split_once(' ')
        .context("could not read the GitHub login")?;
    let author_email = format!("{id}+{login}@users.noreply.github.com");
    let base_branch = gh(
        &probe,
        &[
            "repo",
            "view",
            &repository,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
    )
    .and_then(|out| checked(out, "reading the default branch"))
    .ok()
    .filter(|b| !b.is_empty())
    .unwrap_or_else(default_base_branch);

    let mut problems: Vec<String> = Vec::new();
    let workflows = pull_request_workflows(&repo);
    let workflow = match &options.workflow {
        Some(file) => match fs::read_to_string(repo.join(file)) {
            Ok(text) => Some((file.clone(), text)),
            Err(_) => {
                problems.push(format!(
                    "--workflow {file} does not exist in the repository"
                ));
                None
            }
        },
        None if workflows.len() == 1 => workflows.first().cloned(),
        None => {
            let preferred = workflows
                .iter()
                .find(|(file, _)| file.ends_with("/ci.yml") || file.ends_with("/ci.yaml"));
            if preferred.is_none() {
                if workflows.is_empty() {
                    problems.push(
                        "no workflow in .github/workflows runs on pull_request. Copy one from \
                         examples/workflows/ to .github/workflows/ci.yml; the user commits and \
                         pushes it, then init runs again"
                            .into(),
                    );
                } else {
                    let files: Vec<&str> = workflows.iter().map(|(f, _)| f.as_str()).collect();
                    problems.push(format!(
                        "several workflows run on pull_request ({}); pass --workflow FILE",
                        files.join(", ")
                    ));
                }
            }
            preferred.cloned()
        }
    };
    let (workflow_file, workflow_name, jobs, jobs_source) = match &workflow {
        Some((file, text)) => {
            let (jobs, source) = if !options.jobs.is_empty() {
                (options.jobs.clone(), "--job")
            } else {
                match job_names(text) {
                    Ok(jobs) => (jobs, "workflow file"),
                    Err(reason) => {
                        let jobs = jobs_from_latest_run(&probe, &repository, file);
                        if jobs.is_empty() {
                            problems.push(format!(
                                "could not tell the CI job names ({reason}) and the workflow has \
                                 no run yet; pass each name GitHub shows with --job"
                            ));
                        }
                        (jobs, "latest GitHub run")
                    }
                }
            };
            (
                file.clone(),
                workflow_display_name(text, file),
                jobs,
                source,
            )
        }
        None => (String::new(), String::new(), Vec::new(), ""),
    };

    let kind = project_kind(&repo);
    let preset = match options.local_checks.as_str() {
        "off" => None,
        "auto" => {
            if kind.is_none() {
                problems.push(
                    "--local-checks auto found no Cargo.toml, package.json, or pyproject.toml; \
                     choose rust, node, python, or off"
                        .into(),
                );
            }
            kind
        }
        chosen => Some(chosen),
    };
    let local_checks = match preset {
        Some(preset) => {
            let text = CHECK_PRESETS
                .iter()
                .find(|(k, _)| *k == preset)
                .map(|(_, text)| *text)
                .context("unknown local check preset")?;
            serde_json::from_str::<serde_json::Value>(text)?
        }
        None => serde_json::json!({ "enabled": false }),
    };
    let protected_paths: Vec<&str> = if options.protect_manifests {
        match kind {
            Some(kind) => MANIFESTS
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, files)| files.to_vec())
                .unwrap_or_default(),
            None => {
                problems.push(
                    "--protect-manifests found no Cargo.toml, package.json, or pyproject.toml"
                        .into(),
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let detected = serde_json::json!({
        "repo": plain(&repo),
        "repository": repository,
        "login": login,
        "author_email": author_email,
        "base_branch": base_branch,
        "workflow_file": workflow_file,
        "workflow_name": workflow_name,
        "required_jobs": jobs,
        "jobs_source": jobs_source,
        "project_kind": kind,
        "local_checks": preset.unwrap_or("off"),
        "protected_paths": protected_paths,
    });
    if !problems.is_empty() {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "status": "needs_input", "problems": problems, "detected": detected })
            )?
        );
        bail!("setup needs input; nothing was written");
    }

    let value = serde_json::json!({
        "repo": plain(&repo),
        "queue_dir": plain(&queue),
        "git_exe": plain(&git_exe),
        "gh_exe": plain(&gh_exe),
        "repository": repository,
        "author_name": login,
        "author_email": author_email,
        "github_login": login,
        "base_branch": base_branch,
        "remote": probe.remote,
        "ci": {
            "workflow_name": workflow_name,
            "workflow_file": workflow_file,
            "required_jobs": jobs,
            "timeout_minutes": default_ci_timeout_minutes(),
        },
        "local_checks": local_checks,
        "protected_paths": protected_paths,
        "blocked_words": [],
        "cache_limit_gb": default_cache_limit_gb(),
    });
    fs::create_dir_all(&queue)?;
    let config: Config = serde_json::from_value(value.clone())?;
    validate_config(&config)?;
    fs::create_dir_all(install.join("configs"))?;
    fs::write(
        &config_path,
        format!("{}\n", serde_json::to_string_pretty(&value)?),
    )?;
    let doctor = doctor_report(&config_path)?;
    let exe = plain(&fs::canonicalize(std::env::current_exe()?)?);
    let config_text = plain(&config_path);
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "ready",
            "config": config_text,
            "queue": plain(&queue),
            "detected": detected,
            "doctor": doctor,
            "serve": format!("\"{exe}\" serve \"{config_text}\""),
            "snippet": format!("\"{exe}\" snippet \"{config_text}\""),
        }))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// snippet
// ---------------------------------------------------------------------------------------------

/// The agent instructions section with this install's runner and queue paths filled in.
fn fill_snippet(exe: &str, queue: &str) -> String {
    let text = SNIPPET.replace("\r\n", "\n");
    let body = text.split_once("\n---\n").map_or(text.as_str(), |(_, b)| b);
    body.trim_start()
        .replace(
            "`<INSTALL_DIR>/agent-pr-runner` (`.exe` on Windows)",
            &format!("`{exe}`"),
        )
        .replace("`<INSTALL_DIR>/queues/<project>`", &format!("`{queue}`"))
        .replace(
            "<INSTALL_DIR>/agent-pr-runner submit <QUEUE_DIR>",
            &format!("\"{exe}\" submit \"{queue}\""),
        )
        .replace(
            "<INSTALL_DIR>/agent-pr-runner status <QUEUE_DIR>",
            &format!("\"{exe}\" status \"{queue}\""),
        )
}

pub(crate) fn snippet(config_path: &Path) -> Result<()> {
    let config = load_installed_config(config_path)?;
    let exe = plain(&fs::canonicalize(std::env::current_exe()?)?);
    let queue = plain(&fs::canonicalize(&config.queue_dir)?);
    print!("{}", fill_snippet(&exe, &queue));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_names_follow_github_naming() {
        let rust = include_str!("../examples/workflows/ci-rust.yml");
        assert_eq!(
            job_names(rust).unwrap(),
            [
                "lint + test (ubuntu-latest)",
                "lint + test (windows-latest)"
            ]
        );
        let node = include_str!("../examples/workflows/ci-node.yml");
        assert_eq!(job_names(node).unwrap(), ["test"]);
        let plain_matrix = "on: pull_request\njobs:\n  build:\n    strategy:\n      matrix:\n        os:\n          - ubuntu-latest\n          - macos-latest\n        node: [20, \"22\"]\n    runs-on: x\n  lint:\n    runs-on: x # no name\n";
        assert_eq!(
            job_names(plain_matrix).unwrap(),
            [
                "build (macos-latest, 20)",
                "build (macos-latest, 22)",
                "build (ubuntu-latest, 20)",
                "build (ubuntu-latest, 22)",
                "lint"
            ]
        );
    }

    #[test]
    fn job_names_refuse_what_only_github_knows() {
        let include = "jobs:\n  t:\n    strategy:\n      matrix:\n        os: [a]\n        include:\n          - os: b\n";
        assert!(job_names(include).unwrap_err().contains("include"));
        let dynamic =
            "jobs:\n  t:\n    strategy:\n      matrix: ${{ fromJSON(needs.x.outputs.m) }}\n";
        assert!(job_names(dynamic).is_err());
        let reusable = "jobs:\n  t:\n    uses: ./.github/workflows/other.yml\n";
        assert!(job_names(reusable).is_err());
        let expression = "jobs:\n  t:\n    name: test ${{ github.ref }}\n";
        assert!(job_names(expression).is_err());
        assert!(job_names("on: push\n").is_err());
    }

    #[test]
    fn workflow_names_and_triggers() {
        assert_eq!(
            workflow_display_name("name: \"CI\" # main\non: x\n", "f"),
            "CI"
        );
        assert_eq!(
            workflow_display_name("on: x\n", ".github/workflows/a.yml"),
            ".github/workflows/a.yml"
        );
        assert!(triggers_on_pull_request("on:\n  pull_request:\n"));
        assert!(triggers_on_pull_request("on: [push, pull_request]\n"));
        assert!(!triggers_on_pull_request("on:\n  pull_request_target:\n"));
    }

    #[test]
    fn remotes_and_names() {
        for url in [
            "https://github.com/o/n.git",
            "https://github.com/o/n",
            "git@github.com:o/n.git",
            "ssh://git@github.com/o/n",
        ] {
            assert_eq!(repository_from_remote(url).as_deref(), Some("o/n"), "{url}");
        }
        assert!(repository_from_remote("https://gitlab.com/o/n").is_none());
        assert!(repository_from_remote("https://github.com/o/n/extra").is_none());
        assert_eq!(config_name("My Project!"), "My-Project");
        assert_eq!(config_name("..."), "");
    }

    #[test]
    fn the_snippet_gets_real_paths() {
        let text = fill_snippet("C:/apr/agent-pr-runner.exe", "C:/apr/queues/app");
        assert!(
            !text.contains("<INSTALL_DIR>") && !text.contains("<QUEUE_DIR>"),
            "{text}"
        );
        assert!(text.starts_with("## Publishing changes"));
        assert!(text.contains("\"C:/apr/agent-pr-runner.exe\" submit \"C:/apr/queues/app\""));
    }

    #[test]
    fn presets_parse() {
        for (kind, text) in CHECK_PRESETS {
            serde_json::from_str::<LocalChecksConfig>(text).expect(kind);
        }
    }
}
