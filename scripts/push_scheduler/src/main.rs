use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, String>;

const DEFAULT_PLAN_FILE: &str = "/tmp/git-push-scheduler.plan";
const DEFAULT_MIN_DELAY_MINUTES: u64 = 12;
const DEFAULT_MAX_DELAY_MINUTES: u64 = 90;
const NETWORK_RETRY_ATTEMPTS: u32 = 5;
const NETWORK_RETRY_BASE_DELAY_SECONDS: u64 = 2;

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
    }
    Ok(())
}

enum Mode {
    Plan(PlanArgs),
    Execute(ExecuteArgs),
}

struct PlanArgs {
    repo: PathBuf,
    plan_file: PathBuf,
    min_delay_minutes: u64,
    max_delay_minutes: u64,
    seed: Option<u64>,
}

struct ExecuteArgs {
    plan_file: PathBuf,
    yes: bool,
}

#[derive(Clone)]
struct Plan {
    repo_path: String,
    branch: String,
    upstream_short: String,
    remote: String,
    remote_branch: String,
    remote_ref: String,
    base_sha: String,
    planned_at_epoch: u64,
    seed: u64,
    plan_file: PathBuf,
    commits: Vec<CommitPlan>,
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

fn parse_cli(args: Vec<String>) -> AppResult<Mode> {
    if args.len() < 2 {
        return Err(usage());
    }

    match args[1].as_str() {
        "plan" => parse_plan_args(&args[2..]).map(Mode::Plan),
        "execute" => parse_execute_args(&args[2..]).map(Mode::Execute),
        "--help" | "-h" | "help" => Err(usage()),
        other => Err(format!("unknown subcommand '{other}'\n\n{}", usage())),
    }
}

fn parse_plan_args(args: &[String]) -> AppResult<PlanArgs> {
    let mut repo = PathBuf::from(".");
    let mut plan_file = PathBuf::from(DEFAULT_PLAN_FILE);
    let mut min_delay_minutes = DEFAULT_MIN_DELAY_MINUTES;
    let mut max_delay_minutes = DEFAULT_MAX_DELAY_MINUTES;
    let mut seed = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                repo = PathBuf::from(args.get(index).ok_or("--repo requires a path")?);
            }
            "--plan-file" => {
                index += 1;
                plan_file = PathBuf::from(args.get(index).ok_or("--plan-file requires a path")?);
            }
            "--min-delay-minutes" => {
                index += 1;
                min_delay_minutes = parse_u64(args.get(index), "--min-delay-minutes")?;
            }
            "--max-delay-minutes" => {
                index += 1;
                max_delay_minutes = parse_u64(args.get(index), "--max-delay-minutes")?;
            }
            "--seed" => {
                index += 1;
                seed = Some(parse_u64(args.get(index), "--seed")?);
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown plan option '{other}'\n\n{}", usage())),
        }
        index += 1;
    }

    if min_delay_minutes > max_delay_minutes {
        return Err("--min-delay-minutes cannot be greater than --max-delay-minutes".to_string());
    }

    Ok(PlanArgs {
        repo,
        plan_file,
        min_delay_minutes,
        max_delay_minutes,
        seed,
    })
}

fn parse_execute_args(args: &[String]) -> AppResult<ExecuteArgs> {
    let mut plan_file = PathBuf::from(DEFAULT_PLAN_FILE);
    let mut yes = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--plan-file" => {
                index += 1;
                plan_file = PathBuf::from(args.get(index).ok_or("--plan-file requires a path")?);
            }
            "--yes" => yes = true,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown execute option '{other}'\n\n{}", usage())),
        }
        index += 1;
    }

    Ok(ExecuteArgs { plan_file, yes })
}

fn usage() -> String {
    format!(
        "push_scheduler plan [--repo PATH] [--plan-file PATH] [--min-delay-minutes N] [--max-delay-minutes N] [--seed N]\n\
         push_scheduler execute [--plan-file PATH] --yes\n\n\
         Defaults:\n\
           plan file: {DEFAULT_PLAN_FILE}\n\
           min delay minutes: {DEFAULT_MIN_DELAY_MINUTES}\n\
           max delay minutes: {DEFAULT_MAX_DELAY_MINUTES}"
    )
}

