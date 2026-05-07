use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

type AppResult<T> = Result<T, String>;

const DEFAULT_PLAN_FILE: &str = "/tmp/git-push-scheduler.plan";
const DEFAULT_MIN_DELAY_SECONDS: u64 = 720;
const DEFAULT_MAX_DELAY_SECONDS: u64 = 5400;
const NETWORK_RETRY_ATTEMPTS: u32 = 5;
const NETWORK_RETRY_BASE_DELAY_SECONDS: u64 = 2;
const DAEMON_POLL_INTERVAL_SECONDS: u64 = 30;
// Sentinel base SHA for fresh repos where the remote branch does not yet exist.
const EMPTY_SHA: &str = "0000000000000000000000000000000000000000";

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> AppResult<()> {
    match parse_cli(env::args().collect())? {
        Mode::Plan(args) => {
            let plan = build_plan(args)?;
            print_plan_summary(&plan);
            write_plan_file(&plan)?;
        }
        Mode::Execute(args) => execute_plan(args)?,
        Mode::Script(args) => render_script(args)?,
        Mode::Account(args) => cmd_account(args)?,
        Mode::Split(args) => cmd_split(args)?,
        Mode::Daemon(args) => cmd_daemon(args)?,
        Mode::DaemonInstall(args) => cmd_daemon_install(args)?,
    }
    Ok(())
}

// ─── modes ───────────────────────────────────────────────────────────────────

enum Mode {
    Plan(PlanArgs),
    Execute(ExecuteArgs),
    Script(ScriptArgs),
    Account(AccountArgs),
    Split(SplitArgs),
    Daemon(DaemonArgs),
    DaemonInstall(DaemonInstallArgs),
}

struct PlanArgs {
    repo: PathBuf,
    plan_file: PathBuf,
    min_delay_seconds: u64,
    max_delay_seconds: u64,
    first_delay_seconds: Option<u64>,
    seed: Option<u64>,
    account_name: Option<String>,
}

struct ExecuteArgs {
    plan_file: PathBuf,
    yes: bool,
    background: bool,
    log_file: Option<PathBuf>,
    token_override: Option<String>,
}

struct ScriptArgs {
    plan_file: PathBuf,
    shell: ScriptShell,
    output: Option<PathBuf>,
}

struct SplitArgs {
    repo: PathBuf,
    count: Option<usize>,
    message_prefix: Option<String>,
}

struct AccountArgs {
    action: AccountAction,
}

struct DaemonArgs {
    dir: PathBuf,
    poll_interval_seconds: u64,
}

struct DaemonInstallArgs {
    dir: Option<PathBuf>,
}

enum AccountAction {
    Add {
        name: String,
        github_username: String,
        github_email: String,
        token: String,
    },
    List,
    Remove(String),
    Verify {
        name: String,
        repo_path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy)]
enum ScriptShell {
    Bash,
    PowerShell,
}

// ─── data structures ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Plan {
    repo_path: String,
    branch: String,
    upstream_short: String,
    remote: String,
    remote_branch: String,
    remote_ref: String,
    remote_url: String,
    base_sha: String,
    planned_at_epoch: u64,
    seed: u64,
    plan_file: PathBuf,
    commits: Vec<CommitPlan>,
    github_username: String,
    github_email: String,
}

#[derive(Clone)]
struct CommitPlan {
    sha: String,
    subject: String,
    body: String,
    author_name: String,
    author_email: String,
    committer_name: String,
    committer_email: String,
    files_changed: u64,
    binary_files: u64,
    insertions: u64,
    deletions: u64,
    size_band: String,
    delay_seconds: u64,
    cumulative_seconds: u64,
}

struct UpstreamInfo {
    short_name: String,
    remote: String,
    remote_ref: String,
    remote_branch: String,
}

struct MarkerIssue {
    sha: String,
    field: &'static str,
    marker: String,
}

#[derive(Clone)]
struct AccountConfig {
    name: String,
    github_username: String,
    github_email: String,
    token: String,
}

struct NumstatFile {
    path: String,
    insertions: u64,
    deletions: u64,
}

// ─── CLI parsing ─────────────────────────────────────────────────────────────

fn parse_cli(args: Vec<String>) -> AppResult<Mode> {
    if args.len() < 2 {
        return Err(usage());
    }

    match args[1].as_str() {
        "plan" => parse_plan_args(&args[2..]).map(Mode::Plan),
        "execute" => parse_execute_args(&args[2..]).map(Mode::Execute),
        "script" => parse_script_args(&args[2..]).map(Mode::Script),
        "account" => parse_account_args(&args[2..]).map(Mode::Account),
        "split" => parse_split_args(&args[2..]).map(Mode::Split),
        "daemon" => parse_daemon_args(&args[2..]).map(Mode::Daemon),
        "daemon-install" => parse_daemon_install_args(&args[2..]).map(Mode::DaemonInstall),
        "--help" | "-h" | "help" => Err(usage()),
        other => Err(format!("unknown subcommand '{other}'\n\n{}", usage())),
    }
}

fn parse_plan_args(args: &[String]) -> AppResult<PlanArgs> {
    let mut repo = PathBuf::from(".");
    let mut plan_file = PathBuf::from(DEFAULT_PLAN_FILE);
    let mut min_delay_seconds = DEFAULT_MIN_DELAY_SECONDS;
    let mut max_delay_seconds = DEFAULT_MAX_DELAY_SECONDS;
    let mut first_delay_seconds = None;
    let mut seed = None;
    let mut account_name = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => { i += 1; repo = PathBuf::from(args.get(i).ok_or("--repo requires a path")?); }
            "--plan-file" => { i += 1; plan_file = PathBuf::from(args.get(i).ok_or("--plan-file requires a path")?); }
            "--min-delay-seconds" => { i += 1; min_delay_seconds = parse_u64(args.get(i), "--min-delay-seconds")?; }
            "--max-delay-seconds" => { i += 1; max_delay_seconds = parse_u64(args.get(i), "--max-delay-seconds")?; }
            "--min-delay-minutes" => { i += 1; min_delay_seconds = parse_u64(args.get(i), "--min-delay-minutes")? * 60; }
            "--max-delay-minutes" => { i += 1; max_delay_seconds = parse_u64(args.get(i), "--max-delay-minutes")? * 60; }
            "--first-delay-seconds" => { i += 1; first_delay_seconds = Some(parse_u64(args.get(i), "--first-delay-seconds")?); }
            "--seed" => { i += 1; seed = Some(parse_u64(args.get(i), "--seed")?); }
            "--account" => { i += 1; account_name = Some(args.get(i).ok_or("--account requires a name")?.clone()); }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown plan option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    if min_delay_seconds > max_delay_seconds {
        return Err("--min-delay-seconds cannot be greater than --max-delay-seconds".to_string());
    }

    Ok(PlanArgs { repo, plan_file, min_delay_seconds, max_delay_seconds, first_delay_seconds, seed, account_name })
}

fn parse_execute_args(args: &[String]) -> AppResult<ExecuteArgs> {
    let mut plan_file = PathBuf::from(DEFAULT_PLAN_FILE);
    let mut yes = false;
    let mut background = false;
    let mut log_file = None;
    let mut token_override = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan-file" => { i += 1; plan_file = PathBuf::from(args.get(i).ok_or("--plan-file requires a path")?); }
            "--yes" => yes = true,
            "--background" => background = true,
            "--log-file" => { i += 1; log_file = Some(PathBuf::from(args.get(i).ok_or("--log-file requires a path")?)); }
            "--token" => { i += 1; token_override = Some(args.get(i).ok_or("--token requires a value")?.clone()); }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown execute option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    Ok(ExecuteArgs { plan_file, yes, background, log_file, token_override })
}

fn parse_script_args(args: &[String]) -> AppResult<ScriptArgs> {
    let mut plan_file = PathBuf::from(DEFAULT_PLAN_FILE);
    let mut shell = ScriptShell::Bash;
    let mut output = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plan-file" => { i += 1; plan_file = PathBuf::from(args.get(i).ok_or("--plan-file requires a path")?); }
            "--shell" => { i += 1; shell = parse_shell(args.get(i))?; }
            "--output" => { i += 1; output = Some(PathBuf::from(args.get(i).ok_or("--output requires a path")?)); }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown script option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    Ok(ScriptArgs { plan_file, shell, output })
}

fn parse_split_args(args: &[String]) -> AppResult<SplitArgs> {
    let mut repo = PathBuf::from(".");
    let mut count = None;
    let mut message_prefix = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => { i += 1; repo = PathBuf::from(args.get(i).ok_or("--repo requires a path")?); }
            "--count" => {
                i += 1;
                let n = parse_u64(args.get(i), "--count")? as usize;
                if n == 0 { return Err("--count must be at least 1".to_string()); }
                count = Some(n);
            }
            "--message-prefix" => { i += 1; message_prefix = Some(args.get(i).ok_or("--message-prefix requires a value")?.clone()); }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown split option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    Ok(SplitArgs { repo, count, message_prefix })
}

