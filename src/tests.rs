use super::*;

fn request() -> Request {
    Request {
        id: "case-1".into(),
        branch: "fix/example".into(),
        create_branch: false,
        expected_head: "a".repeat(40),
        resume: false,
        files: vec!["src/lib.rs".into()],
        commit_message: "fix: example".into(),
        pr_title: "fix: example".into(),
        summary: vec!["Fix example".into()],
        verification: vec![Evidence {
            check: "cargo test".into(),
            result: "passed".into(),
        }],
        traceability: vec!["Plan item 1".into()],
    }
}

fn ok(value: &Request) -> bool {
    validate_request(value, &Policy::default()).is_ok()
}

fn ci(required: &[&str]) -> CiConfig {
    CiConfig {
        workflow_name: "CI".into(),
        workflow_file: ".github/workflows/ci.yml".into(),
        required_jobs: required.iter().map(|s| s.to_string()).collect(),
        timeout_minutes: 30,
    }
}

const UBUNTU: &str = "lint + test (ubuntu-latest)";
const WINDOWS: &str = "lint + test (windows-latest)";

#[test]
fn workflow_requires_semantic_titles_evidence_and_traceability() {
    let mut value = request();
    value.pr_title = "Example change".into();
    assert!(!ok(&value));
    value = request();
    value.commit_message = "update: example".into();
    assert!(!ok(&value));
    value = request();
    value.commit_message = "feat(ui)!: example".into();
    assert!(ok(&value));
    value = request();
    value.traceability.clear();
    assert!(!ok(&value));
    value = request();
    value.verification.clear();
    assert!(!ok(&value));
}

#[test]
fn ai_product_names_are_refused_in_published_text() {
    let mut value = request();
    value.branch = "codex/example".into();
    assert!(!ok(&value));
    value = request();
    value.pr_title = "fix: tune Claude prompt".into();
    assert!(!ok(&value));
    value = request();
    value.traceability = vec!["Requested in ChatGPT".into()];
    assert!(!ok(&value));
}

#[test]
fn configured_words_are_refused_too() {
    let policy = Policy {
        protected: Vec::new(),
        blocked: vec!["internal-codename".into()],
    };
    let mut value = request();
    assert!(validate_request(&value, &policy).is_ok());
    value.summary = vec!["Ships Internal-Codename work".into()];
    assert!(validate_request(&value, &policy).is_err());
}

#[test]
fn attribution_trailers_are_refused_but_ordinary_wording_is_not() {
    assert!(has_attribution(
        "fix: x\n\nCo-authored-by: Bot <bot@example.com>"
    ));
    assert!(has_attribution("Signed-off-by: someone"));
    assert!(has_attribution("Generated with Claude Code"));
    assert!(!has_attribution("Tables generated with the export script"));
}

#[test]
fn pr_body_has_evidence_table_and_traceability() {
    let mut value = request();
    value.verification[0].result = "passed | 12 tests".into();
    let body = pr_body(&value);
    assert!(body.starts_with("## Summary\n- Fix example\n"));
    assert!(body.contains("## Verification Evidence\n\n| Check | Result |\n| --- | --- |\n"));
    assert!(body.contains("| cargo test | passed \\| 12 tests |\n"));
    assert!(body.contains("## Traceability\n- Plan item 1\n"));
}

#[test]
fn request_is_data_not_shell() {
    let mut value = request();
    assert!(ok(&value));
    value.files = vec!["../AGENTS.md".into()];
    assert!(!ok(&value));
    value = request();
    value.commit_message = "fix: example\n\nCo-authored-by: Bot <bot@example.com>".into();
    assert!(!ok(&value));
    value = request();
    value.summary = vec!["Generated with Claude".into()];
    assert!(!ok(&value));
}

#[test]
fn resume_finishes_an_existing_commit_and_stages_nothing() {
    let mut value = request();
    value.resume = true;
    value.files.clear();
    assert!(ok(&value));
    value.files = vec!["src/lib.rs".into()];
    assert!(!ok(&value), "resume must not stage files");
    value.files.clear();
    value.create_branch = true;
    assert!(!ok(&value), "resume never creates a branch");
    value.create_branch = false;
    value.expected_head.clear();
    assert!(!ok(&value), "resume pins the head");

    let mut normal = request();
    normal.files.clear();
    assert!(!ok(&normal), "only resume may omit files");
}