fn parse_u64(value: Option<&String>, flag_name: &str) -> AppResult<u64> {
    value
        .ok_or_else(|| format!("{flag_name} requires a numeric value"))?
        .parse::<u64>()
        .map_err(|_| format!("{flag_name} requires a numeric value"))
}

fn build_plan(args: PlanArgs) -> AppResult<Plan> {
    let repo_path = canonical_repo_path(&args.repo)?;
    let branch = git(&repo_path, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err("HEAD is detached; switch to a branch with a tracked upstream first".to_string());
    }

    let upstream = upstream_info(&repo_path, &branch)?;
    let base_sha = git(&repo_path, &["rev-parse", "@{upstream}"])?;

    let rev_list = git(&repo_path, &["rev-list", "--reverse", "@{upstream}..HEAD"])?;
    let pending_shas: Vec<String> = rev_list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if pending_shas.is_empty() {
        return Err("no commits are ahead of the tracked upstream branch".to_string());
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

    let planned_at_epoch = now_epoch_seconds()?;
    let seed = args.seed.unwrap_or_else(|| planned_at_epoch ^ (commits.len() as u64).wrapping_mul(7919));
    let mut rng = XorShift64::new(seed);

    let mut cumulative_seconds = 0;
    for (index, commit) in commits.iter_mut().enumerate() {
        let (size_band, delay_seconds) = if index == 0 {
            (classify_commit(commit).0.to_string(), 0)
        } else {
            let (band, min_minutes, max_minutes) = classify_commit(commit);
            let raw_delay = rng.range_inclusive(min_minutes, max_minutes);
            let delay_minutes = raw_delay.clamp(args.min_delay_minutes, args.max_delay_minutes);
            (band.to_string(), delay_minutes * 60)
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
        base_sha,
        planned_at_epoch,
        seed,
        plan_file: args.plan_file,
        commits,
    })
}

fn execute_plan(args: ExecuteArgs) -> AppResult<()> {
    if !args.yes {
        return Err("execution requires --yes so the plan cannot run accidentally".to_string());
    }

    let plan = read_plan_file(&args.plan_file)?;
    let repo_path = canonical_repo_path(Path::new(&plan.repo_path))?;
    let current_branch = git(&repo_path, &["branch", "--show-current"])?;
    if current_branch != plan.branch {
        return Err(format!(
            "current branch is '{}' but the saved plan targets '{}'; re-plan or switch back first",
            current_branch, plan.branch
        ));
    }

    let last_commit = plan
        .commits
        .last()
        .ok_or_else(|| "plan file contains no commits".to_string())?;
    let head_sha = git(&repo_path, &["rev-parse", "HEAD"])?;
    if head_sha != last_commit.sha {
        return Err(format!(
            "HEAD is {} but the saved plan expected {}; local history changed, so re-plan",
            short_sha(&head_sha),
            short_sha(&last_commit.sha)
        ));
    }

    let fetch_refspec = format!(
        "{}:refs/remotes/{}/{}",
        plan.remote_ref, plan.remote, plan.remote_branch
    );
    git_with_retry(
        &repo_path,
        &["fetch", "--quiet", &plan.remote, &fetch_refspec],
        "fetch remote branch state",
    )?;
    let remote_tracking_ref = format!("refs/remotes/{}/{}", plan.remote, plan.remote_branch);
    let fetched_remote_sha = git_with_retry(
        &repo_path,
        &["rev-parse", &remote_tracking_ref],
        "read fetched remote branch state",
    )?;
    if fetched_remote_sha != plan.base_sha {
        return Err(format!(
            "remote branch moved from {} to {} since the plan was created; re-plan before pushing",
            short_sha(&plan.base_sha),
            short_sha(&fetched_remote_sha)
        ));
    }

    println!(
        "Executing plan for {} commits on {} -> {}",
        plan.commits.len(),
        plan.branch,
        plan.upstream_short
    );

    for (index, commit) in plan.commits.iter().enumerate() {
        if index > 0 && commit.delay_seconds > 0 {
            println!(
                "Waiting {} before push {}/{} ({})",
                format_duration(commit.delay_seconds),
                index + 1,
                plan.commits.len(),
                short_sha(&commit.sha)
            );
            thread::sleep(Duration::from_secs(commit.delay_seconds));
        }

        let expected_remote_sha = if index == 0 {
            &plan.base_sha
        } else {
            &plan.commits[index - 1].sha
        };
        let remote_sha = remote_head_sha_with_retry(&repo_path, &plan.remote, &plan.remote_branch)?;
        if remote_sha != *expected_remote_sha {
            return Err(format!(
                "remote branch drifted before push {}/{}: expected {}, found {}; re-plan",
                index + 1,
                plan.commits.len(),
                short_sha(expected_remote_sha),
                short_sha(&remote_sha)
            ));
        }

        println!(
            "Pushing {}/{} {} {}",
            index + 1,
            plan.commits.len(),
            short_sha(&commit.sha),
            commit.subject
        );
        let remote_ref = format!("refs/heads/{}", plan.remote_branch);
        git_with_retry(
            &repo_path,
            &["push", &plan.remote, &format!("{}:{}", commit.sha, remote_ref)],
            "push commit",
        )?;
    }

    println!(
        "Finished staggered push schedule in {}",
        format_duration(
            plan.commits
                .last()
                .map(|commit| commit.cumulative_seconds)
                .unwrap_or_default()
        )
    );
    Ok(())
}

fn canonical_repo_path(path: &Path) -> AppResult<PathBuf> {
    let path = if path.exists() {
        path.to_path_buf()
    } else {
        return Err(format!("repository path '{}' does not exist", path.display()));
    };

    let top_level = git(&path, &["rev-parse", "--show-toplevel"])?;
    fs::canonicalize(top_level.trim())
        .map_err(|error| format!("failed to resolve repository path: {error}"))
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
            "branch '{}' does not have a tracked upstream; configure one before using this skill",
            branch
        ));
    }

    let remote_branch = remote_ref
        .strip_prefix("refs/heads/")
        .ok_or_else(|| format!("unsupported upstream ref '{}'", remote_ref))?
        .to_string();

    Ok(UpstreamInfo {
        short_name,
        remote,
        remote_ref,
        remote_branch,
    })
}