fn parse_account_args(args: &[String]) -> AppResult<AccountArgs> {
    if args.is_empty() {
        return Err(format!("account requires a subcommand: add, list, remove, verify\n\n{}", usage()));
    }
    let action = match args[0].as_str() {
        "add" => {
            let mut name = None;
            let mut github_username = None;
            let mut github_email = None;
            let mut token = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => { i += 1; name = Some(args.get(i).ok_or("--name requires a value")?.clone()); }
                    "--username" => { i += 1; github_username = Some(args.get(i).ok_or("--username requires a value")?.clone()); }
                    "--email" => { i += 1; github_email = Some(args.get(i).ok_or("--email requires a value")?.clone()); }
                    "--token" => { i += 1; token = Some(args.get(i).ok_or("--token requires a value")?.clone()); }
                    other => return Err(format!("unknown account add option '{other}'")),
                }
                i += 1;
            }
            AccountAction::Add {
                name: name.ok_or("account add requires --name")?,
                github_username: github_username.ok_or("account add requires --username")?,
                github_email: github_email.ok_or("account add requires --email")?,
                token: token.ok_or("account add requires --token")?,
            }
        }
        "list" => AccountAction::List,
        "remove" => {
            let mut name = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => { i += 1; name = Some(args.get(i).ok_or("--name requires a value")?.clone()); }
                    other => return Err(format!("unknown account remove option '{other}'")),
                }
                i += 1;
            }
            AccountAction::Remove(name.ok_or("account remove requires --name")?)
        }
        "verify" => {
            let mut name = None;
            let mut repo_path = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => { i += 1; name = Some(args.get(i).ok_or("--name requires a value")?.clone()); }
                    "--repo" => { i += 1; repo_path = Some(PathBuf::from(args.get(i).ok_or("--repo requires a path")?)); }
                    other => return Err(format!("unknown account verify option '{other}'")),
                }
                i += 1;
            }
            AccountAction::Verify {
                name: name.ok_or("account verify requires --name")?,
                repo_path,
            }
        }
        other => return Err(format!("unknown account subcommand '{other}'; expected add, list, remove, verify")),
    };
    Ok(AccountArgs { action })
}

fn parse_daemon_args(args: &[String]) -> AppResult<DaemonArgs> {
    let default_dir = default_config_dir();
    let mut dir = default_dir;
    let mut poll_interval_seconds = DAEMON_POLL_INTERVAL_SECONDS;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => { i += 1; dir = PathBuf::from(args.get(i).ok_or("--dir requires a path")?); }
            "--poll-interval-seconds" => { i += 1; poll_interval_seconds = parse_u64(args.get(i), "--poll-interval-seconds")?; }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown daemon option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    Ok(DaemonArgs { dir, poll_interval_seconds })
}

fn parse_daemon_install_args(args: &[String]) -> AppResult<DaemonInstallArgs> {
    let mut dir = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => { i += 1; dir = Some(PathBuf::from(args.get(i).ok_or("--dir requires a path")?)); }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown daemon-install option '{other}'\n\n{}", usage())),
        }
        i += 1;
    }

    Ok(DaemonInstallArgs { dir })
}

fn parse_shell(value: Option<&String>) -> AppResult<ScriptShell> {
    match value.ok_or("--shell requires a value: bash or powershell")?.to_ascii_lowercase().as_str() {
        "bash" | "sh" => Ok(ScriptShell::Bash),
        "powershell" | "pwsh" | "ps1" => Ok(ScriptShell::PowerShell),
        other => Err(format!("unsupported shell '{other}'; expected bash or powershell")),
    }
}

fn usage() -> String {
    format!(
        "push_scheduler plan   [--repo PATH] [--plan-file PATH]\n\
         \x20               [--min-delay-seconds N] [--max-delay-seconds N]\n\
         \x20               [--first-delay-seconds N] [--seed N] [--account NAME]\n\
         push_scheduler split  [--repo PATH] [--count N] [--message-prefix TEXT]\n\
         push_scheduler script [--plan-file PATH] [--shell bash|powershell] [--output PATH]\n\
         push_scheduler execute [--plan-file PATH] --yes [--background] [--log-file PATH] [--token TOKEN]\n\
         push_scheduler account add --name NAME --username USER --email EMAIL --token TOKEN\n\
         push_scheduler account list\n\
         push_scheduler account remove --name NAME\n\
         push_scheduler account verify --name NAME [--repo PATH]\n\
         push_scheduler daemon [--dir PATH] [--poll-interval-seconds N]\n\
         push_scheduler daemon-install [--dir PATH]\n\n\
         Defaults:\n\
           plan file:          {DEFAULT_PLAN_FILE}\n\
           min delay seconds:  {DEFAULT_MIN_DELAY_SECONDS} ({}m)\n\
           max delay seconds:  {DEFAULT_MAX_DELAY_SECONDS} ({}m)\n\
           daemon poll:        {DAEMON_POLL_INTERVAL_SECONDS}s\n\
           daemon dir:         ~/.config/push_scheduler",
        DEFAULT_MIN_DELAY_SECONDS / 60,
        DEFAULT_MAX_DELAY_SECONDS / 60,
    )
}

fn parse_u64(value: Option<&String>, flag_name: &str) -> AppResult<u64> {
    value
        .ok_or_else(|| format!("{flag_name} requires a numeric value"))?
        .parse::<u64>()
        .map_err(|_| format!("{flag_name} requires a numeric value"))
}

fn default_config_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join("push_scheduler")
}

// ─── plan command ─────────────────────────────────────────────────────────────

fn build_plan(args: PlanArgs) -> AppResult<Plan> {
    let repo_path = canonical_repo_path(&args.repo)?;
    let branch = git(&repo_path, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err("HEAD is detached; switch to a named branch before planning".to_string());
    }

    // Try to find a configured upstream; fall back to inferring remote for fresh repos.
    let (upstream, base_sha) = match upstream_info(&repo_path, &branch) {
        Ok(info) => {
            let sha = git(&repo_path, &["rev-parse", "@{upstream}"])?;
            (info, sha)
        }
        Err(_) => {
            let remote = infer_remote(&repo_path)?;
            let remote_ref = format!("refs/heads/{branch}");
            let upstream_short = format!("{remote}/{branch}");
            let info = UpstreamInfo {
                short_name: upstream_short,
                remote,
                remote_ref,
                remote_branch: branch.clone(),
            };
            (info, EMPTY_SHA.to_string())
        }
    };

    let remote_url = git(&repo_path, &["remote", "get-url", &upstream.remote]).unwrap_or_default();

    let rev_list = if base_sha == EMPTY_SHA {
        // Fresh repo — include all commits from the very first one.
        git(&repo_path, &["rev-list", "--reverse", "HEAD"])?
    } else {
        git(&repo_path, &["rev-list", "--reverse", "@{upstream}..HEAD"])?
    };

    let pending_shas: Vec<String> = rev_list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if pending_shas.is_empty() {
        if base_sha == EMPTY_SHA {
            return Err("no commits in repository; create at least one commit before planning".to_string());
        }
        return Err("no commits are ahead of the tracked upstream branch".to_string());
    }

    if base_sha == EMPTY_SHA {
        eprintln!("note: remote branch '{}/{}' does not exist yet — will be created on first push",
            upstream.remote, upstream.remote_branch);
    }

    let mut commits = Vec::with_capacity(pending_shas.len());
    let mut marker_issues = Vec::new();
    for sha in pending_shas {
        let commit = collect_commit(&repo_path, &sha)?;
        marker_issues.extend(find_marker_issues(&commit));
        commits.push(commit);
    }

    if !marker_issues.is_empty() {
        return Err(format_marker_issues(&marker_issues));
    }

    // Resolve account
    let (github_username, github_email, account_token) = if let Some(name) = &args.account_name {
        let accounts = load_accounts()?;
        let acc = accounts.iter()
            .find(|a| a.name == *name)
            .ok_or_else(|| format!("account '{}' not found; run 'push_scheduler account list' to see available accounts", name))?
            .clone();
        (acc.github_username.clone(), acc.github_email.clone(), Some(acc.token.clone()))
    } else {
        (String::new(), String::new(), None)
    };

    // Quick-fail: verify account can push to this remote
    if let Some(ref token) = account_token {
        println!("Verifying account '{}' against remote...", args.account_name.as_deref().unwrap_or(""));
        let auth_url = build_authenticated_url(&remote_url, &github_username, token)?;
        quick_verify_remote(&repo_path, &auth_url, args.account_name.as_deref().unwrap_or(""), token)?;
        println!("  Account verified.");
    }

    let planned_at_epoch = now_epoch_seconds()?;
    let seed = args.seed.unwrap_or_else(|| {
        planned_at_epoch ^ (commits.len() as u64).wrapping_mul(7919)
    });
    let mut rng = XorShift64::new(seed);

    let mut cumulative_seconds = 0u64;
    for (index, commit) in commits.iter_mut().enumerate() {
        let (size_band, delay_seconds) = if index == 0 {
            let band = classify_commit(commit).0.to_string();
            let first_delay = args.first_delay_seconds.unwrap_or(0);
            (band, first_delay)
        } else {
            let (band, min_secs, max_secs) = classify_commit(commit);
            let base = rng.range_inclusive(min_secs, max_secs);
            let base = base.clamp(args.min_delay_seconds, args.max_delay_seconds);
            let jitter = rng.range_inclusive(0, 89);
            (band.to_string(), base + jitter)
        };
        cumulative_seconds += delay_seconds;
        commit.size_band = size_band;
        commit.delay_seconds = delay_seconds;
        commit.cumulative_seconds = cumulative_seconds;
    }

    Ok(Plan {
        repo_path: repo_path.display().to_string(),
        branch,
        upstream_short: upstream.short_name,
        remote: upstream.remote,
        remote_branch: upstream.remote_branch,
        remote_ref: upstream.remote_ref,
        remote_url,
        base_sha,
        planned_at_epoch,
        seed,
        plan_file: args.plan_file,
        commits,
        github_username,
        github_email,
    })
}

// ─── execute command ──────────────────────────────────────────────────────────

fn execute_plan(args: ExecuteArgs) -> AppResult<()> {
    if !args.yes {
        return Err("execution requires --yes so the plan cannot run accidentally".to_string());
    }

    if args.background {
        return execute_background(&args);
    }

    let plan = read_plan_file(&args.plan_file)?;
    let repo_path = canonical_repo_path(Path::new(&plan.repo_path))?;
    validate_local_state(&plan, &repo_path)?;
    run_plan_core(&plan, &repo_path, args.token_override.as_deref())
}