#[test]
fn pr_heads_must_all_match_the_pushed_commit() {
    let pr = |head: &str| ExistingPr {
        number: 1,
        url: String::new(),
        head_ref_oid: head.into(),
        author: PrAuthor { login: "op".into() },
        is_cross_repository: false,
    };
    let head = "b".repeat(40);
    assert!(heads_settled(&[], &head), "no PR yet is settled");
    assert!(heads_settled(&[pr(&head)], &head));
    assert!(!heads_settled(&[pr(&"a".repeat(40))], &head));
}

#[test]
fn any_safe_branch_name_is_accepted() {
    let mut value = request();
    for branch in ["docs/example", "fix/parser-1", "feature/x", "plain-name"] {
        value.branch = branch.into();
        assert!(ok(&value), "{branch}");
    }
    for branch in ["", "-D", "--force", "has space", "a..b", "tab\tname"] {
        value.branch = branch.into();
        assert!(!ok(&value), "{branch:?}");
    }
    assert!(targets_base("main", "main"));
    assert!(targets_base("Main", "main"));
    assert!(!targets_base("fix/main", "main"));
}

#[test]
fn coderabbit_summary_is_not_tampering_but_other_edits_are() {
    let ours = pr_body(&request());
    let block =
        format!("\n\n{CODERABBIT_START}\n\n## Summary by CodeRabbit\n* x\n\n{CODERABBIT_END}\n");
    let same = |body: &str| without_review_bot_summary(body).trim_end() == ours.trim_end();
    assert!(same(&ours));
    assert!(same(&format!("{ours}{block}")));
    assert!(!same(&format!("{ours}{block}extra text after the block")));
    assert!(!same(&format!(
        "{ours}\n\n{CODERABBIT_START}\nno end marker"
    )));
    assert!(!same(&format!("injected\n{ours}{block}")));
}

fn log(step: &str, lines: &[&str]) -> String {
    lines
        .iter()
        .map(|text| format!("{UBUNTU}\t{step}\t2026-09-22T22:21:53.0334885Z {text}\n"))
        .collect()
}

#[test]
fn real_clippy_failure_is_lint_not_infra() {
    let text = log(
        "Clippy (deny warnings)",
        &[
            "^[[1m^[[91merror^[[0m^[[1m: call to `std::mem::drop` with a value that does not implement `Drop`^[[0m",
            "    ^[[1m^[[94m= ^[[0m^[[1mhelp^[[0m: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.98.0/index.html#drop_non_drop",
            "^[[1m^[[91merror^[[0m: could not compile `example` (lib) due to 1 previous error",
            "##[error]Process completed with exit code 101.",
        ],
    );
    assert_eq!(classify_failure(&text), FailureKind::Lint);
}

#[test]
fn genuine_compile_error_beats_echoed_infra_text() {
    let text = log(
        "UNKNOWN STEP",
        &[
            "^[[36;1m  printf '::error::install-action: %s\\n' \"$*\"^[[0m",
            "error[E0308]: mismatched types",
            "^[[1m^[[91merror^[[0m: could not compile `example` (lib) due to 1 previous error",
        ],
    );
    assert_eq!(classify_failure(&text), FailureKind::Build);
}

#[test]
fn a_typescript_error_is_a_build_failure() {
    let text = log(
        "Build",
        &["src/App.tsx(12,5): error TS2322: Type 'string' is not assignable to type 'number'."],
    );
    assert_eq!(classify_failure(&text), FailureKind::Build);
}