fn collect_commit(repo_path: &Path, sha: &str) -> AppResult<CommitPlan> {
    let metadata = git(
        repo_path,
        &[
            "show",
            "-s",
            "--format=%H%x1f%an%x1f%ae%x1f%cn%x1f%ce%x1f%s%x1f%B",
            sha,
        ],
    )?;

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
    let mut files_changed = 0;
    let mut binary_files = 0;
    let mut insertions = 0;
    let mut deletions = 0;

    for line in numstat.lines().filter(|line| !line.trim().is_empty()) {
        let columns: Vec<&str> = line.split('\t').collect();
        if columns.len() < 3 {
            continue;
        }
        files_changed += 1;
        let additions_field = columns[0].trim();
        let deletions_field = columns[1].trim();
        if additions_field == "-" || deletions_field == "-" {
            binary_files += 1;
        }
        if additions_field != "-" {
            insertions += additions_field.parse::<u64>().unwrap_or_default();
        }
        if deletions_field != "-" {
            deletions += deletions_field.parse::<u64>().unwrap_or_default();
        }
    }

    Ok(CommitPlan {
        sha: commit_sha,
        subject,
        body,
        author_name,
        author_email,
        committer_name,
        committer_email,
        files_changed,
        binary_files,
        insertions,
        deletions,
        size_band: String::new(),
        delay_seconds: 0,
        cumulative_seconds: 0,
    })
}

fn classify_commit(commit: &CommitPlan) -> (&'static str, u64, u64) {
    let score = commit.insertions + commit.deletions + (commit.files_changed * 12) + (commit.binary_files * 30);
    match score {
        0..=40 => ("small", 12, 22),
        41..=140 => ("medium", 18, 35),
        141..=360 => ("large", 30, 55),
        _ => ("very-large", 45, 85),
    }
}

fn find_marker_issues(commit: &CommitPlan) -> Vec<MarkerIssue> {
    let marker_phrases = [
        "codex",
        "copilot",
        "claude",
        "chatgpt",
        "gpt",
        "openai",
        "anthropic",
        "llm",
        "ai generated",
        "ai-generated",
        "ai assisted",
        "ai-assisted",
        "generated by ai",
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
            issues.push(MarkerIssue {
                sha: commit.sha.clone(),
                field,
                marker,
            });
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
    let mut previous_was_space = true;
    for character in value.chars().flat_map(|character| character.to_lowercase()) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_space = false;
        } else if !previous_was_space {
            output.push(' ');
            previous_was_space = true;
        }
    }
    output.trim().to_string()
}

