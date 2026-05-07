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

## macOS Keychain — One-Time Setup (Fix "Always Allow" prompt)

When the daemon pushes in the background, macOS may show a keychain dialog:
> "git-credential-osxkeychain wants to use your confidential information stored in github.com"

**Fix: click "Always Allow"** (not just "Allow") the next time it appears and enter your Mac login password. macOS will never prompt again for that keychain entry.

If the daemon is unattended and you can't click the dialog, switch to a credential store that never prompts:

```bash
git config --global credential.helper store
# Then do one manual push — enters username+token once, writes to ~/.git-credentials
```

After either fix, the daemon runs fully silent with no dialogs.

---

## MANDATORY: Confirm Before Any Action

**Before running ANY command, present the filled confirmation table to the user and wait for explicit "yes" / approval.**

### Step 0 — Auto-detect repo state (always run this first)

Run these commands immediately when the skill is invoked, before asking the user anything:

```bash
git -C <repo> log --oneline --not --remotes 2>/dev/null || git -C <repo> log --oneline   # unpushed commits (fallback for repos with no remote refs)
git -C <repo> diff --stat HEAD                 # uncommitted changes
git -C <repo> remote get-url origin 2>/dev/null || echo "(no remote)"  # remote URL
git -C <repo> rev-parse --abbrev-ref HEAD      # current branch
git -C <repo> config user.name
git -C <repo> config user.email
```

**Fresh-repo detection:** if `git log --not --remotes` returns all local commits (no remote refs exist) and there is no tracked upstream, this is a **fresh push**. The binary handles this automatically — do not manually create fake tracking refs or seed commits. Just proceed; `base_sha` in the plan will be the null SHA and the remote branch will be created by the first push.

Use the output to fill every field in the table below. Apply these defaults for any field the user has not specified:

| Field | Default if not specified |
|---|---|
| Commits to push | All unpushed commits found by `git log --not --remotes` |
| Split uncommitted changes | Yes, if `git diff --stat HEAD` is non-empty; No otherwise |
| Split count | 3–5 based on total changed-line count |
| Min delay | 10 min |
| Max delay | 40 min |
| First push delay | 0 s |
| GitHub account | `git config user.name` + `git config user.email` |
| Execution method | daemon if installed, else background |

**Never leave a field blank or as a placeholder.** If a value truly cannot be determined (e.g. remote URL not set), fill it with `⚠ unknown — please specify` so the user knows to correct it.

### Confirmation table

Present this table, fully filled with real values, every time the skill runs:

```
📋 Push Schedule — Please Confirm

  Repo path        : /path/to/repo
  Remote URL       : https://github.com/owner/repo
  Branch           : main → origin/main

  Uncommitted changes : yes (will split into N commits) / no
  Commits to push  :
    abc1234  feat: add login page
    def5678  fix: correct header styles
    ...

  Commit author    : First Last <email@example.com>
                     ⚠ confirm — no Claude/AI attribution allowed

  Schedule
    Delay before first push : 0 s
    Min delay between pushes: 10 min
    Max delay between pushes: 40 min
    Total window (estimated): ~X hours

  GitHub account   : username / default git credentials
  Execution method : daemon / background / script

  Run command (manual fallback):
    bash /tmp/push.sh
```

**The "Run command" field must always appear in the confirmation table.** The script will be generated at `/tmp/push.sh` after approval — show the path now so the user knows where to find it. This is critical: it is the user's manual override if the daemon stalls.

**Do not proceed until the user confirms or corrects this table.**
Reply "yes", "looks good", or similar to approve. Any correction → update the table and re-show before proceeding.

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

Present the **mandatory confirmation table** above (if not already shown). Wait for explicit approval. Do not write any plan file until the user says yes.

After the plan runs, always generate a standalone bash script (Step 4a) and show the user the command to run it themselves, even if they chose daemon execution.

---

## Typical Workflow (without daemon / direct execution)

### Steps 1–3 as above, then:

### 4a — Generate a copy-paste script (ALWAYS DO THIS)

Always generate the script regardless of execution method chosen. This gives the user a fallback they can run manually.

```bash
/tmp/git-push-scheduler-target/release/push_scheduler \
  script \
  --plan-file /tmp/git-push-scheduler.plan \
  --shell bash         # or powershell
  --output /tmp/push.sh
```

**Always output the run command to the user**, even when using the daemon:

```
✅ Script saved. To run it yourself at any time:

    bash /tmp/push.sh

(If using a GitHub account token)
    export PUSH_SCHEDULER_TOKEN='ghp_...'
    bash /tmp/push.sh
```

Show this block at the end of every skill run — it is the user's manual override if the daemon is unavailable.

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
| Fresh repo / no upstream | `plan` detects missing upstream and uses null SHA as base; remote branch created by first push — **do not manually create fake tracking refs** |

---

## Example Invocations

- *"Split my uncommitted work into 4 commits, then schedule a staggered push via the daemon."*
- *"Push these 3 local commits as my work GitHub account."*
- *"Generate a push script I can run myself after review."*
- *"Install the daemon so future plans run automatically."*
- *"Verify that my 'personal' account can push to this repo."*