fn validate_local_state(plan: &Plan, repo_path: &Path) -> AppResult<()> {
    let current_branch = git(repo_path, &["branch", "--show-current"])?;
    if current_branch != plan.branch {
        return Err(format!(
            "current branch is '{}' but the saved plan targets '{}'; re-plan or switch back first",
            current_branch, plan.branch
        ));
    }

    let last_commit = plan.commits.last()
        .ok_or_else(|| "plan file contains no commits".to_string())?;
    let head_sha = git(repo_path, &["rev-parse", "HEAD"])?;
    if head_sha != last_commit.sha {
        return Err(format!(
            "HEAD is {} but the saved plan expected {}; local history changed, re-plan",
            short_sha(&head_sha), short_sha(&last_commit.sha)
        ));
    }

    Ok(())
}

fn run_plan_core(plan: &Plan, repo_path: &Path, token_override: Option<&str>) -> AppResult<()> {
    let account = resolve_push_account(plan, token_override)?;
    if let Some(ref acc) = account {
        println!("Pushing as account '{}' ({})", acc.name, acc.github_username);
        let auth_url = build_authenticated_url(&plan.remote_url, &acc.github_username, &acc.token)?;
        quick_verify_remote(repo_path, &auth_url, &acc.name, &acc.token)?;
    }

    // Verify remote state hasn't drifted (skip when remote branch doesn't exist yet)
    if plan.base_sha != EMPTY_SHA {
        let fetch_refspec = format!("{}:refs/remotes/{}/{}", plan.remote_ref, plan.remote, plan.remote_branch);
        let fetch_url = account.as_ref()
            .map(|a| build_authenticated_url(&plan.remote_url, &a.github_username, &a.token))
            .transpose()?;
        let fetch_target = fetch_url.as_deref().unwrap_or(&plan.remote);
        git_with_retry(
            repo_path,
            &["fetch", "--quiet", fetch_target, &fetch_refspec],
            "fetch remote branch state",
        )?;

        let remote_tracking_ref = format!("refs/remotes/{}/{}", plan.remote, plan.remote_branch);
        let fetched_remote_sha = git(repo_path, &["rev-parse", &remote_tracking_ref])?;
        if fetched_remote_sha != plan.base_sha {
            return Err(format!(
                "remote branch moved from {} to {} since plan was created; re-plan before pushing",
                short_sha(&plan.base_sha), short_sha(&fetched_remote_sha)
            ));
        }
    } else {
        eprintln!("note: fresh push — remote branch will be created by the first push");
    }

    println!(
        "Executing plan: {} commits on {} -> {}",
        plan.commits.len(), plan.branch, plan.upstream_short
    );

    for (index, commit) in plan.commits.iter().enumerate() {
        if commit.delay_seconds > 0 && (index > 0 || commit.delay_seconds > 0) {
            if index == 0 {
                println!(
                    "Waiting {} before first push ({})",
                    format_duration(commit.delay_seconds), short_sha(&commit.sha)
                );
            } else {
                println!(
                    "Waiting {} before push {}/{} ({})",
                    format_duration(commit.delay_seconds),
                    index + 1, plan.commits.len(), short_sha(&commit.sha)
                );
            }
            thread::sleep(Duration::from_secs(commit.delay_seconds));
        }

        let expected_remote_sha = if index == 0 {
            &plan.base_sha
        } else {
            &plan.commits[index - 1].sha
        };

        // Skip drift check when pushing to a branch that doesn't exist on the remote yet.
        if *expected_remote_sha != EMPTY_SHA {
            let remote_sha = if let Some(ref acc) = account {
                let auth_url = build_authenticated_url(&plan.remote_url, &acc.github_username, &acc.token)?;
                remote_head_sha_auth_with_retry(repo_path, &auth_url, &plan.remote_branch, &acc.token)?
            } else {
                remote_head_sha_with_retry(repo_path, &plan.remote, &plan.remote_branch)?
            };

            if remote_sha != *expected_remote_sha {
                return Err(format!(
                    "remote branch drifted before push {}/{}: expected {}, found {}; re-plan",
                    index + 1, plan.commits.len(),
                    short_sha(expected_remote_sha), short_sha(&remote_sha)
                ));
            }
        }

        println!(
            "Pushing {}/{} {} {}",
            index + 1, plan.commits.len(), short_sha(&commit.sha), commit.subject
        );

        let remote_ref = format!("refs/heads/{}", plan.remote_branch);
        let push_refspec = format!("{}:{}", commit.sha, remote_ref);

        if let Some(ref acc) = account {
            let auth_url = build_authenticated_url(&plan.remote_url, &acc.github_username, &acc.token)?;
            git_with_retry_env(
                repo_path,
                &["push", &auth_url, &push_refspec],
                "push commit",
                &[("GIT_TERMINAL_PROMPT", "0")],
                &acc.token,
            )?;
        } else {
            git_with_retry(
                repo_path,
                &["push", &plan.remote, &push_refspec],
                "push commit",
            )?;
        }
    }

    println!(
        "Done — staggered push complete in {}",
        format_duration(plan.commits.last().map(|c| c.cumulative_seconds).unwrap_or_default())
    );
    Ok(())
}

fn execute_background(args: &ExecuteArgs) -> AppResult<()> {
    let log_path = args.log_file.clone().unwrap_or_else(|| {
        let ts = now_epoch_seconds().unwrap_or(0);
        PathBuf::from(format!("/tmp/push_scheduler_{ts}.log"))
    });

    let exe = std::env::current_exe()
        .map_err(|e| format!("failed to locate current executable: {e}"))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("execute");
    cmd.arg("--plan-file");
    cmd.arg(&args.plan_file);
    cmd.arg("--yes");
    if let Some(ref token) = args.token_override {
        cmd.arg("--token");
        cmd.arg(token);
    }

    let log_file = File::create(&log_path)
        .map_err(|e| format!("failed to create log file '{}': {e}", log_path.display()))?;
    let log_clone = log_file.try_clone()
        .map_err(|e| format!("failed to clone log file handle: {e}"))?;

    cmd.stdout(log_file)
        .stderr(log_clone)
        .stdin(File::open("/dev/null").map_err(|e| format!("cannot open /dev/null: {e}"))?);

    let child = cmd.spawn()
        .map_err(|e| format!("failed to start background process: {e}"))?;

    let pid = child.id();
    std::mem::forget(child);

    println!("Background push started.");
    println!("  PID:     {pid}");
    println!("  Log:     {}", log_path.display());
    println!("  Monitor: tail -f {}", log_path.display());
    println!("  Stop:    kill {pid}");
    Ok(())
}

// ─── daemon command ───────────────────────────────────────────────────────────

fn cmd_daemon(args: DaemonArgs) -> AppResult<()> {
    let pending_dir = args.dir.join("pending");
    let running_dir = args.dir.join("running");
    let done_dir    = args.dir.join("done");
    let failed_dir  = args.dir.join("failed");

    for dir in [&pending_dir, &running_dir, &done_dir, &failed_dir] {
        fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }

    // Crash recovery: anything left in running/ didn't finish — move back to pending/
    for entry in fs::read_dir(&running_dir)
        .map_err(|e| format!("cannot read running dir: {e}"))? {
        let path = entry.map_err(|e| format!("dir entry error: {e}"))?.path();
        if path.extension().map_or(false, |e| e == "plan") {
            let dest = pending_dir.join(path.file_name().unwrap());
            if let Err(e) = fs::rename(&path, &dest) {
                daemon_log(&format!("warn: cannot recover {}: {e}", path.display()));
            } else {
                daemon_log(&format!("recovered interrupted plan: {}", path.file_name().unwrap().to_string_lossy()));
            }
        }
    }

    let (tx, rx) = mpsc::channel::<(PathBuf, AppResult<()>)>();

    daemon_log(&format!(
        "daemon started — watching {} every {}s",
        pending_dir.display(), args.poll_interval_seconds
    ));

    loop {
        // Collect completions from finished threads
        while let Ok((running_file, result)) = rx.try_recv() {
            let stem = running_file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ts = now_epoch_seconds().unwrap_or(0);
            match result {
                Ok(()) => {
                    let dest = done_dir.join(format!("{stem}_{ts}.plan"));
                    let _ = fs::rename(&running_file, &dest);
                    daemon_log(&format!("done: {stem}"));
                }
                Err(e) => {
                    let dest = failed_dir.join(format!("{stem}_{ts}.plan"));
                    let _ = fs::rename(&running_file, &dest);
                    daemon_log(&format!("failed: {stem}: {e}"));
                }
            }
        }

        // Scan pending/ and pick up any new plan files
        let pending_files: Vec<PathBuf> = match fs::read_dir(&pending_dir) {
            Ok(entries) => entries
                .filter_map(|e| {
                    let path = e.ok()?.path();
                    if path.extension()?.to_str()? == "plan" { Some(path) } else { None }
                })
                .collect(),
            Err(e) => {
                daemon_log(&format!("warn: cannot scan pending dir: {e}"));
                vec![]
            }
        };

        for plan_path in pending_files {
            let file_name = match plan_path.file_name() {
                Some(n) => n.to_owned(),
                None => continue,
            };
            let running_path = running_dir.join(&file_name);

            // Atomic claim: rename pending → running. Fails if another daemon instance raced us.
            if let Err(e) = fs::rename(&plan_path, &running_path) {
                daemon_log(&format!("warn: cannot claim {}: {e}", file_name.to_string_lossy()));
                continue;
            }

            daemon_log(&format!("starting: {}", file_name.to_string_lossy()));

            let tx2 = tx.clone();
            let rp  = running_path.clone();
            thread::spawn(move || {
                let result = run_plan_from_file(&rp);
                tx2.send((rp, result)).ok();
            });
        }

        thread::sleep(Duration::from_secs(args.poll_interval_seconds));
    }
}

fn run_plan_from_file(plan_file: &Path) -> AppResult<()> {
    let plan = read_plan_file(plan_file)?;
    let repo_path = canonical_repo_path(Path::new(&plan.repo_path))?;
    validate_local_state(&plan, &repo_path)?;
    run_plan_core(&plan, &repo_path, None)
}

fn daemon_log(msg: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("[{ts}] {msg}");
}

// ─── daemon-install command ───────────────────────────────────────────────────