fn format_marker_issues(issues: &[MarkerIssue]) -> String {
    let mut lines = vec![
        "commit metadata audit failed; the scheduler will not rewrite history or conceal markers".to_string(),
    ];
    for issue in issues {
        lines.push(format!(
            "  {} field '{}' matched '{}'",
            short_sha(&issue.sha),
            issue.field,
            issue.marker
        ));
    }
    lines.join("\n")
}

fn write_plan_file(plan: &Plan) -> AppResult<()> {
    let parent = plan
        .plan_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create plan directory '{}': {error}", parent.display()))?;

    let mut file = File::create(&plan.plan_file)
        .map_err(|error| format!("failed to create '{}': {error}", plan.plan_file.display()))?;
    write_pair(&mut file, "version", "1")?;
    write_pair(&mut file, "repo_path", &plan.repo_path)?;
    write_pair(&mut file, "branch", &plan.branch)?;
    write_pair(&mut file, "upstream_short", &plan.upstream_short)?;
    write_pair(&mut file, "remote", &plan.remote)?;
    write_pair(&mut file, "remote_branch", &plan.remote_branch)?;
    write_pair(&mut file, "remote_ref", &plan.remote_ref)?;
    write_pair(&mut file, "base_sha", &plan.base_sha)?;
    write_pair(&mut file, "planned_at_epoch", &plan.planned_at_epoch.to_string())?;
    write_pair(&mut file, "seed", &plan.seed.to_string())?;

    for commit in &plan.commits {
        let fields = [
            "commit",
            &commit.sha,
            &commit.delay_seconds.to_string(),
            &commit.cumulative_seconds.to_string(),
            &commit.files_changed.to_string(),
            &commit.binary_files.to_string(),
            &commit.insertions.to_string(),
            &commit.deletions.to_string(),
            &commit.size_band,
            &commit.author_name,
            &commit.author_email,
            &commit.committer_name,
            &commit.committer_email,
            &commit.subject,
        ];
        let escaped: Vec<String> = fields.iter().map(|field| escape_field(field)).collect();
        writeln!(file, "{}", escaped.join("\t"))
            .map_err(|error| format!("failed to write plan file: {error}"))?;
    }

    println!("Saved plan file to {}", plan.plan_file.display());
    Ok(())
}

fn write_pair(file: &mut File, key: &str, value: &str) -> AppResult<()> {
    writeln!(file, "{}\t{}", escape_field(key), escape_field(value))
        .map_err(|error| format!("failed to write plan file: {error}"))
}