#[test]
fn failing_tests_are_named_from_nextest_libtest_and_pytest() {
    let text = log(
        "Test (nextest)",
        &[
            "        FAIL [   0.337s] (261/899) example engine::tests::detects_cycle",
            "        FAIL [   0.337s] (261/899) example engine::tests::detects_cycle",
            "test ui::tests::renders_footer ... FAILED",
            "error: test run failed",
        ],
    );
    assert_eq!(
        classify_failure(&text),
        FailureKind::Test(vec![
            "engine::tests::detects_cycle".into(),
            "ui::tests::renders_footer".into(),
        ])
    );
    let py = "FAILED tests/test_api.py::test_login - AssertionError\n";
    assert_eq!(
        classify_failure(py),
        FailureKind::Test(vec!["tests/test_api.py::test_login".into()])
    );
}

#[test]
fn network_and_setup_failures_are_transient() {
    let network = log(
        "Test (nextest)",
        &[
            "error: failed to download `regex v1.11.0`",
            "  503 Service Unavailable",
        ],
    );
    assert_eq!(classify_failure(&network), FailureKind::InfraTransient);
    let setup = log(
        "Install cargo-nextest",
        &["##[error]install-action: installation failed due to bash startup failure"],
    );
    assert_eq!(classify_failure(&setup), FailureKind::InfraTransient);
    let unknown = log("Post job cleanup", &["something odd happened"]);
    assert_eq!(classify_failure(&unknown), FailureKind::Unknown);
}

#[test]
fn failure_description_names_the_kind_and_keeps_the_excerpt() {
    let kind = FailureKind::Test(vec!["a::b".into()]);
    assert_eq!(kind.label(), "test_failure");
    assert_eq!(describe_failure(&kind, "x"), "test failure: a::b; x");
    let receipt = Receipt {
        id: "r".into(),
        status: "merged".into(),
        detail: String::new(),
        pr_url: None,
        diagnostic_log: None,
        failure_kind: None,
    };
    assert!(!serde_json::to_string(&receipt)
        .unwrap()
        .contains("failure_kind"));
}

