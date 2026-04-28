---
name: git-push-scheduler
description: Split uncommitted work into commits, schedule staggered pushes across multiple GitHub accounts, and run the schedule in the background or export a copy-paste script.
---

# Git Push Scheduler

Use this skill to:
1. Split uncommitted local changes into logical commits.
2. Schedule those commits to push to a remote with realistic, randomized delays.
3. Push under any of several saved GitHub accounts (useful for managing multiple identities).
4. Run the schedule in the background so Claude's response completes immediately.

This skill never rewrites commit history, amends authors, or suppresses AI-marker audit failures.

---

## Build

```bash
CARGO_TARGET_DIR=/tmp/git-push-scheduler-target \
  cargo build --locked --release \
  --manifest-path /Users/iwan/.codex/skills/git-push-scheduler/scripts/push_scheduler/Cargo.toml
```

Binary path after build: `/tmp/git-push-scheduler-target/release/push_scheduler`

---

## Typical Workflow

### 1 — Split uncommitted changes into commits (optional)

If the user has uncommitted work to split into several commits first:

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  split \
  --repo /absolute/path/to/repo \
  --count 4                      # optional; defaults to 3–5 based on line count
  --message-prefix "feat"        # optional commit message prefix
```

`split` stages all changes, groups files into N logical buckets by directory, and commits each bucket. After this step, the commits are local and can be planned.

### 2 — Plan the push schedule

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  plan \
  --repo /absolute/path/to/repo \
  --plan-file /tmp/git-push-scheduler.plan \
  --account work                  # optional; uses saved account credentials
  --first-delay-seconds 0         # optional; default 0 (push first commit immediately)
  --min-delay-seconds 720         # optional; default 720 (12 min)
  --max-delay-seconds 5400        # optional; default 5400 (90 min)
```

The planner:
- Finds all commits ahead of `@{upstream}`.
- Classifies each commit (small/medium/large/very-large) to determine proportional delays.
- Adds per-commit randomized jitter at the second level (not just whole minutes).
- If `--account` is given, performs a quick-fail auth test against the remote before saving the plan.
- Prints a full schedule: SHA, subject, size band, per-commit delay, cumulative time, author, and the remote/branch/account details.
- Saves the plan to disk.

### 3 — Confirm with the user

Show the printed schedule. Always include:
- Branch and tracked upstream
- Author/committer identities in the pending commits
- Per-commit delay and cumulative push time
- GitHub username and email if an account is configured
- Total runtime

Wait for explicit approval before generating a script or executing.

### 4a — Generate a copy-paste script

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  script \
  --plan-file /tmp/git-push-scheduler.plan \
  --shell bash         # or powershell
  --output /tmp/push.sh
```

Return the script to the user in a fenced code block. If an account is configured the script requires:
```bash
export PUSH_SCHEDULER_TOKEN='ghp_...'
bash /tmp/push.sh
```

### 4b — Execute directly (background)

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  execute \
  --plan-file /tmp/git-push-scheduler.plan \
  --yes \
  --background \
  --log-file /tmp/push_scheduler.log \
  --token ghp_...        # required when plan has an account
```

`--background` spawns the worker process and returns immediately so Claude's response can complete while pushes run in the background. The command prints the PID and log path. Monitor with `tail -f /tmp/push_scheduler.log`.

---

## Account Management

Accounts are stored in `~/.config/push_scheduler/accounts` (mode 600). Tokens are never written to plan files or generated scripts — scripts reference `$PUSH_SCHEDULER_TOKEN`.

```bash
# Add an account
push_scheduler account add \
  --name work \
  --username githubuser123 \
  --email work@example.com \
  --token ghp_...

# List accounts
push_scheduler account list

# Verify an account against a specific repo (quick-fail auth test)
push_scheduler account verify --name work --repo /path/to/repo

# Remove an account
push_scheduler account remove --name work
```

Verification checks:
- Token is valid and not expired.
- Account has read access to the repository.
- Reports actionable hints for authentication failures, missing scopes, and permission errors.

---

## Commands Reference

### `split`
| Option | Default | Description |
|---|---|---|
| `--repo PATH` | `.` | Repository root |
| `--count N` | auto (3–5) | Number of commits to create |
| `--message-prefix TEXT` | none | Prepend `prefix: ` to each commit message |

### `plan`
| Option | Default | Description |
|---|---|---|
| `--repo PATH` | `.` | Repository root |
| `--plan-file PATH` | `/tmp/git-push-scheduler.plan` | Output plan |
| `--account NAME` | none | Push under this saved account |
| `--first-delay-seconds N` | `0` | Delay before the first push |
| `--min-delay-seconds N` | `720` | Minimum delay between pushes (12 min) |
| `--max-delay-seconds N` | `5400` | Maximum delay between pushes (90 min) |
| `--seed N` | auto | Seed for reproducible plans |

Delays are in seconds with per-commit random jitter of 0–89 additional seconds. `--min-delay-minutes` / `--max-delay-minutes` are accepted as aliases for backward compatibility.

### `script`
| Option | Default | Description |
|---|---|---|
| `--plan-file PATH` | `/tmp/git-push-scheduler.plan` | Input plan |
| `--shell bash\|powershell` | `bash` | Output shell syntax |
| `--output PATH` | stdout | Write script to file (chmod +x on Unix) |

### `execute`
| Option | Default | Description |
|---|---|---|
| `--plan-file PATH` | `/tmp/git-push-scheduler.plan` | Input plan |
| `--yes` | required | Confirmation flag |
| `--background` | off | Detach worker process immediately |
| `--log-file PATH` | `/tmp/push_scheduler_<epoch>.log` | Log file for background mode |
| `--token TOKEN` | none | Override GitHub token (required when plan has an account) |

### `account`
Subcommands: `add`, `list`, `remove`, `verify`. See Account Management above.

---

## Delay Sizing

| Commit size | Score formula | Base range | Jitter |
|---|---|---|---|
| small | ≤ 40 | 12–22 min | +0–89 s |
| medium | 41–140 | 18–35 min | +0–89 s |
| large | 141–360 | 30–55 min | +0–89 s |
| very-large | > 360 | 45–85 min | +0–89 s |

Score = insertions + deletions + (files_changed × 12) + (binary_files × 30)

The first commit uses `--first-delay-seconds` (default 0 = immediate). Its size classification is computed but does not affect timing unless the user specifies a delay.

---

## Failure Cases

| Condition | Behaviour |
|---|---|
| AI-tool markers in commit metadata | Fails during `plan`; reports commit SHA and field |
| Account authentication fails | Fails immediately during `plan --account` or `execute --token` |
| Remote branch drifted | Script/execute fails closed before the affected push |
| Branch mismatch | Script/execute fails closed |
| HEAD changed since plan | Script/execute fails closed |
| No pending commits | `plan` fails with clear message |
| No uncommitted changes | `split` fails with clear message |

---

## Example Invocations

- *"Split my uncommitted work into 4 commits, then schedule a staggered push."*
- *"Push these 3 local commits as my work GitHub account."*
- *"Generate a push script I can run myself after review."*
- *"Start the push schedule in the background and return the log path."*
- *"Verify that my 'personal' account can push to this repo."*