fn read_plan_file(plan_file: &Path) -> AppResult<Plan> {
    let file = File::open(plan_file)
        .map_err(|error| format!("failed to open '{}': {error}", plan_file.display()))?;
    let reader = BufReader::new(file);

    let mut repo_path = String::new();
    let mut branch = String::new();
    let mut upstream_short = String::new();
    let mut remote = String::new();
    let mut remote_branch = String::new();
    let mut remote_ref = String::new();
    let mut base_sha = String::new();
    let mut planned_at_epoch = 0;
    let mut seed = 0;
    let mut commits = Vec::new();

    for raw_line in reader.lines() {
        let line = raw_line.map_err(|error| format!("failed to read plan file: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_escaped_fields(&line)?;
        if fields.is_empty() {
            continue;
        }
        match fields[0].as_str() {
            "version" => {}
            "repo_path" => repo_path = required_field(&fields, 1, "repo_path")?,
            "branch" => branch = required_field(&fields, 1, "branch")?,
            "upstream_short" => upstream_short = required_field(&fields, 1, "upstream_short")?,
            "remote" => remote = required_field(&fields, 1, "remote")?,
            "remote_branch" => remote_branch = required_field(&fields, 1, "remote_branch")?,
            "remote_ref" => remote_ref = required_field(&fields, 1, "remote_ref")?,
            "base_sha" => base_sha = required_field(&fields, 1, "base_sha")?,
            "planned_at_epoch" => {
                planned_at_epoch = required_field(&fields, 1, "planned_at_epoch")?
                    .parse::<u64>()
                    .map_err(|_| "invalid planned_at_epoch in plan file".to_string())?;
            }
            "seed" => {
                seed = required_field(&fields, 1, "seed")?
                    .parse::<u64>()
                    .map_err(|_| "invalid seed in plan file".to_string())?;
            }
            "commit" => {
                commits.push(CommitPlan {
                    sha: required_field(&fields, 1, "commit.sha")?,
                    body: String::new(),
                    delay_seconds: required_field(&fields, 2, "commit.delay_seconds")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit delay in plan file".to_string())?,
                    cumulative_seconds: required_field(&fields, 3, "commit.cumulative_seconds")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit cumulative time in plan file".to_string())?,
                    files_changed: required_field(&fields, 4, "commit.files_changed")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit files_changed in plan file".to_string())?,
                    binary_files: required_field(&fields, 5, "commit.binary_files")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit binary_files in plan file".to_string())?,
                    insertions: required_field(&fields, 6, "commit.insertions")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit insertions in plan file".to_string())?,
                    deletions: required_field(&fields, 7, "commit.deletions")?
                        .parse::<u64>()
                        .map_err(|_| "invalid commit deletions in plan file".to_string())?,
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
        repo_path,
        branch,
        upstream_short,
        remote,
        remote_branch,
        remote_ref,
        base_sha,
        planned_at_epoch,
        seed,
        plan_file: plan_file.to_path_buf(),
        commits,
    })
}

fn required_field(fields: &[String], index: usize, name: &str) -> AppResult<String> {
    fields
        .get(index)
        .cloned()
        .ok_or_else(|| format!("missing field '{}' in plan file", name))
}

fn split_escaped_fields(line: &str) -> AppResult<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                let escaped = chars
                    .next()
                    .ok_or_else(|| "plan file ends with an incomplete escape sequence".to_string())?;
                match escaped {
                    't' => current.push('\t'),
                    'n' => current.push('\n'),
                    'r' => current.push('\r'),
                    '\\' => current.push('\\'),
                    other => {
                        current.push('\\');
                        current.push(other);
                    }
                }
            }
            '\t' => {
                fields.push(current);
                current = String::new();
            }
            other => current.push(other),
        }
    }
    fields.push(current);
    Ok(fields)
}

fn escape_field(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn print_plan_summary(plan: &Plan) {
    println!("Repository: {}", plan.repo_path);
    println!("Branch: {} -> {}", plan.branch, plan.upstream_short);
    println!("Pending commits: {}", plan.commits.len());
    println!("Base remote commit: {}", short_sha(&plan.base_sha));
    println!("Plan seed: {}", plan.seed);
    println!("Planner timestamp: {}", plan.planned_at_epoch);
    println!("Push behavior: existing commit metadata is preserved; this tool does not rewrite history.");

    let authors: BTreeSet<String> = plan
        .commits
        .iter()
        .map(|commit| format!("{} <{}>", commit.author_name, commit.author_email))
        .collect();
    let committers: BTreeSet<String> = plan
        .commits
        .iter()
        .map(|commit| format!("{} <{}>", commit.committer_name, commit.committer_email))
        .collect();

    println!("Authors:");
    for author in authors {
        println!("  - {author}");
    }

    println!("Committers:");
    for committer in committers {
        println!("  - {committer}");
    }

    println!("Schedule:");
    for (index, commit) in plan.commits.iter().enumerate() {
        let delay = if index == 0 {
            "immediate".to_string()
        } else {
            format_duration(commit.delay_seconds)
        };
        println!(
            "  {}. {} {} | {} | {} files (+{}, -{}) | delay {} | push at +{} | author {} <{}>",
            index + 1,
            short_sha(&commit.sha),
            commit.subject,
            commit.size_band,
            commit.files_changed,
            commit.insertions,
            commit.deletions,
            delay,
            format_duration(commit.cumulative_seconds),
            commit.author_name,
            commit.author_email
        );
    }

    let total_seconds = plan
        .commits
        .last()
        .map(|commit| commit.cumulative_seconds)
        .unwrap_or_default();
    println!("Total runtime: {}", format_duration(total_seconds));
}

fn remote_head_sha(repo_path: &Path, remote: &str, remote_branch: &str) -> AppResult<String> {
    let remote_ref = format!("refs/heads/{remote_branch}");
    let output = git(repo_path, &["ls-remote", "--heads", remote, &remote_ref])?;
    let sha = output.split_whitespace().next().unwrap_or("").trim().to_string();
    if sha.is_empty() {
        return Err(format!(
            "failed to resolve remote branch head for {}/{}",
            remote, remote_branch
        ));
    }
    Ok(sha)
}

fn remote_head_sha_with_retry(repo_path: &Path, remote: &str, remote_branch: &str) -> AppResult<String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match remote_head_sha(repo_path, remote, remote_branch) {
            Ok(sha) => return Ok(sha),
            Err(error) => {
                if attempt >= NETWORK_RETRY_ATTEMPTS || !is_transient_network_error(&error) {
                    return Err(error);
                }

                let delay_seconds = NETWORK_RETRY_BASE_DELAY_SECONDS * (1u64 << (attempt - 1));
                eprintln!(
                    "warning: remote branch check failed with transient network error; retrying in {} (attempt {}/{})",
                    format_duration(delay_seconds),
                    attempt + 1,
                    NETWORK_RETRY_ATTEMPTS
                );
                thread::sleep(Duration::from_secs(delay_seconds));
            }
        }
    }
}

