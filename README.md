# commit_schedule_pusher

Git push scheduler skill bundle and Rust scheduler binary.

Contents:
- `SKILL.md`: skill instructions and operating rules
- `agents/openai.yaml`: agent metadata
- `scripts/push_scheduler/`: Rust CLI for planning schedules, exporting Bash/PowerShell runner scripts, and optionally executing them directly

Build:

```bash
cargo build --release --manifest-path scripts/push_scheduler/Cargo.toml
```

Usage:

```bash
./target/release/push_scheduler plan --repo /path/to/repo --plan-file /tmp/git-push-scheduler.plan
./target/release/push_scheduler script --plan-file /tmp/git-push-scheduler.plan --shell bash
./target/release/push_scheduler script --plan-file /tmp/git-push-scheduler.plan --shell powershell
./target/release/push_scheduler execute --plan-file /tmp/git-push-scheduler.plan --yes
```

Typical workflow:

1. Build plan from commits already ahead of `@{upstream}`.
2. Review planner summary and audit output.
3. Export Bash or PowerShell script from saved plan.
4. Hand script to user to run themselves.

The generated script preserves existing commit metadata and fails closed if:
- current branch changed
- local HEAD changed
- remote branch drifted since plan creation
