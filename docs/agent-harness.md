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
| After each `Edit`/`Write` | `PostToolUse` → `fsmap hook post-edit` | For `.rs`, `Cargo.toml` and `build.rs`: `cargo check --all-targets --offline` on the edited package. Errors go back to Claude as a block reason (first 20, de-duplicated); the fsmap brief for notable files goes along as context. One check at a time per worktree: a later edit waits up to 30 s for the running one, then checks the tree as it is. 60 s budget. | 1.1 s median |
| Claude ends a turn | `Stop` → `fsmap hook stop` | If the tree changed since the session started or last passed, runs the gate in quick mode. The gate passes only when every step passes. A failure blocks the stop with the failing steps' output, marking which files the session changed. After three blocked stops in a row it lets go and asks Claude to report; when every failure is in files the session did not change, it lets go after one block instead, telling the user the gate did **not** pass. A pass shows the user any added suppressions, ignored tests, removed assertions or changed lint configuration. While background agents (subagents, workflows) are still working, it waits for the real end of the turn. | 37–42 s |
| The user writes | `UserPromptSubmit` → `fsmap hook prompt` | Resets the failure streak. | ms |
| Before `git push`, `gh pr create`, `gh pr merge` | `PreToolUse` (Bash) → `fsmap hook pre-bash` | Runs the quick gate unless this exact tree already passed, and denies the command on failure or when the gate cannot run. Commands are read with their quoting, wrappers (`env`, `timeout`, `sudo` …), `sh -c` and `eval` scripts, and git's and gh's global options. | 5 ms for other commands |

When cargo cannot read the project (a broken manifest, a malformed verify table, an unsafe
toolchain setting), the gate cannot run: the stop is blocked and a push denied with the reason,
never silently allowed. A hook that crashes reports it to the user.

The gate itself, `fsmap verify` (every step runs even after a failure, so one round reports
everything):

| Step | Command | Notes |
|---|---|---|
| integrity | (in-process) | Reads every cargo, rustfmt, clippy and rustup file that applies, as TOML, against an allowlist. See below. |
| fmt | `cargo fmt --all -- --check` | |
| clippy | `cargo clippy --all-targets -- -D warnings` | `--locked` when `Cargo.lock` exists |
| test | `cargo test` | `--locked` when `Cargo.lock` exists |
| deny | `cargo deny check` | `--mode full` only; skipped (and reported) when cargo-deny is missing, failed with `--strict`. |
| extra | from `Cargo.toml` | e.g. this repo runs the viewer syntax check and fsmap's own insights gate. |

Exit codes: `0` passed, `1` a step failed, `2` the gate could not run. `--json` prints the report
for tools.

**Integrity.** The files: `.cargo/config(.toml)` in the root and its ancestors and in
`CARGO_HOME`; every `rustfmt.toml` and `clippy.toml` inside the tree, git-ignored ones included
(rustfmt looks upward from each source file), in the ancestors and in rustfmt's user
configuration directory; `rust-toolchain(.toml)` in the ancestors. What fails, by the kind a
repository can trust:

| Kind | Setting |
|---|---|
| `runner`, `linker` | `target.*.runner`; `target.*.linker`, `-C linker`, `-C link-arg` and other linker flags (a fake linker can replace the test binary) |
| `rustc-wrapper` | `build.rustc`, `build.rustc-wrapper`, `build.rustc-workspace-wrapper`, `build.rustdoc` |
| `cap-lints`, `allow-flag`, `rustflag` | `--cap-lints`, `-A`/`--allow`, and any other rustflag outside a short safe list (`-C target-cpu`, `-C opt-level`, `-W`/`-D` …) |
| `env`, `alias` | an `[env]` table; an `[alias]` named after a command the gate runs (`fmt`, `clippy`, `deny` …) |
| `config:<table>` | any cargo key outside the allowlist (`[patch]`, `[unstable]`, `profile.*.debug-assertions` …) |
| `fmt-disabled`, `fmt-ignore` | `disable_all_formatting`, a non-empty `ignore`, `skip_children` |
| `untracked-clippy-config` | a `clippy.toml` inside the repository that git does not track |
| `toolchain-path`, `toolchain-custom` | a toolchain `path`, or a channel that is not an official one (a toolchain linked with `rustup toolchain link`). Checked before the gate runs cargo at all. |

Every step runs without the variables that do the same from the environment (`RUSTFLAGS`,
`RUSTDOCFLAGS`, `RUSTC`, `RUSTC_WRAPPER`, `RUSTUP_TOOLCHAIN`, `RUSTC_BOOTSTRAP`, all
`CARGO_BUILD_*`, `CARGO_TARGET_*`, `CARGO_ALIAS_*`, `CARGO_PROFILE_*`, snapshot-update switches
such as `INSTA_FORCE_PASS`), with the build directory inside the repository, within a time budget
(600 s for the hooks). A step that overruns has its process tree killed; background processes it
left behind cannot hold the gate past the budget.

Timings: a 4-core cloud container, 2026-10-29, this repository (106 tests). Session start and
pre-bash on a clean clone, three runs each; post-edit from the live session's log; the gate is
`fsmap verify` run by hand: 37 s with nothing to rebuild (three runs), 42 s after a source change.
Nearly all of it is `cargo test`, whose 25 end-to-end hook tests each build a small Cargo
project; a repository without such tests pays its own test time.

## What it catches

Each row has an end-to-end test in [`tests/harness.rs`](../tests/harness.rs) or a unit test next
to the guard. Every guard listed was checked by removing it and watching its test fail
(D-15 lists the mutation run).