fn git_with_retry(repo_path: &Path, args: &[&str], action: &str) -> AppResult<String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match git(repo_path, args) {
            Ok(output) => return Ok(output),
            Err(error) => {
                if attempt >= NETWORK_RETRY_ATTEMPTS || !is_transient_network_error(&error) {
                    return Err(error);
                }

                let delay_seconds = NETWORK_RETRY_BASE_DELAY_SECONDS * (1u64 << (attempt - 1));
                eprintln!(
                    "warning: {action} failed with transient network error; retrying in {} (attempt {}/{})",
                    format_duration(delay_seconds),
                    attempt + 1,
                    NETWORK_RETRY_ATTEMPTS
                );
                thread::sleep(Duration::from_secs(delay_seconds));
            }
        }
    }
}

fn git(repo_path: &Path, args: &[&str]) -> AppResult<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .map_err(|error| format!("failed to run git {:?}: {error}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("git {:?} failed: {}", args, detail));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_transient_network_error(error: &str) -> bool {
    let normalized = error.to_lowercase();
    [
        "could not resolve host",
        "temporary failure in name resolution",
        "network is unreachable",
        "connection timed out",
        "timed out",
        "connection reset by peer",
        "failed to connect",
        "unable to access",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn now_epoch_seconds() -> AppResult<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system time error: {error}"))?
        .as_secs())
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "0m".to_string();
    }

    let total_minutes = seconds / 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours == 0 {
        format!("{minutes}m")
    } else if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {minutes}m")
    }
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 { 0x9E3779B97F4A7C15 } else { seed };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn range_inclusive(&mut self, start: u64, end: u64) -> u64 {
        if start >= end {
            return start;
        }
        start + (self.next_u64() % (end - start + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_marker, escape_field, format_duration, normalize_marker_text, split_escaped_fields};

    #[test]
    fn escapes_round_trip() {
        let fields = vec![
            "commit".to_string(),
            "hello\tworld".to_string(),
            "line1\nline2".to_string(),
            "slash\\value".to_string(),
        ];
        let encoded = fields
            .iter()
            .map(|field| escape_field(field))
            .collect::<Vec<_>>()
            .join("\t");
        let decoded = split_escaped_fields(&encoded).expect("decode");
        assert_eq!(decoded, fields);
    }

    #[test]
    fn normalizes_marker_text() {
        assert_eq!(normalize_marker_text("OpenAI/Codex"), "openai codex");
    }

    #[test]
    fn detects_markers_on_word_boundaries() {
        let markers = ["openai", "gpt"];
        assert_eq!(
            contains_marker("Generated with OpenAI tooling", &markers),
            Some("openai".to_string())
        );
        assert_eq!(contains_marker("adapt", &markers), None);
    }

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(60 * 17), "17m");
        assert_eq!(format_duration(60 * 60 * 2), "2h");
        assert_eq!(format_duration((60 * 60 * 2) + (60 * 7)), "2h 7m");
    }
}