fn cmd_daemon_install(args: DaemonInstallArgs) -> AppResult<()> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate current executable: {e}"))?;

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "HOME environment variable is not set".to_string())?;

    let dir = args.dir.unwrap_or_else(|| default_config_dir());

    let log_file = PathBuf::from(&home)
        .join("Library").join("Logs").join("push_scheduler_daemon.log");

    let plist_dir = PathBuf::from(&home).join("Library").join("LaunchAgents");
    fs::create_dir_all(&plist_dir)
        .map_err(|e| format!("cannot create LaunchAgents directory: {e}"))?;

    let plist_path = plist_dir.join("com.push-scheduler.daemon.plist");

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>com.push-scheduler.daemon</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exe}</string>\n\
         \t\t<string>daemon</string>\n\
         \t\t<string>--dir</string>\n\
         \t\t<string>{dir}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        exe = exe.display(),
        dir = dir.display(),
        log = log_file.display(),
    );

    fs::write(&plist_path, &plist)
        .map_err(|e| format!("cannot write plist to {}: {e}", plist_path.display()))?;

    // Unload any previous instance (ignore errors — may not be loaded yet)
    let _ = Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();

    let load_output = Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output()
        .map_err(|e| format!("launchctl not found: {e}"))?;

    if !load_output.status.success() {
        let stderr = String::from_utf8_lossy(&load_output.stderr).trim().to_string();
        return Err(format!("launchctl load failed: {stderr}"));
    }

    println!("Daemon installed and started via launchd.");
    println!("  Plist:    {}", plist_path.display());
    println!("  Drop dir: {}/pending/", dir.display());
    println!("  Log:      {}", log_file.display());
    println!("  Monitor:  tail -f {}", log_file.display());
    println!("  Stop:     launchctl unload {}", plist_path.display());
    println!("  Restart:  launchctl unload {path} && launchctl load {path}",
             path = plist_path.display());
    println!("\nAgent workflow: push_scheduler plan --repo PATH --plan-file {}/pending/job.plan",
             dir.display());

    Ok(())
}

// ─── script command ───────────────────────────────────────────────────────────

fn render_script(args: ScriptArgs) -> AppResult<()> {
    let plan = read_plan_file(&args.plan_file)?;
    let script = match args.shell {
        ScriptShell::Bash => render_bash_script(&plan)?,
        ScriptShell::PowerShell => render_powershell_script(&plan)?,
    };

    if let Some(output_path) = args.output {
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("failed to create script output directory '{}': {e}", parent.display())
            })?;
        }
        fs::write(&output_path, &script)
            .map_err(|e| format!("failed to write generated script '{}': {e}", output_path.display()))?;
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&output_path, fs::Permissions::from_mode(0o755));
        }
        println!("Saved script to {}", output_path.display());
    } else {
        print!("{script}");
    }

    Ok(())
}

// ─── account command ──────────────────────────────────────────────────────────

fn cmd_account(args: AccountArgs) -> AppResult<()> {
    match args.action {
        AccountAction::Add { name, github_username, github_email, token } => {
            let mut accounts = load_accounts().unwrap_or_default();
            if accounts.iter().any(|a| a.name == name) {
                return Err(format!("account '{}' already exists; remove it first with 'account remove --name {}'", name, name));
            }
            accounts.push(AccountConfig { name: name.clone(), github_username: github_username.clone(), github_email: github_email.clone(), token });
            save_accounts(&accounts)?;
            println!("Added account '{}' ({} <{}>)", name, github_username, github_email);
        }
        AccountAction::List => {
            let accounts = load_accounts().unwrap_or_default();
            if accounts.is_empty() {
                println!("No accounts configured.");
                println!("Add one with: push_scheduler account add --name NAME --username USER --email EMAIL --token TOKEN");
            } else {
                println!("Configured accounts ({}):", accounts.len());
                for acc in &accounts {
                    println!("  {} — {} <{}>", acc.name, acc.github_username, acc.github_email);
                }
            }
        }
        AccountAction::Remove(name) => {
            let mut accounts = load_accounts().unwrap_or_default();
            let before = accounts.len();
            accounts.retain(|a| a.name != name);
            if accounts.len() == before {
                return Err(format!("account '{}' not found", name));
            }
            save_accounts(&accounts)?;
            println!("Removed account '{}'.", name);
        }
        AccountAction::Verify { name, repo_path } => {
            let accounts = load_accounts()?;
            let acc = accounts.iter()
                .find(|a| a.name == name)
                .ok_or_else(|| format!("account '{}' not found", name))?;

            println!("Verifying account '{}' ({} <{}>)...", acc.name, acc.github_username, acc.github_email);

            let remote_url = if let Some(rp) = repo_path {
                let canonical = canonical_repo_path(&rp)?;
                let url = git(&canonical, &["remote", "get-url", "origin"])
                    .map_err(|e| format!("failed to get remote URL from repo: {e}"))?;
                println!("  Remote: {}", url);
                url
            } else {
                format!("https://github.com/{}", acc.github_username)
            };

            let auth_url = build_authenticated_url(&remote_url, &acc.github_username, &acc.token)?;
            let tmp = std::env::temp_dir();
            match git_env(&tmp, &["ls-remote", "--quiet", "--heads", &auth_url], &[("GIT_TERMINAL_PROMPT", "0")]) {
                Ok(_) => {
                    println!("  Authentication: OK");
                    println!("  Repository access: OK");
                    println!("Verification passed.");
                }
                Err(e) => {
                    let safe_err = redact_token(&e, &acc.token);
                    let hint = auth_error_hint(&safe_err);
                    eprintln!("  Authentication failed: {safe_err}");
                    for h in hint { eprintln!("  hint: {h}"); }
                    return Err(format!("account '{}' verification failed", name));
                }
            }
        }
    }
    Ok(())
}

// ─── split command ────────────────────────────────────────────────────────────

fn cmd_split(args: SplitArgs) -> AppResult<()> {
    let repo_path = canonical_repo_path(&args.repo)?;

    let status = git(&repo_path, &["status", "--porcelain"])?;
    if status.is_empty() {
        return Err("no uncommitted changes to split into commits".to_string());
    }

    // Stage all changes
    git(&repo_path, &["add", "-A"])?;

    // Get staged file stats
    let numstat_output = git(&repo_path, &["diff", "--cached", "--numstat"])?;
    let files = parse_numstat(&numstat_output)?;

    if files.is_empty() {
        return Err("no staged changes found after staging all files".to_string());
    }

    let total_lines: u64 = files.iter().map(|f| f.insertions + f.deletions).sum();
    let count = args.count.unwrap_or_else(|| auto_split_count(total_lines));
    let count = count.min(files.len());

    println!("Splitting {} changed files ({} lines) into {} commits...", files.len(), total_lines, count);

    // Reset staging area (keep working tree changes)
    git(&repo_path, &["reset", "HEAD"])?;

    let groups = split_files_into_groups(&files, count);

    for (i, group) in groups.iter().enumerate() {
        let group_lines: u64 = group.iter().map(|f| f.insertions + f.deletions).sum();
        let msg = auto_commit_message(group, args.message_prefix.as_deref());

        let mut add_args: Vec<&str> = vec!["add"];
        let paths: Vec<&str> = group.iter().map(|f| f.path.as_str()).collect();
        add_args.extend_from_slice(&paths);
        git(&repo_path, &add_args)?;

        git(&repo_path, &["commit", "-m", &msg])?;
        println!("  {}/{} committed: {} ({} files, {} lines)", i + 1, count, msg, group.len(), group_lines);
    }

    println!("\nCreated {} commits. Run 'push_scheduler plan --repo {}' to schedule pushes.", count, args.repo.display());
    Ok(())
}

fn parse_numstat(output: &str) -> AppResult<Vec<NumstatFile>> {
    let mut files = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        if cols.len() < 3 { continue; }
        let insertions = if cols[0] == "-" { 0 } else { cols[0].parse::<u64>().unwrap_or(0) };
        let deletions = if cols[1] == "-" { 0 } else { cols[1].parse::<u64>().unwrap_or(0) };
        let path = cols[2].to_string();
        files.push(NumstatFile { path, insertions, deletions });
    }
    Ok(files)
}

fn auto_split_count(total_lines: u64) -> usize {
    match total_lines {
        0..=99 => 3,
        100..=299 => 4,
        _ => 5,
    }
}

fn split_files_into_groups(files: &[NumstatFile], count: usize) -> Vec<Vec<&NumstatFile>> {
    if files.is_empty() || count == 0 { return vec![]; }
    let count = count.min(files.len());

    let mut sorted: Vec<&NumstatFile> = files.iter().collect();
    sorted.sort_by(|a, b| (b.insertions + b.deletions).cmp(&(a.insertions + a.deletions)));

    let mut groups: Vec<(u64, Vec<&NumstatFile>)> = (0..count).map(|_| (0u64, vec![])).collect();

    for file in sorted {
        let min_idx = groups.iter()
            .enumerate()
            .min_by_key(|(_, (size, _))| *size)
            .map(|(i, _)| i)
            .unwrap_or(0);
        groups[min_idx].0 += file.insertions + file.deletions;
        groups[min_idx].1.push(file);
    }

    groups.into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(_, v)| v)
        .collect()
}

fn auto_commit_message(files: &[&NumstatFile], prefix: Option<&str>) -> String {
    let dirs: BTreeSet<String> = files.iter()
        .map(|f| {
            let p = Path::new(&f.path);
            p.parent()
                .and_then(|d| d.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(".")
                .to_string()
        })
        .collect();

    let total_ins: u64 = files.iter().map(|f| f.insertions).sum();
    let total_del: u64 = files.iter().map(|f| f.deletions).sum();
    let verb = if total_del > total_ins * 3 { "Remove" } else if total_ins > total_del * 3 { "Add" } else { "Update" };

    let location = if dirs.len() == 1 {
        dirs.iter().next().unwrap().clone()
    } else {
        let parts: Vec<&str> = dirs.iter().take(2).map(|s| s.as_str()).collect();
        parts.join(", ")
    };

    let subject = format!("{verb} {location}");
    match prefix {
        Some(p) => format!("{p}: {subject}"),
        None => subject,
    }
}

// ─── account helpers ──────────────────────────────────────────────────────────

fn accounts_config_path() -> AppResult<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "cannot determine home directory".to_string())?;
    Ok(PathBuf::from(home).join(".config").join("push_scheduler").join("accounts"))
}