#[test]
fn only_one_serve_process_may_hold_the_queue() {
    let dir = std::env::temp_dir().join(format!("agent-pr-runner-lock-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let first = serve_lock(&dir).unwrap();
    assert!(serve_lock(&dir).is_err());
    drop(first);
    assert!(serve_lock(&dir).is_ok());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn branch_creation_is_opt_in_and_existing_requests_still_parse() {
    let old = serde_json::to_value(request()).unwrap();
    let old = old
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, _)| *key != "create_branch")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let parsed: Request = serde_json::from_value(serde_json::Value::Object(old)).unwrap();
    assert!(!parsed.create_branch);
    assert!(ok(&parsed));

    let mut new_request = request();
    new_request.create_branch = true;
    new_request.expected_head.clear();
    assert!(ok(&new_request));
    new_request.create_branch = false;
    assert!(!ok(&new_request));
    new_request.create_branch = true;
    new_request.expected_head = "wrong".into();
    assert!(!ok(&new_request));
}

#[test]
fn ci_requires_every_configured_job_not_only_one_green_check() {
    let required = ci(&[UBUNTU, WINDOWS]).required_jobs;
    let mut run = serde_json::json!({
        "status": "completed", "conclusion": "success",
        "jobs": [
            {"name": UBUNTU, "status": "completed", "conclusion": "success"},
            {"name": WINDOWS, "status": "completed", "conclusion": "success"}
        ]
    });
    assert_eq!(ci_state(&run, &required), "pass");
    run["jobs"][1]["conclusion"] = "skipped".into();
    assert_eq!(ci_state(&run, &required), "fail");
    run["jobs"][1]["conclusion"] = "success".into();
    run["jobs"].as_array_mut().unwrap().pop();
    assert_eq!(ci_state(&run, &required), "pending");
    run["status"] = "in_progress".into();
    assert_eq!(ci_state(&run, &required), "pending");
    run["status"] = "completed".into();
    run["jobs"].as_array_mut().unwrap().push(serde_json::json!({
        "name": "additional gate", "status": "completed", "conclusion": "failure"
    }));
    assert_eq!(ci_state(&run, &required), "fail");
    let single = ci(&["test"]).required_jobs;
    let one = serde_json::json!({
        "status": "completed", "conclusion": "success",
        "jobs": [{"name": "test", "status": "completed", "conclusion": "success"}]
    });
    assert_eq!(ci_state(&one, &single), "pass");
}

#[test]
fn stale_green_run_cannot_verify_a_new_commit() {
    let old = WorkflowRun {
        database_id: 1,
        head_sha: "a".repeat(40),
        status: "completed".into(),
        conclusion: "success".into(),
    };
    assert!(latest_run_for_head(&[], &"b".repeat(40)).unwrap().is_none());
    assert!(latest_run_for_head(&[old], &"b".repeat(40)).is_err());
}

#[test]
fn remote_is_exactly_pinned_not_a_suffix_on_another_host() {
    assert!(remote_matches(
        "git@github.com:owner/project.git",
        "owner/project"
    ));
    assert!(remote_matches(
        "https://github.com/owner/project",
        "owner/project"
    ));
    assert!(!remote_matches(
        "https://evil.example/github.com/owner/project",
        "owner/project"
    ));
}

#[test]
fn diagnostics_keep_the_error_but_hide_a_token() {
    let log = "error: request token=github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890 was rejected";
    let summary = summarize_failure(log);
    assert!(summary.contains("error:"));
    assert!(summary.contains("[redacted]"));
    assert!(!summary.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
}

#[test]
fn request_rejects_directories_and_git_metadata_syntax() {
    let mut value = request();
    value.files = vec![".git/config".into()];
    assert!(!ok(&value));
    value.files = vec!["src/lib.rs\n--author=Bot".into()];
    assert!(!ok(&value));
    value.files = vec!["src/lib.rs".into(), "src/lib.rs".into()];
    assert!(!ok(&value));
}

#[test]
fn built_in_protected_paths_need_the_operator() {
    let mut value = request();
    for path in [
        ".github/workflows/ci.yml",
        ".GiT/config",
        ".gitattributes",
        "CODEOWNERS",
        "docs/CODEOWNERS",
        "src/*.rs",
    ] {
        value.files = vec![path.into()];
        assert!(!ok(&value), "accepted {path}");
    }
    value.files = vec!["Cargo.toml".into()];
    assert!(ok(&value), "Cargo.toml is protected only when configured");
}

#[test]
fn configured_protected_paths_are_exact_files_or_folder_prefixes() {
    let policy = Policy {
        protected: vec!["cargo.toml".into(), ".cargo/".into(), "build.rs".into()],
        blocked: Vec::new(),
    };
    let mut value = request();
    for path in ["Cargo.toml", ".cargo/config.toml", "build.rs"] {
        value.files = vec![path.into()];
        assert!(
            validate_request(&value, &policy).is_err(),
            "accepted {path}"
        );
    }
    for path in ["crates/core/Cargo.toml", "src/build.rs", ".cargo-notes.md"] {
        value.files = vec![path.into()];
        assert!(validate_request(&value, &policy).is_ok(), "refused {path}");
    }
}

#[test]
fn only_a_run_whose_every_job_github_refused_for_billing_is_checked_locally() {
    let note = "The job was not started because recent account payments have failed or your \
                spending limit needs to be increased. Please check the 'Billing & plans' \
                section in your settings";
    let job = |id, steps| BlockedJobProbe { id, steps };
    let blocked = vec![vec![note.to_string()], vec![note.to_string()]];
    assert!(jobs_all_billing_blocked(&[job(1, 0), job(2, 0)], &blocked));
    // One job actually ran: its failure is real, so no local fallback.
    assert!(!jobs_all_billing_blocked(&[job(1, 0), job(2, 7)], &blocked));
    let mixed = vec![
        vec![note.to_string()],
        vec!["Process completed with exit code 101.".into()],
    ];
    assert!(!jobs_all_billing_blocked(&[job(1, 0), job(2, 0)], &mixed));
    assert!(!jobs_all_billing_blocked(&[], &[]));
    assert!(!jobs_all_billing_blocked(&[job(1, 0)], &[]));
    assert!(is_billing_block(
        "YOUR SPENDING LIMIT NEEDS TO BE INCREASED"
    ));
    assert!(!is_billing_block("test result: FAILED"));

    // One job refused for billing and the other never taken by a runner still counts.
    let never = "The job was not acquired by Runner of type hosted even after multiple attempts";
    let windows = vec![vec![note.to_string()], vec![never.to_string()]];
    assert!(jobs_all_billing_blocked(&[job(1, 0), job(2, 0)], &windows));
    // Never acquired everywhere, with no billing note, is an outage: no local fallback.
    let outage = vec![vec![never.to_string()], vec![never.to_string()]];
    assert!(!jobs_all_billing_blocked(&[job(1, 0), job(2, 0)], &outage));
    // A never-acquired job that somehow ran steps is not blocked.
    assert!(!jobs_all_billing_blocked(&[job(1, 0), job(2, 3)], &windows));
}

#[test]
fn a_no_reply_address_is_not_passed_to_the_merge() {
    assert!(merge_author_args("1+me@users.noreply.github.com").is_empty());
    assert_eq!(
        merge_author_args("me@example.com"),
        ["--author-email", "me@example.com"]
    );
}

fn step(json: serde_json::Value) -> CheckStep {
    serde_json::from_value(json).unwrap()
}

#[test]
fn a_check_step_expands_the_cache_and_carries_no_github_token() {
    let program = if cfg!(windows) { "cmd" } else { "sh" };
    let check = step(serde_json::json!({
        "name": "probe",
        "program": program,
        "args": ["{cache}/x"],
        "dir": "sub",
        "env": {"CARGO_TARGET_DIR": "{cache}/target"}
    }));
    let cache = Path::new("c");
    let cmd = check_command(&check, Path::new("."), cache).unwrap();
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert!(envs.contains(&("CARGO_TARGET_DIR".into(), Some("c/target".into()))));
    assert!(!envs
        .iter()
        .any(|(k, v)| k.to_ascii_uppercase().starts_with("GH_") && v.is_some()));
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["c/x"]);
    assert_eq!(
        cmd.get_current_dir(),
        Some(Path::new(".").join("sub").as_path())
    );
}

#[test]
fn a_step_runs_only_when_its_paths_exist() {
    let dir = std::env::temp_dir().join(format!("agent-pr-runner-step-{}", std::process::id()));
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::write(dir.join("app/package.json"), "{}").unwrap();
    let app = step(serde_json::json!({
        "name": "npm ci", "program": "npm", "dir": "app",
        "only_if_exists": ["app/package.json"]
    }));
    let missing = step(serde_json::json!({
        "name": "pytest", "program": "python", "only_if_exists": ["pyproject.toml"]
    }));
    assert!(step_applies(&app, &dir));
    assert!(!step_applies(&missing, &dir));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn programs_resolve_on_path_and_missing_ones_do_not() {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    assert!(resolve_program(shell).is_some());
    assert!(resolve_program("definitely-not-a-real-program-xyz").is_none());
}

#[test]
fn local_checks_comment_lists_what_ran_and_what_did_not() {
    let note = local_checks_comment(&["cargo test".into(), "npm run build".into()]);
    assert!(note.contains("`cargo test`, `npm run build`"));
    assert!(note.contains("Other CI platforms were not checked"));
}

#[test]
fn commit_message_cannot_suppress_ci() {
    let mut value = request();
    for marker in [
        "[skip ci]",
        "[ci skip]",
        "[no ci]",
        "[skip actions]",
        "[actions skip]",
    ] {
        value.commit_message = format!("fix: test {marker}");
        assert!(!ok(&value), "accepted {marker}");
    }
    value.commit_message = "fix: test\0hidden".into();
    assert!(!ok(&value));
}

#[test]
fn ci_run_must_belong_to_this_pr_and_workflow_file() {
    let sha = "a".repeat(40);
    let base = "b".repeat(40);
    let file = ".github/workflows/ci.yml";
    let mut run = serde_json::json!({
        "head_sha": sha,
        "event": "pull_request",
        "path": ".github/workflows/ci.yml@refs/heads/main",
        "pull_requests": [{"number": 42, "head": {"sha": sha}, "base": {"sha": base}}]
    });
    assert!(run_matches_pr(&run, file, &sha, &base, 42));
    assert!(!run_matches_pr(&run, file, &sha, &base, 43));
    assert!(!run_matches_pr(&run, file, &sha, &"c".repeat(40), 42));
    run["path"] = ".github/workflows/lookalike.yml@main".into();
    assert!(!run_matches_pr(&run, file, &sha, &base, 42));
    run["path"] = ".github/workflows/ci.yml".into();
    run["pull_requests"] = serde_json::json!([]);
    assert!(run_matches_pr(&run, file, &sha, &base, 42));
    assert!(!run_matches_pr(
        &run,
        ".github/workflows/test.yml",
        &sha,
        &base,
        42
    ));
}

#[test]
fn pr_checks_must_link_every_required_job_to_the_exact_run() {
    let checks = vec![
        PrCheckLink {
            name: UBUNTU.into(),
            link: "https://github.com/owner/project/actions/runs/42/job/1".into(),
            workflow: Some("CI".into()),
        },
        PrCheckLink {
            name: WINDOWS.into(),
            link: "https://github.com/owner/project/actions/runs/42/job/2".into(),
            workflow: Some("CI".into()),
        },
    ];
    let both = ci(&[UBUNTU, WINDOWS]);
    assert!(checks_link_to_run(&checks, &both, "owner/project", 42));
    assert!(!checks_link_to_run(&checks, &both, "owner/project", 4));
    assert!(!checks_link_to_run(&checks, &both, "other/repo", 42));
    let three = ci(&[UBUNTU, WINDOWS, "lint + test (macos-latest)"]);
    assert!(!checks_link_to_run(&checks, &three, "owner/project", 42));
}

#[test]
fn a_failure_reported_only_on_stdout_is_not_an_empty_detail() {
    let whitespace = Capture {
        stdout: "notes/a.md:236: trailing whitespace.\n+text\n".into(),
        stderr: String::new(),
        success: false,
    };
    let err = checked(whitespace, "checking staged diff")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("notes/a.md:236: trailing whitespace."),
        "{err}"
    );
    let both = Capture {
        stdout: "noise".into(),
        stderr: "fatal: bad token=ghp_abcdefghijklmnopqrstuvwxyz\n".into(),
        success: false,
    };
    let err = checked(both, "pushing").unwrap_err().to_string();
    assert!(err.contains("fatal:") && !err.contains("noise"), "{err}");
    assert!(!err.contains("ghp_abc"), "still redacted: {err}");
}

#[test]
fn the_example_configs_parse_and_keep_their_promises() {
    for (name, text) in [
        (
            "config.example.json",
            include_str!("../config.example.json"),
        ),
        ("rust", include_str!("../examples/checks/rust.json")),
        ("node", include_str!("../examples/checks/node.json")),
        ("python", include_str!("../examples/checks/python.json")),
        (
            "rust-tauri",
            include_str!("../examples/checks/rust-tauri.json"),
        ),
    ] {
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        let checks = if name == "config.example.json" {
            let config: Config = serde_json::from_value(value).expect(name);
            assert!(!config.ci.required_jobs.is_empty(), "{name}");
            config.local_checks
        } else {
            serde_json::from_value::<LocalChecksConfig>(value).expect(name)
        };
        assert!(!checks.steps.is_empty(), "{name}");
        assert!(
            checks
                .steps
                .iter()
                .all(|s| safe_relative(&s.dir) && s.only_if_exists.iter().all(|p| safe_relative(p))),
            "{name}"
        );
    }
    let request: Request =
        serde_json::from_str(include_str!("../examples/request.example.json")).unwrap();
    assert!(ok(&request));
}

#[test]
fn step_folders_must_stay_inside_the_checkout() {
    assert!(safe_relative("."));
    assert!(safe_relative("app/src-tauri"));
    assert!(!safe_relative("../elsewhere"));
    assert!(!safe_relative("/etc"));
    assert!(!safe_relative("app\\src"));
    assert!(!safe_relative(""));
}
