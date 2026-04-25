# commit_schedule_pusher

Git push scheduler skill bundle and Rust scheduler binary.

Contents:
- `SKILL.md`: skill instructions and operating rules
- `agents/openai.yaml`: agent metadata
- `scripts/push_scheduler/`: Rust CLI for planning and executing staggered `git push` schedules

Build:

```bash
cargo build --release --manifest-path scripts/push_scheduler/Cargo.toml
```

Usage:

```bash
./target/release/push_scheduler plan --repo /path/to/repo --plan-file /tmp/git-push-scheduler.plan
./target/release/push_scheduler execute --plan-file /tmp/git-push-scheduler.plan --yes
```