fn load_accounts() -> AppResult<Vec<AccountConfig>> {
    let path = accounts_config_path()?;
    if !path.exists() { return Ok(vec![]); }

    let file = File::open(&path)
        .map_err(|e| format!("failed to open accounts file '{}': {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut accounts = Vec::new();

    for raw_line in reader.lines() {
        let line = raw_line.map_err(|e| format!("failed to read accounts file: {e}"))?;
        if line.trim().is_empty() || line.starts_with('#') { continue; }
        let fields = split_escaped_fields(&line)?;
        if fields.is_empty() { continue; }
        if fields[0] == "account" && fields.len() >= 5 {
            accounts.push(AccountConfig {
                name: fields[1].clone(),
                github_username: fields[2].clone(),
                github_email: fields[3].clone(),
                token: fields[4].clone(),
            });
        }
    }
    Ok(accounts)
}

fn save_accounts(accounts: &[AccountConfig]) -> AppResult<()> {
    let path = accounts_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create config directory '{}': {e}", parent.display()))?;
    }

    let mut file = File::create(&path)
        .map_err(|e| format!("failed to create accounts file '{}': {e}", path.display()))?;

    #[cfg(unix)]
    {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set permissions on accounts file: {e}"))?;
    }

    writeln!(file, "# push_scheduler accounts — do not share this file")
        .map_err(|e| format!("failed to write accounts file: {e}"))?;

    for acc in accounts {
        let fields = [
            "account",
            acc.name.as_str(),
            acc.github_username.as_str(),
            acc.github_email.as_str(),
            acc.token.as_str(),
        ];
        let escaped: Vec<String> = fields.iter().map(|f| escape_field(f)).collect();
        writeln!(file, "{}", escaped.join("\t"))
            .map_err(|e| format!("failed to write accounts file: {e}"))?;
    }
    Ok(())
}

fn resolve_push_account(plan: &Plan, token_override: Option<&str>) -> AppResult<Option<AccountConfig>> {
    if plan.github_username.is_empty() { return Ok(None); }

    // Try token override first
    if let Some(token) = token_override {
        return Ok(Some(AccountConfig {
            name: plan.github_username.clone(),
            github_username: plan.github_username.clone(),
            github_email: plan.github_email.clone(),
            token: token.to_string(),
        }));
    }

    // Fall back to stored accounts matched by username
    let accounts = load_accounts().unwrap_or_default();
    if let Some(acc) = accounts.iter().find(|a| a.github_username == plan.github_username) {
        return Ok(Some(acc.clone()));
    }

    Err(format!(
        "plan requires GitHub account '{}' but no matching stored account found.\n\
         Run: push_scheduler account add --name NAME --username {} ...",
        plan.github_username, plan.github_username
    ))
}

fn build_authenticated_url(url: &str, _username: &str, token: &str) -> AppResult<String> {
    let https_url = ssh_to_https(url);
    if let Some(rest) = https_url.strip_prefix("https://") {
        // Remove any existing credentials in the URL
        let rest = if let Some(at_pos) = rest.find('@') { &rest[at_pos + 1..] } else { rest };
        Ok(format!("https://oauth2:{}@{}", token, rest))
    } else if https_url.starts_with("http://") {
        Err(format!("plain HTTP remote URL is not supported for account auth: {}", url))
    } else {
        Err(format!("cannot add credentials to non-HTTPS URL: {url}; set the remote to an HTTPS URL first"))
    }
}

fn ssh_to_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon_pos) = rest.find(':') {
            let host = &rest[..colon_pos];
            let path = &rest[colon_pos + 1..];
            return format!("https://{}/{}", host, path);
        }
    }
    url.to_string()
}

fn quick_verify_remote(repo_path: &Path, auth_url: &str, account_name: &str, token: &str) -> AppResult<()> {
    let tmp = std::env::temp_dir();
    let probe_dir = if repo_path.exists() { repo_path } else { &tmp };
    match git_env(probe_dir, &["ls-remote", "--quiet", "--heads", auth_url], &[("GIT_TERMINAL_PROMPT", "0")]) {
        Ok(_) => Ok(()),
        Err(e) => {
            let safe_err = redact_token(&e, token);
            let hints = auth_error_hint(&safe_err);
            let mut msg = format!("account '{}' cannot access remote: {}", account_name, safe_err);
            for h in hints { msg.push_str(&format!("\n  hint: {h}")); }
            Err(msg)
        }
    }
}

fn redact_token(s: &str, token: &str) -> String {
    if token.is_empty() { return s.to_string(); }
    s.replace(token, "***REDACTED***")
}

fn auth_error_hint(error: &str) -> Vec<&'static str> {
    let lower = error.to_lowercase();
    if lower.contains("authentication failed") || lower.contains("bad credentials") || lower.contains("invalid username") {
        vec![
            "the GitHub token may be expired or invalid",
            "ensure the token has 'repo' scope for private repos",
            "classic tokens start with 'ghp_', fine-grained with 'github_pat_'",
        ]
    } else if lower.contains("repository not found") || lower.contains("not found") {
        vec![
            "check the remote URL in the repository (git remote get-url origin)",
            "confirm the account has at least read access to this repository",
        ]
    } else if lower.contains("permission denied") || lower.contains("access denied") || lower.contains("403") {
        vec![
            "the token may lack write access; enable 'repo' scope or add write permission",
            "for fine-grained tokens, check repository 'Contents' permission is set to 'Read and write'",
        ]
    } else if lower.contains("could not read from remote") {
        vec![
            "the remote URL may be incorrect",
            "check network connectivity",
        ]
    } else {
        vec![]
    }
}

// ─── git helpers ──────────────────────────────────────────────────────────────

fn canonical_repo_path(path: &Path) -> AppResult<PathBuf> {
    if !path.exists() {
        return Err(format!("repository path '{}' does not exist", path.display()));
    }
    let top_level = git(path, &["rev-parse", "--show-toplevel"])?;
    fs::canonicalize(top_level.trim())
        .map_err(|e| format!("failed to resolve repository path: {e}"))
}

fn upstream_info(repo_path: &Path, branch: &str) -> AppResult<UpstreamInfo> {
    let upstream_ref = git(repo_path, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])?;
    let remote = git(repo_path, &["config", "--get", &format!("branch.{branch}.remote")])?;
    let remote_ref = git(repo_path, &["config", "--get", &format!("branch.{branch}.merge")])?;
    let short_name = upstream_ref.trim().to_string();
    let remote = remote.trim().to_string();
    let remote_ref = remote_ref.trim().to_string();

    if short_name.is_empty() || remote.is_empty() || remote_ref.is_empty() {
        return Err(format!(
            "branch '{}' has no tracked upstream; configure one before using this skill",
            branch
        ));
    }

    let remote_branch = remote_ref
        .strip_prefix("refs/heads/")
        .ok_or_else(|| format!("unsupported upstream ref '{}'", remote_ref))?
        .to_string();

    Ok(UpstreamInfo { short_name, remote, remote_ref, remote_branch })
}

fn infer_remote(repo_path: &Path) -> AppResult<String> {
    let remotes = git(repo_path, &["remote"])?;
    let first = remotes.lines().map(str::trim).find(|r| !r.is_empty());
    match first {
        Some(r) => Ok(r.to_string()),
        None => Err("no git remote configured; add one with: git remote add origin <url>".to_string()),
    }
}

fn collect_commit(repo_path: &Path, sha: &str) -> AppResult<CommitPlan> {
    let metadata = git(repo_path, &["show", "-s", "--format=%H%x1f%an%x1f%ae%x1f%cn%x1f%ce%x1f%s%x1f%B", sha])?;

    let mut parts = metadata.splitn(7, '\u{1f}');
    let commit_sha = parts.next().unwrap_or("").trim().to_string();
    let author_name = parts.next().unwrap_or("").trim().to_string();
    let author_email = parts.next().unwrap_or("").trim().to_string();
    let committer_name = parts.next().unwrap_or("").trim().to_string();
    let committer_email = parts.next().unwrap_or("").trim().to_string();
    let subject = parts.next().unwrap_or("").trim().to_string();
    let body = parts.next().unwrap_or("").trim().to_string();

    if commit_sha.is_empty() || subject.is_empty() {
        return Err(format!("failed to parse metadata for commit {sha}"));
    }

    let numstat = git(repo_path, &["show", "--numstat", "--format=", sha])?;
    let mut files_changed = 0u64;
    let mut binary_files = 0u64;
    let mut insertions = 0u64;
    let mut deletions = 0u64;

    for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 { continue; }
        files_changed += 1;
        let add_field = cols[0].trim();
        let del_field = cols[1].trim();
        if add_field == "-" || del_field == "-" { binary_files += 1; }
        if add_field != "-" { insertions += add_field.parse::<u64>().unwrap_or_default(); }
        if del_field != "-" { deletions += del_field.parse::<u64>().unwrap_or_default(); }
    }

    Ok(CommitPlan {
        sha: commit_sha, subject, body,
        author_name, author_email, committer_name, committer_email,
        files_changed, binary_files, insertions, deletions,
        size_band: String::new(), delay_seconds: 0, cumulative_seconds: 0,
    })
}

fn classify_commit(commit: &CommitPlan) -> (&'static str, u64, u64) {
    let score = commit.insertions + commit.deletions
        + (commit.files_changed * 12) + (commit.binary_files * 30);
    match score {
        0..=40 => ("small", 720, 1320),
        41..=140 => ("medium", 1080, 2100),
        141..=360 => ("large", 1800, 3300),
        _ => ("very-large", 2700, 5100),
    }
}

