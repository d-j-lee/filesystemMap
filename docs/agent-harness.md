# The agent harness

> A verification gate and Claude Code hooks for Rust repositories where an AI agent writes much of
> the code. Decision record: [decisions.md D-15](decisions.md). Rules the agent follows:
> [../AGENTS.md](../AGENTS.md). Why Rust and agents fit: [rust-ai-playbook.md](rust-ai-playbook.md).

## Two layers

An agent needs two kinds of feedback while it works.

| Layer | Question | Provided by |
|---|---|---|
| **Verification** | Does this compile, lint and pass the tests, and did I weaken any check to get there? | `fsmap verify` and the hooks below |
| **Context** | What is this repository, what usually changes with this file, what is risky? | fsmap's analysis: `brief`, `context`, `insights` |

The harness puts both into the agent's loop through Claude Code hooks, using one binary. The other
agent tools (Codex, Cursor) get the same rules through `AGENTS.md` and can run `fsmap verify`
themselves.

## What runs when

| Moment | Hook | What happens | Measured on this repo |
|---|---|---|---|
| Session starts | `SessionStart` → `fsmap hook session-start` | Records the baseline (only on `startup`, never on resume or compact). Tells Claude the real `rustc`/`cargo` versions and where each direct dependency's source is, and warns about settings that weaken the gate and about other sessions in the same worktree. Warms the brief index. | 0.4 s |
| After each `Edit`/`Write` | `PostToolUse` → `fsmap hook post-edit` | For `.rs`, `Cargo.toml` and `build.rs`: `cargo check --all-targets --offline` on the edited package. Errors go back to Claude as a block reason (first 20, de-duplicated); the fsmap brief for notable files goes along as context. One check at a time per worktree, 60 s budget. | 1.1 s median |
| Claude ends a turn | `Stop` → `fsmap hook stop` | If the tree changed since the session started or last passed: runs the gate in quick mode. New failures block the stop with the failing steps' output. After three blocked stops in a row it lets go and asks Claude to report. A pass shows the user any added suppressions, ignored tests or removed assertions. While background agents (subagents, workflows) are still working, it waits for the real end of the turn. | 11–23 s |
| The user writes | `UserPromptSubmit` → `fsmap hook prompt` | Resets the failure streak. | ms |
| Before `git push`, `gh pr create`, `gh pr merge` | `PreToolUse` (Bash) → `fsmap hook pre-bash` | Runs the quick gate unless the tree already passed; denies the command on failure. | 5 ms for other commands |

The gate itself, `fsmap verify` (every step runs even after a failure, so one round reports
everything):

| Step | Command | Notes |
|---|---|---|
| integrity | (in-process) | Fails on cargo, rustfmt or rustup settings that make the other steps pass without the code changing: `runner`, `rustc-wrapper`, `--cap-lints`, `-A`, `[env]`, `disable_all_formatting`, rustfmt `ignore`, a toolchain `path`. |
| fmt | `cargo fmt --all -- --check` | |
| clippy | `cargo clippy --all-targets -- -D warnings` | `--locked` when `Cargo.lock` exists |
| test | `cargo test` | `--locked` when `Cargo.lock` exists |
| deny | `cargo deny check` | `--mode full` only; skipped (and reported) when cargo-deny is missing, failed with `--strict`. |
| extra | from `Cargo.toml` | e.g. this repo runs the viewer syntax check and fsmap's own insights gate. |

All steps run without `RUSTFLAGS`, `RUSTC_WRAPPER`, target runners, `INSTA_FORCE_PASS` and similar
variables, with the build directory inside the repository, within a time budget (600 s for the
hooks; a step that overruns has its whole process tree killed). `--json` prints the report for
tools.

Timings: a 4-core cloud container, 2026-10-29, this repository (90 tests). Session start and
pre-bash on a clean clone, three runs each; post-edit from the live session's log; the gate is
`fsmap verify` run three times by hand: 11 s with nothing to rebuild, 16–23 s after a source edit,
of which `cargo test` is 11–17 s.

## What it catches

Each item has an end-to-end test in [`tests/harness.rs`](../tests/harness.rs), and each test was
checked to fail when its guard is removed.