| Shortcut or failure | Guard |
|---|---|
| Committing during the turn, so the tree looks clean | The fingerprint includes HEAD |
| The first commit of an empty repository | Unborn → born HEAD changes the fingerprint |
| Editing an already-modified file (no `git status` change) | Dirty files' contents are hashed |
| A hidden `.cargo/config.toml` with `--cap-lints allow`, `runner = "true"`, an `[alias]` for `clippy`, a fake linker; a nested `rustfmt.toml`; a toolchain `path` | The integrity step (TOML allowlist), never counted as someone else's failure; config files are part of the fingerprint whether git sees them or not |
| The same through `RUSTFLAGS`, `CARGO_TARGET_*_RUNNER`, `CARGO_ALIAS_*` | Variables removed for every step |
| A toolchain file that runs the repository's own `cargo` | Refused before the first cargo call |
| Auto-compaction re-recording a broken tree as the baseline | Baseline written once, on `startup` |
| A dev server running in the background switching the gate off | Only background *agents* postpone it |
| The gate failing forever, or never again after one release | Three strikes then a report; the next user prompt resets |
| Pushing a broken commit, also from a session that started on it | The pre-push gate; only a recorded pass counts |
| A change that breaks a file it never touched (a helper left without a caller) | The gate passes only when everything passes |
| Failures from before the session in files nobody touched | Blocked once, then a report; the user is told the gate did not pass |
| A broken manifest or verify table | The stop is blocked and a push denied; no silent pass |
| A suppression, an ignored test or a deleted assertion that keeps the gate green | Shown to the user on pass |
| Two sessions in one worktree | Announced at session start; state updates under a lock keep both records |
| fsmap not installed, or an older fsmap without `fsmap hook` | Session start says the harness is OFF; the other hooks exit 0 and stay out of the way |

## Setting it up in a Rust repository

1. Install fsmap where the agent runs:
   `cargo install --locked --git https://github.com/d-j-lee/filesystemMap fsmap`. In a Claude Code
   cloud environment, put that line in the environment's setup script.
2. Copy from this repository's [`harness/`](../harness/):
   - `settings.json` → `.claude/settings.json` (merge by hand if you already have one);
   - `agents/rust-reviewer.md` → `.claude/agents/rust-reviewer.md`;
   - `AGENTS.md` → `AGENTS.md`, and add `@AGENTS.md` at the top of `CLAUDE.md`.

   The hook commands are POSIX shell (`… || true`); on Windows, Claude Code runs them with Git Bash.
3. Add the lints from this repository's `Cargo.toml` (`[lints.clippy]`), its `clippy.toml`, and a
   `deny.toml` (start from this repository's and adjust the licence list).
4. Optional: extra steps and trusted settings in `Cargo.toml`:

   ```toml
   [package.metadata.fsmap.verify]   # or [workspace.metadata.fsmap.verify]
   extra = [["node", "--check", "web/app.js"]]
   trust = ["rustc-wrapper", "env:RUSTC_WRAPPER"]   # e.g. for sccache
   ```

   Only `extra` and `trust` are accepted; anything else, or a malformed entry, is an error.
5. In CI, keep explicit fmt/clippy/test steps (the gate of record) and add a job that runs
   `fsmap verify --mode full --strict`.

Check the first session: Claude's context should start with `[harness] fsmap … hooks are active`.
`fsmap hook stats` summarises what the hooks did (blocks, passes, skips, median times) from the
log in `<git-dir>/fsmap-harness/log.jsonl`.

## Limits

- **A guardrail, not a security boundary.** The agent can edit what the hooks read (its state
  directory, `.claude/settings.local.json`, which git usually ignores). `PATH`, the installed
  toolchain and settings outside the files above are trusted. The ask rules make the obvious
  edits visible; CI is the gate of record.
- **What the pre-push check cannot see.** A git alias for `push`, a script or a shell function
  that pushes. The stop gate still runs at the end of the turn.
- **clippy.toml is not judged.** Raising a lint threshold there is a legitimate choice as often
  as a shortcut. An untracked one fails the gate; changes to a tracked one are shown on pass.
- **Where hooks run.** In Claude Code cloud sessions, a repository's `.claude/settings.json` hooks
  and permission rules apply only in a session with one repository, outside a Project thread.
  Interactive local sessions need workspace trust first. A turn the user interrupts does not
  fire Stop; the pre-push gate still applies.
- **Running repository code.** `cargo check` and the tests run build scripts, proc macros and test
  code automatically. That is the point for your own repository; do not enable the hooks on a
  repository you do not trust.
- **One workspace.** The gate checks the Cargo workspace at the project root. Several workspaces
  in one repository, or a workspace in a subdirectory, are not covered yet.
- **Windows and macOS.** The harness tests are in the CI matrix for both (the Unix-only shell
  tests aside); a real Windows Claude Code session has not been tried.
- **Stat-only changes.** With filters neutralised, a file whose stat changed but whose content did
  not (common with Git LFS) can make the gate run again for nothing.

## Evidence

The design was reviewed by an independent agent before implementation (3 blockers, 8 major
findings), and the implementation by another after it (2 blockers, 6 major findings, among them
bypasses of the first integrity check and claims in this guide that were not true). What was
adopted, and what was not, is in D-15.

The hooks took effect in the Claude Code cloud session that wrote them, without a restart. Its log
shows:

- the first `Edit` after `.claude/settings.json` existed running `post-edit` (cargo check, median
  1.1 s over 10 edits), and later blocking an edit that broke two callers;
- an automatic compaction firing `SessionStart` with source `compact`, which left the baseline
  alone and told Claude the tree had unverified changes;
- the first turn end firing `Stop` while a review subagent ran in the background: the gate was
  postponed (`skip-background`), which also confirms that Claude Code lists subagents in the
  Stop payload's `background_tasks`.

A gate run at a live turn end had not been logged yet when this was written.