fn find_marker_issues(commit: &CommitPlan) -> Vec<MarkerIssue> {
    let marker_phrases = [
        "codex", "copilot", "claude", "chatgpt", "gpt", "openai", "anthropic",
        "llm", "ai generated", "ai-generated", "ai assisted", "ai-assisted", "generated by ai",
    ];
    let checks = [
        ("author_name", commit.author_name.as_str()),
        ("author_email", commit.author_email.as_str()),
        ("committer_name", commit.committer_name.as_str()),
        ("committer_email", commit.committer_email.as_str()),
        ("subject", commit.subject.as_str()),
        ("body", commit.body.as_str()),
    ];
    let mut issues = Vec::new();
    for (field, value) in checks {
        if let Some(marker) = contains_marker(value, &marker_phrases) {
            issues.push(MarkerIssue { sha: commit.sha.clone(), field, marker });
        }
    }
    issues
}

fn contains_marker(value: &str, markers: &[&str]) -> Option<String> {
    let normalized = normalize_marker_text(value);
    let padded = format!(" {normalized} ");
    for marker in markers {
        let needle = normalize_marker_text(marker);
        if padded.contains(&format!(" {needle} ")) {
            return Some(marker.to_string());
        }
    }
    None
}

fn normalize_marker_text(value: &str) -> String {
    let mut output = String::new();
    let mut prev_space = true;
    for ch in value.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            output.push(ch);
            prev_space = false;
        } else if !prev_space {
            output.push(' ');
            prev_space = true;
        }
    }
    output.trim().to_string()
}

fn format_marker_issues(issues: &[MarkerIssue]) -> String {
    let mut lines = vec![
        "commit metadata audit failed; the scheduler will not rewrite history or conceal markers".to_string(),
    ];
    for issue in issues {
        lines.push(format!("  {} field '{}' matched '{}'", short_sha(&issue.sha), issue.field, issue.marker));
    }
    lines.join("\n")
}

// ─── plan file I/O ────────────────────────────────────────────────────────────

fn write_plan_file(plan: &Plan) -> AppResult<()> {
    let parent = plan.plan_file.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)
        .map_err(|e| format!("failed to create plan directory '{}': {e}", parent.display()))?;

    let mut file = File::create(&plan.plan_file)
        .map_err(|e| format!("failed to create '{}': {e}", plan.plan_file.display()))?;

    write_pair(&mut file, "version", "2")?;
    write_pair(&mut file, "repo_path", &plan.repo_path)?;
    write_pair(&mut file, "branch", &plan.branch)?;
    write_pair(&mut file, "upstream_short", &plan.upstream_short)?;
    write_pair(&mut file, "remote", &plan.remote)?;
    write_pair(&mut file, "remote_branch", &plan.remote_branch)?;
    write_pair(&mut file, "remote_ref", &plan.remote_ref)?;
    write_pair(&mut file, "remote_url", &plan.remote_url)?;
    write_pair(&mut file, "base_sha", &plan.base_sha)?;
    write_pair(&mut file, "planned_at_epoch", &plan.planned_at_epoch.to_string())?;
    write_pair(&mut file, "seed", &plan.seed.to_string())?;
    write_pair(&mut file, "github_username", &plan.github_username)?;
    write_pair(&mut file, "github_email", &plan.github_email)?;

    for commit in &plan.commits {
        let fields = [
            "commit", &commit.sha,
            &commit.delay_seconds.to_string(), &commit.cumulative_seconds.to_string(),
            &commit.files_changed.to_string(), &commit.binary_files.to_string(),
            &commit.insertions.to_string(), &commit.deletions.to_string(),
            &commit.size_band, &commit.author_name, &commit.author_email,
            &commit.committer_name, &commit.committer_email, &commit.subject,
        ];
        let escaped: Vec<String> = fields.iter().map(|f| escape_field(f)).collect();
        writeln!(file, "{}", escaped.join("\t"))
            .map_err(|e| format!("failed to write plan file: {e}"))?;
    }

    println!("Saved plan file to {}", plan.plan_file.display());
    Ok(())
}

fn write_pair(file: &mut File, key: &str, value: &str) -> AppResult<()> {
    writeln!(file, "{}\t{}", escape_field(key), escape_field(value))
        .map_err(|e| format!("failed to write plan file: {e}"))
}

fn read_plan_file(plan_file: &Path) -> AppResult<Plan> {
    let file = File::open(plan_file)
        .map_err(|e| format!("failed to open '{}': {e}", plan_file.display()))?;
    let reader = BufReader::new(file);

    let mut repo_path = String::new();
    let mut branch = String::new();
    let mut upstream_short = String::new();
    let mut remote = String::new();
    let mut remote_branch = String::new();
    let mut remote_ref = String::new();
    let mut remote_url = String::new();
    let mut base_sha = String::new();
    let mut planned_at_epoch = 0u64;
    let mut seed = 0u64;
    let mut github_username = String::new();
    let mut github_email = String::new();
    let mut commits = Vec::new();

    for raw_line in reader.lines() {
        let line = raw_line.map_err(|e| format!("failed to read plan file: {e}"))?;
        if line.trim().is_empty() { continue; }
        let fields = split_escaped_fields(&line)?;
        if fields.is_empty() { continue; }
        match fields[0].as_str() {
            "version" => {}
            "repo_path" => repo_path = required_field(&fields, 1, "repo_path")?,
            "branch" => branch = required_field(&fields, 1, "branch")?,
            "upstream_short" => upstream_short = required_field(&fields, 1, "upstream_short")?,
            "remote" => remote = required_field(&fields, 1, "remote")?,
            "remote_branch" => remote_branch = required_field(&fields, 1, "remote_branch")?,
            "remote_ref" => remote_ref = required_field(&fields, 1, "remote_ref")?,
            "remote_url" => remote_url = required_field(&fields, 1, "remote_url")?,
            "base_sha" => base_sha = required_field(&fields, 1, "base_sha")?,
            "planned_at_epoch" => {
                planned_at_epoch = required_field(&fields, 1, "planned_at_epoch")?
                    .parse::<u64>().map_err(|_| "invalid planned_at_epoch".to_string())?;
            }
            "seed" => {
                seed = required_field(&fields, 1, "seed")?
                    .parse::<u64>().map_err(|_| "invalid seed".to_string())?;
            }
            "github_username" => github_username = required_field(&fields, 1, "github_username")?,
            "github_email" => github_email = required_field(&fields, 1, "github_email")?,
            "commit" => {
                commits.push(CommitPlan {
                    sha: required_field(&fields, 1, "commit.sha")?,
                    body: String::new(),
                    delay_seconds: required_field(&fields, 2, "commit.delay_seconds")?
                        .parse::<u64>().map_err(|_| "invalid commit delay".to_string())?,
                    cumulative_seconds: required_field(&fields, 3, "commit.cumulative_seconds")?
                        .parse::<u64>().map_err(|_| "invalid commit cumulative time".to_string())?,
                    files_changed: required_field(&fields, 4, "commit.files_changed")?
                        .parse::<u64>().map_err(|_| "invalid commit files_changed".to_string())?,
                    binary_files: required_field(&fields, 5, "commit.binary_files")?
                        .parse::<u64>().map_err(|_| "invalid commit binary_files".to_string())?,
                    insertions: required_field(&fields, 6, "commit.insertions")?
                        .parse::<u64>().map_err(|_| "invalid commit insertions".to_string())?,
                    deletions: required_field(&fields, 7, "commit.deletions")?
                        .parse::<u64>().map_err(|_| "invalid commit deletions".to_string())?,
                    size_band: required_field(&fields, 8, "commit.size_band")?,
                    author_name: required_field(&fields, 9, "commit.author_name")?,
                    author_email: required_field(&fields, 10, "commit.author_email")?,
                    committer_name: required_field(&fields, 11, "commit.committer_name")?,
                    committer_email: required_field(&fields, 12, "commit.committer_email")?,
                    subject: required_field(&fields, 13, "commit.subject")?,
                });
            }
            other => return Err(format!("unknown plan file key '{other}'")),
        }
    }

    if repo_path.is_empty() || branch.is_empty() || remote.is_empty() || base_sha.is_empty() || commits.is_empty() {
        return Err("plan file is missing required fields".to_string());
    }

    Ok(Plan {
        repo_path, branch, upstream_short, remote, remote_branch, remote_ref,
        remote_url, base_sha, planned_at_epoch, seed,
        plan_file: plan_file.to_path_buf(), commits,
        github_username, github_email,
    })
}

fn required_field(fields: &[String], index: usize, name: &str) -> AppResult<String> {
    fields.get(index).cloned()
        .ok_or_else(|| format!("missing field '{}' in plan file", name))
}

fn split_escaped_fields(line: &str) -> AppResult<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let esc = chars.next().ok_or_else(|| "plan file ends with incomplete escape".to_string())?;
                match esc {
                    't' => current.push('\t'),
                    'n' => current.push('\n'),
                    'r' => current.push('\r'),
                    '\\' => current.push('\\'),
                    other => { current.push('\\'); current.push(other); }
                }
            }
            '\t' => { fields.push(current); current = String::new(); }
            other => current.push(other),
        }
    }
    fields.push(current);
    Ok(fields)
}