| Shortcut or failure | Guard |
|---|---|
| Committing during the turn, so the tree looks clean | The fingerprint includes HEAD |
| The first commit of an empty repository | Unborn → born HEAD changes the fingerprint |
| Editing an already-modified file (no `git status` change) | Dirty files' contents are hashed |
| `.cargo/config.toml` with `--cap-lints allow` and `runner = "true"`, even hidden through `.git/info/exclude` | integrity step; always blocking; config files hashed whether ignored or not |
| The same through `RUSTFLAGS` or `CARGO_TARGET_*_RUNNER` | Variables removed for every step |
| Auto-compaction re-recording a broken tree as the baseline | Baseline written once, on `startup` |
| A dev server running in the background switching the gate off | Only background *agents* postpone it |
| The gate failing forever, or never again after one release | Three strikes then a report; the next user prompt resets |
| Pushing a broken commit | The pre-push gate |
| Failures from before the session (or a new toolchain's lints) in files nobody touched | Reported to the user, not blocked on |
| A suppression, an ignored test or a deleted assertion that keeps the gate green | Shown to the user on pass |
| Two sessions in one worktree | Announced at session start |
| fsmap not installed | The session-start command says so to the user and to Claude, instead of running without a gate |

## Setting it up in a Rust repository

1. Install fsmap where the agent runs:
   `cargo install --locked --git https://github.com/d-j-lee/filesystemMap fsmap`. In a Claude Code
   cloud environment, put that line in the environment's setup script.
2. Copy from this repository's [`harness/`](../harness/):
   - `settings.json` → `.claude/settings.json` (merge by hand if you already have one);
   - `agents/rust-reviewer.md` → `.claude/agents/rust-reviewer.md`;
   - `AGENTS.md` → `AGENTS.md`, and add `@AGENTS.md` at the top of `CLAUDE.md`.
3. Add the lints from this repository's `Cargo.toml` (`[lints.clippy]`), its `clippy.toml`, and a
   `deny.toml` (start from this repository's and adjust the licence list).
4. Optional: extra steps and trusted settings in `Cargo.toml`:

   ```toml
   [package.metadata.fsmap.verify]   # or [workspace.metadata.fsmap.verify]
   extra = [["node", "--check", "web/app.js"]]
   trust = ["rustc-wrapper", "env:RUSTC_WRAPPER"]   # e.g. for sccache
   ```

5. In CI, keep explicit fmt/clippy/test steps (the gate of record) and add a job that runs
   `fsmap verify --mode full --strict`.

Check the first session: Claude's context should start with `[harness] fsmap … hooks are active`.
`fsmap hook stats` summarises what the hooks did (blocks, passes, skips, median times) from the
log in `<git-dir>/fsmap-harness/log.jsonl`.

## Limits

- **A guardrail, not a security boundary.** The agent can edit what the hooks read (its state
  directory, `.claude/settings.local.json`, which git usually ignores). The ask rules make the
  obvious edits visible; CI is the gate of record.
- **Where hooks run.** In Claude Code cloud sessions, a repository's `.claude/settings.json` hooks
  and permission rules apply only in a session with one repository, outside a Project thread.
  Interactive local sessions need workspace trust first. A turn the user interrupts does not
  fire Stop; the pre-push gate still applies.
- **Running repository code.** `cargo check` and the tests run build scripts, proc macros and test
  code automatically. That is the point for your own repository; do not enable the hooks on a
  repository you do not trust.
- **One workspace.** The gate checks the Cargo workspace at the project root. Several workspaces
  in one repository, or a workspace in a subdirectory, are not covered yet.
- **Windows.** The harness tests run in the Windows CI job (one Unix-only shell test aside); a real
  Windows Claude Code session has not been tried.
- **Stat-only changes.** With filters neutralised, a file whose stat changed but whose content did
  not (common with Git LFS) can make the gate run again for nothing.

## Evidence

The design was reviewed by an independent agent before implementation (3 blockers, 8 major
findings; what was adopted and what was not is in D-15). Each guard in the table above was checked
by removing it and watching its test fail.

The hooks took effect in the Claude Code cloud session that wrote them, without a restart. Its log
shows:

- the first `Edit` after `.claude/settings.json` existed running `post-edit` (cargo check, median
  1.1 s over 10 edits);
- an automatic compaction firing `SessionStart` with source `compact`, which left the baseline
  alone and told Claude the tree had unverified changes;
- the first turn end firing `Stop` while a review subagent ran in the background: the gate was
  postponed (`skip-background`), which also confirms that Claude Code lists subagents in the
  Stop payload's `background_tasks`.

A gate run at a live turn end had not been logged yet when this was written.
