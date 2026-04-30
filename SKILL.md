---
name: git-push-scheduler
description: Split uncommitted work into commits, schedule staggered pushes across multiple GitHub accounts, and run the schedule via a persistent daemon or in the background.
---

# Git Push Scheduler

Use this skill to:
1. Split uncommitted local changes into logical commits.
2. Schedule those commits to push to a remote with realistic, randomized delays.
3. Push under any of several saved GitHub accounts (useful for managing multiple identities).
4. Deliver the plan to the daemon (preferred) or run directly in the background.

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

## Daemon Setup (one-time, preferred)

The daemon watches `~/.config/push_scheduler/pending/` for plan files and executes them automatically. Once installed, the agent only needs to create a plan file — no process spawning required.

```bash
# Install and start the daemon via launchd (survives reboots, auto-restarts on crash)
/tmp/git-push-scheduler-target/release/push_scheduler daemon-install

# Monitor daemon activity
tail -f ~/Library/Logs/push_scheduler_daemon.log

# Stop the daemon
launchctl unload ~/Library/LaunchAgents/com.push-scheduler.daemon.plist
```

Directory layout the daemon manages:
```
~/.config/push_scheduler/
  pending/    ← drop .plan files here
  running/    ← plan being executed (moved atomically from pending/)
  done/       ← completed plans (timestamped)
  failed/     ← failed plans (timestamped)
```

Crash recovery: on startup the daemon moves any files left in `running/` back to `pending/` and retries them.

---

## Typical Workflow (with daemon)

### 1 — Split uncommitted changes into commits (optional)

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  split \
  --repo /absolute/path/to/repo \
  --count 4                      # optional; defaults to 3–5 based on line count
  --message-prefix "feat"        # optional commit message prefix
```

### 2 — Plan the push schedule

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  plan \
  --repo /absolute/path/to/repo \
  --plan-file ~/.config/push_scheduler/pending/job.plan \
  --account work                  # optional; uses saved account credentials
  --first-delay-seconds 0
  --min-delay-seconds 720
  --max-delay-seconds 5400
```

Saving the plan directly to `pending/` is enough — the daemon picks it up within 30 seconds. No further action needed.

### 3 — Confirm with the user

Show the printed schedule. Always include:
- Branch and tracked upstream
- Author/committer identities in the pending commits
- Per-commit delay and cumulative push time
- GitHub username and email if an account is configured
- Total runtime

Wait for explicit approval before writing the plan file to `pending/`.

---

## Typical Workflow (without daemon / direct execution)

### Steps 1–3 as above, then:

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

`--background` spawns the worker process and returns immediately. Monitor with `tail -f /tmp/push_scheduler.log`.

---

## Account Management

Accounts are stored in `~/.config/push_scheduler/accounts` (mode 600). Tokens are never written to plan files or generated scripts. The daemon resolves accounts by `github_username` from the stored accounts file — no token needs to be passed at plan time.

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

---

## Commands Reference

### `daemon`
| Option | Default | Description |
|---|---|---|
| `--dir PATH` | `~/.config/push_scheduler` | Root directory containing pending/running/done/failed subdirs |
| `--poll-interval-seconds N` | `30` | How often to scan pending/ for new plans |

### `daemon-install`
| Option | Default | Description |
|---|---|---|
| `--dir PATH` | `~/.config/push_scheduler` | Same dir passed to daemon |

Writes `~/Library/LaunchAgents/com.push-scheduler.daemon.plist` and loads it via `launchctl`. Logs to `~/Library/Logs/push_scheduler_daemon.log`.

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
| `--plan-file PATH` | `/tmp/git-push-scheduler.plan` | Output plan — use `~/.config/push_scheduler/pending/name.plan` for daemon delivery |
| `--account NAME` | none | Push under this saved account |
| `--first-delay-seconds N` | `0` | Delay before the first push |
| `--min-delay-seconds N` | `720` | Minimum delay between pushes (12 min) |
| `--max-delay-seconds N` | `5400` | Maximum delay between pushes (90 min) |
| `--seed N` | auto | Seed for reproducible plans |

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
| `--token TOKEN` | none | Override GitHub token (required when plan has an account and no daemon) |

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

---

## Failure Cases

| Condition | Behaviour |
|---|---|
| AI-tool markers in commit metadata | Fails during `plan`; reports commit SHA and field |
| Account authentication fails | Fails immediately during `plan --account` or at execution start |
| Remote branch drifted | Execution fails closed before the affected push |
| Branch mismatch | Execution fails closed |
| HEAD changed since plan | Execution fails closed |
| No pending commits | `plan` fails with clear message |
| No uncommitted changes | `split` fails with clear message |
| Daemon crash mid-plan | Plan moved from running/ back to pending/ on next startup |

---

## Example Invocations

- *"Split my uncommitted work into 4 commits, then schedule a staggered push via the daemon."*
- *"Push these 3 local commits as my work GitHub account."*
- *"Generate a push script I can run myself after review."*
- *"Install the daemon so future plans run automatically."*
- *"Verify that my 'personal' account can push to this repo."*