fn escape_field(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

// ─── plan summary & script rendering ─────────────────────────────────────────

fn print_plan_summary(plan: &Plan) {
    println!("Repository:  {}", plan.repo_path);
    println!("Branch:      {} -> {}", plan.branch, plan.upstream_short);
    println!("Remote URL:  {}", plan.remote_url);
    if !plan.github_username.is_empty() {
        println!("Push account: {} <{}>", plan.github_username, plan.github_email);
    }
    println!("Commits:     {}", plan.commits.len());
    if plan.base_sha == EMPTY_SHA {
        println!("Base commit: (none — fresh push, remote branch '{}/{}' will be created on first push)",
            plan.remote, plan.remote_branch);
    } else {
        println!("Base commit: {}", short_sha(&plan.base_sha));
    }
    println!("Plan seed:   {}", plan.seed);
    println!("Note: existing commit metadata is preserved; this tool does not rewrite history.");

    let authors: BTreeSet<String> = plan.commits.iter()
        .map(|c| format!("{} <{}>", c.author_name, c.author_email))
        .collect();
    let committers: BTreeSet<String> = plan.commits.iter()
        .map(|c| format!("{} <{}>", c.committer_name, c.committer_email))
        .collect();

    println!("Authors:");
    for a in &authors { println!("  - {a}"); }
    println!("Committers:");
    for c in &committers { println!("  - {c}"); }

    println!("Schedule:");
    for (i, commit) in plan.commits.iter().enumerate() {
        let delay_str = if i == 0 && commit.delay_seconds == 0 {
            "immediate".to_string()
        } else {
            format!("wait {}", format_duration(commit.delay_seconds))
        };
        println!(
            "  {}. {} {} | {} | {} files (+{}, -{}) | {} | push at +{}",
            i + 1, short_sha(&commit.sha), commit.subject,
            commit.size_band, commit.files_changed,
            commit.insertions, commit.deletions,
            delay_str, format_duration(commit.cumulative_seconds)
        );
    }

    let total = plan.commits.last().map(|c| c.cumulative_seconds).unwrap_or_default();
    println!("Total runtime: {}", format_duration(total));
    println!("\nRun the script:");
    println!("  push_scheduler script --plan-file {} --shell bash", plan.plan_file.display());
    if !plan.github_username.is_empty() {
        println!("\nScript requires:  export PUSH_SCHEDULER_TOKEN='your-token'");
    }
    println!("\nRun directly (background):");
    if plan.github_username.is_empty() {
        println!("  push_scheduler execute --plan-file {} --yes --background", plan.plan_file.display());
    } else {
        println!("  push_scheduler execute --plan-file {} --yes --background --token YOUR_TOKEN", plan.plan_file.display());
    }
    println!("\nRun via daemon (drop plan in pending dir):");
    println!("  cp {} ~/.config/push_scheduler/pending/", plan.plan_file.display());
}

fn render_bash_script(plan: &Plan) -> AppResult<String> {
    let last_commit = plan.commits.last()
        .ok_or_else(|| "plan file contains no commits".to_string())?;
    let mut out = String::new();
    let uses_account = !plan.github_username.is_empty();

    writeln!(out, "#!/usr/bin/env bash").unwrap();
    writeln!(out, "set -euo pipefail").unwrap();
    writeln!(out).unwrap();

    if uses_account {
        writeln!(out, "# Account: {} <{}>", plan.github_username, plan.github_email).unwrap();
        writeln!(out, "# Set before running: export PUSH_SCHEDULER_TOKEN='your-token'").unwrap();
        writeln!(out, "if [[ -z \"${{PUSH_SCHEDULER_TOKEN:-}}\" ]]; then").unwrap();
        writeln!(out, "  echo \"error: PUSH_SCHEDULER_TOKEN must be set for account '{}'\" >&2", plan.github_username).unwrap();
        writeln!(out, "  echo \"  export PUSH_SCHEDULER_TOKEN='your-github-token'\" >&2").unwrap();
        writeln!(out, "  exit 1").unwrap();
        writeln!(out, "fi").unwrap();
        writeln!(out, "export GIT_TERMINAL_PROMPT=0").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(out, "REPO={}", bash_quote(&plan.repo_path)).unwrap();
    writeln!(out, "BRANCH={}", bash_quote(&plan.branch)).unwrap();
    writeln!(out, "REMOTE_BRANCH={}", bash_quote(&plan.remote_branch)).unwrap();
    writeln!(out, "BASE_SHA={}", bash_quote(&plan.base_sha)).unwrap();
    writeln!(out, "EXPECTED_HEAD={}", bash_quote(&last_commit.sha)).unwrap();

    if uses_account {
        let https_url = ssh_to_https(&plan.remote_url);
        let url_without_creds = if let Some(at) = https_url.find('@') { &https_url[at + 1..] } else { &https_url };
        writeln!(out, "PUSH_REMOTE=\"https://oauth2:${{PUSH_SCHEDULER_TOKEN}}@{}\"", url_without_creds).unwrap();
    } else {
        writeln!(out, "PUSH_REMOTE={}", bash_quote(&plan.remote)).unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "cd \"$REPO\"").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "current_branch=$(git branch --show-current)").unwrap();
    writeln!(out, "if [[ \"$current_branch\" != \"$BRANCH\" ]]; then").unwrap();
    writeln!(out, "  echo \"error: branch '$current_branch' != expected '$BRANCH'\" >&2; exit 1").unwrap();
    writeln!(out, "fi").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "head_sha=$(git rev-parse HEAD)").unwrap();
    writeln!(out, "if [[ \"$head_sha\" != \"$EXPECTED_HEAD\" ]]; then").unwrap();
    writeln!(out, "  echo \"error: HEAD $head_sha != expected $EXPECTED_HEAD; re-plan\" >&2; exit 1").unwrap();
    writeln!(out, "fi").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "check_remote_sha() {{").unwrap();
    writeln!(out, "  local expected=\"$1\"").unwrap();
    writeln!(out, "  local found").unwrap();
    writeln!(out, "  found=$(git ls-remote --heads \"$PUSH_REMOTE\" \"refs/heads/$REMOTE_BRANCH\" | awk 'NR==1 {{print $1}}')").unwrap();
    writeln!(out, "  if [[ \"$expected\" == \"{}\" ]]; then", EMPTY_SHA).unwrap();
    writeln!(out, "    # Fresh push — remote branch must not exist yet").unwrap();
    writeln!(out, "    if [[ -n \"$found\" ]]; then").unwrap();
    writeln!(out, "      echo \"error: expected no remote branch yet but found $found; re-plan\" >&2; exit 1").unwrap();
    writeln!(out, "    fi").unwrap();
    writeln!(out, "    return 0").unwrap();
    writeln!(out, "  fi").unwrap();
    writeln!(out, "  if [[ -z \"$found\" ]]; then").unwrap();
    writeln!(out, "    echo \"error: cannot resolve remote branch $PUSH_REMOTE/$REMOTE_BRANCH\" >&2; exit 1").unwrap();
    writeln!(out, "  fi").unwrap();
    writeln!(out, "  if [[ \"$found\" != \"$expected\" ]]; then").unwrap();
    writeln!(out, "    echo \"error: remote drifted; expected $expected found $found; re-plan\" >&2; exit 1").unwrap();
    writeln!(out, "  fi").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    let mut expected_remote_sha = plan.base_sha.clone();
    for (i, commit) in plan.commits.iter().enumerate() {
        writeln!(out, "# Push {} of {}: {} {}", i + 1, plan.commits.len(), short_sha(&commit.sha), commit.subject).unwrap();
        if commit.delay_seconds > 0 {
            writeln!(out, "sleep {}", commit.delay_seconds).unwrap();
        }
        writeln!(out, "check_remote_sha {}", bash_quote(&expected_remote_sha)).unwrap();
        writeln!(out, "git push \"$PUSH_REMOTE\" {}:\"refs/heads/$REMOTE_BRANCH\"", bash_quote(&commit.sha)).unwrap();
        writeln!(out, "echo \"Pushed {}/{}: {}\"", i + 1, plan.commits.len(), commit.subject).unwrap();
        writeln!(out).unwrap();
        expected_remote_sha = commit.sha.clone();
    }

    writeln!(out, "echo \"All {} commits pushed.\"", plan.commits.len()).unwrap();
    Ok(out)
}

fn render_powershell_script(plan: &Plan) -> AppResult<String> {
    let last_commit = plan.commits.last()
        .ok_or_else(|| "plan file contains no commits".to_string())?;
    let mut out = String::new();
    let uses_account = !plan.github_username.is_empty();

    writeln!(out, "$ErrorActionPreference = 'Stop'").unwrap();
    writeln!(out).unwrap();

    if uses_account {
        writeln!(out, "# Account: {} <{}>", plan.github_username, plan.github_email).unwrap();
        writeln!(out, "# Set before running: $env:PUSH_SCHEDULER_TOKEN = 'your-token'").unwrap();
        writeln!(out, "if (-not $env:PUSH_SCHEDULER_TOKEN) {{").unwrap();
        writeln!(out, "    throw \"PUSH_SCHEDULER_TOKEN must be set for account '{}'\"", plan.github_username).unwrap();
        writeln!(out, "}}").unwrap();
        writeln!(out, "$env:GIT_TERMINAL_PROMPT = '0'").unwrap();
        writeln!(out).unwrap();
    }

    writeln!(out, "$Repo = {}", powershell_quote(&plan.repo_path)).unwrap();
    writeln!(out, "$Branch = {}", powershell_quote(&plan.branch)).unwrap();
    writeln!(out, "$RemoteBranch = {}", powershell_quote(&plan.remote_branch)).unwrap();
    writeln!(out, "$BaseSha = {}", powershell_quote(&plan.base_sha)).unwrap();
    writeln!(out, "$ExpectedHead = {}", powershell_quote(&last_commit.sha)).unwrap();

    if uses_account {
        let https_url = ssh_to_https(&plan.remote_url);
        let url_without_creds = if let Some(at) = https_url.find('@') { &https_url[at + 1..] } else { &https_url };
        writeln!(out, "$PushRemote = \"https://oauth2:$($env:PUSH_SCHEDULER_TOKEN)@{}\"", url_without_creds).unwrap();
    } else {
        writeln!(out, "$PushRemote = {}", powershell_quote(&plan.remote)).unwrap();
    }
    writeln!(out).unwrap();
    writeln!(out, "Set-Location -Path $Repo").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "$currentBranch = (git branch --show-current).Trim()").unwrap();
    writeln!(out, "if ($currentBranch -ne $Branch) {{ throw \"branch '$currentBranch' != expected '$Branch'\" }}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "$headSha = (git rev-parse HEAD).Trim()").unwrap();
    writeln!(out, "if ($headSha -ne $ExpectedHead) {{ throw \"HEAD $headSha != expected $ExpectedHead; re-plan\" }}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "function Check-RemoteSha([string]$Expected) {{").unwrap();
    writeln!(out, "    $line = git ls-remote --heads $PushRemote \"refs/heads/$RemoteBranch\" | Select-Object -First 1").unwrap();
    writeln!(out, "    if ($Expected -eq '{}') {{", EMPTY_SHA).unwrap();
    writeln!(out, "        if ($line) {{ throw \"expected no remote branch yet but found $line; re-plan\" }}").unwrap();
    writeln!(out, "        return").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "    if (-not $line) {{ throw \"cannot resolve remote branch $PushRemote/$RemoteBranch\" }}").unwrap();
    writeln!(out, "    $found = ($line -split '\\s+')[0]").unwrap();
    writeln!(out, "    if ($found -ne $Expected) {{ throw \"remote drifted; expected $Expected found $found; re-plan\" }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    let mut expected_remote_sha = plan.base_sha.clone();
    for (i, commit) in plan.commits.iter().enumerate() {
        writeln!(out, "# Push {} of {}: {} {}", i + 1, plan.commits.len(), short_sha(&commit.sha), commit.subject).unwrap();
        if commit.delay_seconds > 0 {
            writeln!(out, "Start-Sleep -Seconds {}", commit.delay_seconds).unwrap();
        }
        writeln!(out, "Check-RemoteSha {}", powershell_quote(&expected_remote_sha)).unwrap();
        writeln!(out, "git push $PushRemote {}:\"refs/heads/$RemoteBranch\"", powershell_quote(&commit.sha)).unwrap();
        writeln!(out, "Write-Host \"Pushed {}/{}: {}\"", i + 1, plan.commits.len(), commit.subject).unwrap();
        writeln!(out).unwrap();
        expected_remote_sha = commit.sha.clone();
    }

    writeln!(out, "Write-Host \"All {} commits pushed.\"", plan.commits.len()).unwrap();
    Ok(out)
}

// ─── formatting helpers ───────────────────────────────────────────────────────

fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn remote_head_sha(repo_path: &Path, remote: &str, remote_branch: &str) -> AppResult<String> {
    let remote_ref = format!("refs/heads/{remote_branch}");
    let output = git(repo_path, &["ls-remote", "--heads", remote, &remote_ref])?;
    let sha = output.split_whitespace().next().unwrap_or("").trim().to_string();
    if sha.is_empty() {
        return Err(format!("failed to resolve remote branch head for {}/{}", remote, remote_branch));
    }
    Ok(sha)
}

fn remote_head_sha_with_retry(repo_path: &Path, remote: &str, remote_branch: &str) -> AppResult<String> {
    retry(NETWORK_RETRY_ATTEMPTS, || remote_head_sha(repo_path, remote, remote_branch), "remote branch check")
}

fn remote_head_sha_auth(repo_path: &Path, auth_url: &str, remote_branch: &str, token: &str) -> AppResult<String> {
    let remote_ref = format!("refs/heads/{remote_branch}");
    let output = git_env(repo_path, &["ls-remote", "--heads", auth_url, &remote_ref], &[("GIT_TERMINAL_PROMPT", "0")])
        .map_err(|e| redact_token(&e, token))?;
    let sha = output.split_whitespace().next().unwrap_or("").trim().to_string();
    if sha.is_empty() {
        return Err(format!("failed to resolve remote branch head for {}", remote_branch));
    }
    Ok(sha)
}

fn remote_head_sha_auth_with_retry(repo_path: &Path, auth_url: &str, remote_branch: &str, token: &str) -> AppResult<String> {
    retry(NETWORK_RETRY_ATTEMPTS, || remote_head_sha_auth(repo_path, auth_url, remote_branch, token), "authenticated remote branch check")
}

fn git_with_retry(repo_path: &Path, args: &[&str], action: &str) -> AppResult<String> {
    retry(NETWORK_RETRY_ATTEMPTS, || git(repo_path, args), action)
}

fn git_with_retry_env(repo_path: &Path, args: &[&str], action: &str, env: &[(&str, &str)], token: &str) -> AppResult<String> {
    retry(
        NETWORK_RETRY_ATTEMPTS,
        || git_env(repo_path, args, env).map_err(|e| redact_token(&e, token)),
        action,
    )
}

fn retry<F>(attempts: u32, mut f: F, action: &str) -> AppResult<String>
where
    F: FnMut() -> AppResult<String>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= attempts || !is_transient_network_error(&e) {
                    return Err(e);
                }
                let delay = NETWORK_RETRY_BASE_DELAY_SECONDS * (1u64 << (attempt - 1));
                eprintln!(
                    "warning: {action} transient error; retrying in {} (attempt {}/{})",
                    format_duration(delay), attempt + 1, attempts
                );
                thread::sleep(Duration::from_secs(delay));
            }
        }
    }
}

fn git(repo_path: &Path, args: &[&str]) -> AppResult<String> {
    git_env(repo_path, args, &[])
}

fn git_env(repo_path: &Path, args: &[&str], env: &[(&str, &str)]) -> AppResult<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo_path);
    for (key, val) in env { cmd.env(key, val); }

    let output = cmd.output()
        .map_err(|e| format!("failed to run git {:?}: {e}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("git {:?} failed: {}", args, detail));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_transient_network_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    [
        "could not resolve host", "temporary failure in name resolution",
        "network is unreachable", "connection timed out", "timed out",
        "connection reset by peer", "failed to connect", "unable to access",
    ]
    .iter().any(|needle| lower.contains(needle))
}

// ─── utility ─────────────────────────────────────────────────────────────────

fn now_epoch_seconds() -> AppResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| format!("system time error: {e}"))
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn format_duration(seconds: u64) -> String {
    if seconds == 0 { return "0s".to_string(); }
    if seconds < 60 { return format!("{seconds}s"); }

    let secs = seconds % 60;
    let total_minutes = seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;

    let mut parts = Vec::new();
    if hours > 0 { parts.push(format!("{hours}h")); }
    if minutes > 0 { parts.push(format!("{minutes}m")); }
    if secs > 0 { parts.push(format!("{secs}s")); }
    parts.join(" ")
}

// ─── PRNG ─────────────────────────────────────────────────────────────────────

struct XorShift64 { state: u64 }

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 { 0x9E3779B97F4A7C15 } else { seed };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut v = self.state;
        v ^= v << 13; v ^= v >> 7; v ^= v << 17;
        self.state = v; v
    }

    fn range_inclusive(&mut self, start: u64, end: u64) -> u64 {
        if start >= end { return start; }
        start + (self.next_u64() % (end - start + 1))
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        auto_commit_message, bash_quote, build_authenticated_url, contains_marker,
        escape_field, format_duration, normalize_marker_text, parse_numstat,
        powershell_quote, split_escaped_fields, ssh_to_https,
    };

    #[test]
    fn escapes_round_trip() {
        let fields = vec![
            "commit".to_string(),
            "hello\tworld".to_string(),
            "line1\nline2".to_string(),
            "slash\\value".to_string(),
        ];
        let encoded = fields.iter().map(|f| escape_field(f)).collect::<Vec<_>>().join("\t");
        let decoded = split_escaped_fields(&encoded).expect("decode");
        assert_eq!(decoded, fields);
    }

    #[test]
    fn bash_quote_escapes_single_quotes() {
        assert_eq!(bash_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn powershell_quote_escapes_single_quotes() {
        assert_eq!(powershell_quote("a'b"), "'a''b'");
    }

    #[test]
    fn normalizes_marker_text() {
        assert_eq!(normalize_marker_text("OpenAI/Codex"), "openai codex");
    }

    #[test]
    fn detects_markers_on_word_boundaries() {
        let markers = ["openai", "gpt"];
        assert_eq!(contains_marker("Generated with OpenAI tooling", &markers), Some("openai".to_string()));
        assert_eq!(contains_marker("adapt", &markers), None);
    }

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(60 * 17), "17m");
        assert_eq!(format_duration(60 * 60 * 2), "2h");
        assert_eq!(format_duration((60 * 60 * 2) + (60 * 7) + 13), "2h 7m 13s");
    }

    #[test]
    fn ssh_url_converted_to_https() {
        assert_eq!(
            ssh_to_https("git@github.com:octocat/hello-world.git"),
            "https://github.com/octocat/hello-world.git"
        );
        assert_eq!(
            ssh_to_https("https://github.com/octocat/hello-world.git"),
            "https://github.com/octocat/hello-world.git"
        );
    }

    #[test]
    fn authenticated_url_built_correctly() {
        let url = build_authenticated_url(
            "https://github.com/octocat/hello-world.git",
            "octocat",
            "ghp_testtoken",
        )
        .unwrap();
        assert_eq!(url, "https://oauth2:ghp_testtoken@github.com/octocat/hello-world.git");
    }

    #[test]
    fn authenticated_url_from_ssh() {
        let url = build_authenticated_url(
            "git@github.com:octocat/hello-world.git",
            "octocat",
            "ghp_testtoken",
        )
        .unwrap();
        assert_eq!(url, "https://oauth2:ghp_testtoken@github.com/octocat/hello-world.git");
    }

    #[test]
    fn numstat_parsed() {
        let input = "10\t3\tsrc/main.rs\n-\t-\tassets/logo.png\n";
        let files = parse_numstat(input).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].insertions, 10);
        assert_eq!(files[0].deletions, 3);
        assert_eq!(files[1].insertions, 0);
        assert_eq!(files[1].deletions, 0);
    }

    #[test]
    fn auto_message_add_verb() {
        use super::NumstatFile;
        let files = vec![NumstatFile { path: "src/auth/login.rs".to_string(), insertions: 50, deletions: 2 }];
        let refs: Vec<&NumstatFile> = files.iter().collect();
        let msg = auto_commit_message(&refs, None);
        assert!(msg.starts_with("Add"), "expected Add prefix, got: {msg}");
    }
}
