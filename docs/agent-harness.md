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

## What it defends against

An agent that takes the shortest path to green: it stubs out code, suppresses a lint, skips a
test, calls a change done before checking it, or pushes a broken commit. The harness answers in
three ways.

| What the agent does | Answer |
|---|---|
| Leaves something failing | Blocked: the gate passes only when fmt, clippy, the tests and the repository's own checks all pass |
| Weakens the checks where nobody sees it: an untracked or git-ignored config file, a config outside the repository, environment variables | Blocked: the integrity step and the environment cleanup below. This is what would make the local gate disagree with CI. |
| Weakens the checks in tracked files: a lint allowed in `Cargo.toml`, `debug-assertions = false`, `#[ignore]`, a deleted assertion, a committed `.cargo/config.toml` or toolchain file | Reported to the user on every pass. The change is in the diff and runs the same way in CI; whether it is legitimate is a person's call. Editing these files asks the user first (the ask rules in `settings.json`). |

Out of scope: an agent determined to defeat the gate. It can edit the harness's state, put a
fake `cargo` on `PATH`, or disable hooks in `.claude/settings.local.json`. CI on a clean checkout
is the gate of record.

## What runs when

| Moment | Hook | What happens | Measured on this repo |
|---|---|---|---|
| Session starts | `SessionStart` → `fsmap hook session-start` | Records the baseline (only on `startup`, never on resume or compact). Tells Claude the real `rustc`/`cargo` versions and where each direct dependency's source is, and warns about settings that weaken the gate and about other sessions in the same worktree. Warms the brief index. | 0.4 s |
| After each `Edit`/`Write` | `PostToolUse` → `fsmap hook post-edit` | For `.rs`, `Cargo.toml` and `build.rs`: `cargo check --all-targets --offline` on the edited package. Errors go back to Claude as a block reason (first 20, de-duplicated); the fsmap brief for notable files goes along as context. One check at a time per worktree: a later edit waits up to 30 s for the running one, then checks the tree as it is. 60 s budget. | 1.1 s median |
| Claude ends a turn | `Stop` → `fsmap hook stop` | If the tree changed since the session started or last passed, runs the gate in quick mode. The gate passes only when every step passes. A failure blocks the stop with the failing steps' output, marking which files the session changed. After three blocked stops in a row it lets go and asks Claude to report; when every failure is in files the session did not change, it lets go after one block instead, telling the user the gate did **not** pass. A pass shows the user any added suppressions, ignored tests, removed assertions, and changed lint, format, cargo or toolchain configuration; those already pending when the session started are listed apart. While background agents (subagents, workflows) are still working, it waits for the real end of the turn. | 48 s |
| The user writes | `UserPromptSubmit` → `fsmap hook prompt` | Resets the failure streak. | ms |
| Before `git push`, `gh pr create` | `PreToolUse` (Bash) → `fsmap hook pre-bash` | Denies the command while build inputs are uncommitted (the gate checks the working tree; a push sends HEAD): manifests, `Cargo.lock`, cargo, rustfmt, clippy and toolchain configuration, every file a cargo configuration `include`s, every change to a tracked `.rs` file (renames, deletions and edits to `--skip-worktree` files included; an unchanged one, or one a sparse checkout leaves out, does not count), and untracked `.rs` files in a directory a target reads (a scratch file elsewhere does not count). Otherwise runs the quick gate unless this exact tree already passed, and denies on failure, when the gate cannot run, or when the hook itself fails. Commands are read with their quoting (`$'…'` too), comments, command substitutions (inside double quotes too), heredocs inside them (Claude Code's own `git commit -m "$(cat <<'EOF' … EOF)"`; a heredoc whose output a shell or `eval` runs, also behind wrappers (`timeout 60 bash`, `sudo -u x sh`) or through a pipe or process substitution, is a script), delimiter words (`<<'END-MSG'`, `<<\EOF`), arithmetic (`$(( … ))`, `(( … ))`), reserved words, wrappers and their options (`env -u`, `sudo -u`, `timeout -k` …), `sh -c`/`eval`/`env -S` scripts, here-strings, git's and gh's global options, `git -c alias.x=…` and `git subtree`. When the reading is unsure (a quote or substitution left open, nesting past 16 levels, a shell reading its script from stdin), mentioning a push is enough; a heredoc that never ends is not taken as data. Not publishing: heredoc bodies fed to other programs, `--dry-run`, `--help`, pushes that only delete remote refs, and `gh pr merge` (it merges what was already pushed). | 5 ms for other commands |

When cargo cannot read the project (a broken manifest, a malformed verify table, an unsafe
toolchain setting), the gate cannot run: the stop is blocked and a push denied with the reason,
never silently allowed. When git cannot fingerprint the tree (no repository, a broken git
config), the gate still runs, every time. A hook that crashes, by panic or otherwise, reports it
to the user; the pre-push hook denies a publishing command instead. A session judges as its own
everything changed since it was first seen (or since the empty tree when it started before the
first commit), except files that were already changed then and have not changed since.

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
`CARGO_HOME`; every `rustfmt.toml` and `clippy.toml` that can apply to a workspace member (in the
tree, git-ignored ones included, since rustfmt looks upward from each source file; in the
ancestors; in the home directory and rustfmt's user configuration directory), but not a vendored
crate's own; `rust-toolchain(.toml)` in the ancestors; directory overrides in rustup's
`settings.toml` (`rustup override set`); and every file a cargo configuration `include`s,
followed transitively and placed on its own, whatever its name. Where a file is decides how it
is judged (a link by the stricter of where it is and where it points):

| Where | Judged | `trust` applies |
|---|---|---|
| Tracked by git | No: it is in the diff and CI reads it too. Changes are reported on pass. | — |
| Inside the repository, untracked, ignored, tracked but marked `--skip-worktree`/`--assume-unchanged` (git hides its changes), or a tracked link to such a file (git tracks only the link) | Yes; any rustfmt, clippy or toolchain file fails whatever it says, and so does any `[env]` entry, any rustflag (`--cfg` too: `#[cfg(not(x))]` compiles a failing test out) and any `[target.*]` table | No: the agent is told to remove it and leave committing it to the user |
| Outside the repository (ancestors, `CARGO_HOME`, the home directory, rustup overrides) | Yes | Yes: this is the machine's own setup |
| A project without git | Yes | Yes |

What fails, by kind:

| Kind | Setting |
|---|---|
| `runner`, `linker` | for the host, a build target any configuration file or `CARGO_BUILD_TARGET` names, or a `cfg(…)` selector (a `wasm32` runner or an embedded target's link script does not apply to the gate's build); in a hidden file, for every target: a runner; a linker that is not a known one (`cc`, `gcc`, `clang`, `mold`, `lld` …, named or outside the repository), and linker flags other than `-fuse-ld=` with mold, lld, gold or bfd, by name or by a path outside the repository (a fake linker can replace the test binary) |
| `rustc-wrapper` | `build.rustc`, `build.rustdoc`, and a `rustc-wrapper` other than `sccache`/`cachepot` |
| `cap-lints`, `allow-flag`, `rustflag` | `--cap-lints`; `-A`/`--allow` and `--force-warn` (a force-warned lint stays a warning under `-D warnings`); any other rustflag outside a short safe list (`-C target-cpu`, `-C opt-level=0`, `--cfg`, `-W`/`-D` …), reported once per kind with the flags and their values. `-C opt-level` above 0 counts: it switches debug assertions and overflow checks off unless they are turned on explicitly. |
| `env`, `alias` | in a hidden file, any `[env]` entry (cargo passes them to rustc, clippy and the tests, `LD_PRELOAD` included); elsewhere, one the gate would remove from its own environment; an `[alias]` named after a command the gate runs (`fmt`, `clippy`, `deny` …) |
| `source` | replacing a source with a local directory or local registry (`cargo vendor` output), which can swap in modified dependencies; a registry mirror is fine |
| `config:<table>` | any other cargo key outside the allowlist (`[patch]`, `[unstable]`, `profile.*.debug-assertions` …) |
| `untracked-rustfmt-config`, `untracked-clippy-config`, `untracked-toolchain-config` | a rustfmt, clippy or toolchain file git does not track inside the repository (a toolchain pin to an official channel still changes which clippy runs); a clippy file above it |
| `rustfmt-outside` | a rustfmt file outside the repository (above it, in the home directory) when the repository has none of its own: CI formats without it |
| `fmt-disabled`, `fmt-ignore` | in a project without git: `disable_all_formatting`, a non-empty `ignore`, `skip_children` |
| `toolchain-path`, `toolchain-custom` | a toolchain `path`, or a channel that is not an official one or whose rustup directory is a link (`rustup toolchain link`, even under an official-looking name), in a toolchain file or a rustup directory override. Checked before the gate runs cargo at all. |

Every step runs without the variables that do the same from the environment, judged by name and
value: `RUSTFLAGS` and the other flag variables when they carry a flag the integrity step would
fail (`--cfg tokio_unstable` stays); `RUSTC_WRAPPER` unless it is sccache or cachepot; target
runners, and linkers that are not known ones; `CARGO_PROFILE_*` only for debug assertions,
overflow checks and `opt-level`; `CARGO_ALIAS_*`, the rest of `CARGO_BUILD_*` (jobs, target and
target directory stay); and always `RUSTC`, `RUSTDOC`, `RUSTFMT`, `RUSTUP_TOOLCHAIN`,
`RUSTC_BOOTSTRAP`, `CLIPPY_CONF_DIR`, loader and compiler-driver injection (`LD_PRELOAD`,
`DYLD_*`, `COMPILER_PATH`), snapshot-update switches (`INSTA_FORCE_PASS`, `UPDATE_GOLDENFILES`,
`TRYBUILD` …) and property-test case counts. A repository can keep one with `trust =
["env:NAME"]`. Steps also run without the variables that point git at another repository
(`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` …). git exports them to its hooks, and a gate run from
a hook in a linked worktree would let a test that commits in a scratch directory write to this
repository. Steps run with the build directory inside the repository, within a time budget
(600 s for the hooks). A step that overruns has its process tree killed; background processes it
left behind cannot hold the gate past the budget.

Timings: a 4-core cloud container, this repository. Session start and pre-bash on a clean clone
(2026-10-29), three runs each; post-edit from the live session's log; the gate is `fsmap verify`
run by hand with nothing to rebuild (2026-10-30, 143 tests), three runs: 47.1–47.8 s. Nearly all
of it is `cargo test`, whose 44 end-to-end hook tests each build a small Cargo project; a
repository without such tests pays its own test time.

## What it catches

Each row has an end-to-end test in [`tests/harness.rs`](../tests/harness.rs) or a unit test next
to the guard. 118 guards were checked by removing each and watching its test fail (D-15 lists
them); the rows below without one are noted.

| Shortcut or failure | Guard |
|---|---|
| Committing during the turn, so the tree looks clean | The fingerprint includes HEAD |
| The first commit of an empty repository | Unborn → born HEAD changes the fingerprint |
| Editing an already-modified file (no `git status` change) | Dirty files' contents are hashed |
| A hidden `.cargo/config.toml` with `--cap-lints allow`, `--force-warn`, `runner = "true"`, an `[alias]` for `clippy`, a fake linker, `opt-level` without checks, a local source replacement, `[env] LD_PRELOAD`; a nested or ignored `rustfmt.toml`; a clippy file above the repository; rustfmt rules from the home directory; a toolchain `path` or linked toolchain | The integrity step (TOML allowlist), never counted as someone else's failure and never trusted inside the repository; config files are part of the fingerprint whether git sees them or not |
| The same through `RUSTFLAGS`, `RUSTFMT`, `CARGO_TARGET_*_RUNNER`, `CARGO_ALIAS_*`, `LD_PRELOAD` | Variables removed for every step, judged by value |
| A toolchain file or a `rustup override` that runs the repository's own `cargo` | Refused before the first cargo call |
| Auto-compaction re-recording a broken tree as the baseline | Baseline written once, on `startup` |
| A session that started before the first commit, or before the Cargo project, or was first seen at a compaction, treating its own commits as someone else's | Changes are judged from where the session was first seen, or from the empty tree |
| A change already pending when the session started counted as the session's | Pending files are hashed at first sight; unchanged since, they are someone else's, and listed apart in the pass report |
| A tracked config that `include`s an ignored one; a tracked link to an ignored config; a config marked `--skip-worktree`; an edit to a skip-worktree source | Each file is placed on its own, a link by the stricter of link and target; skip-worktree files are hidden ones, and changed ones are hashed into the fingerprint and counted as uncommitted for a push |
| Re-editing a dirty file in a workspace below the git top | Dirty files are read from the work tree top, where `git status` names them |
| A dev server running in the background switching the gate off | Only background *agents* postpone it |
| The gate failing forever, or never again after one release | Three strikes then a report; the next user prompt resets |
| Pushing a broken commit, also from a session that started on it | The pre-push gate; only a recorded pass counts |
| Pushing HEAD while the gate checked uncommitted files (a module left untracked) | Push denied while build inputs are uncommitted |
| Pushing through `if … then`, `sudo -u`, `sh -c --`, `… \| sh`, `git -c alias.p=push p`, `"$(git push)"`, a line after `echo "<<X"`, a commit message with an apostrophe in `"$(cat <<'EOF' …)"`, a comment with an apostrophe | Push detection (unit tests per form; a parse left open falls back to "mentions a push") |
| The pre-push hook crashing on a push | The push is denied |
| A change that breaks a file it never touched (a helper left without a caller) | The gate passes only when everything passes |
| Failures from before the session in files nobody touched | Blocked once, then a report; the user is told the gate did not pass |
| A broken manifest or verify table; a git configuration git cannot read | The stop is blocked and a push denied; without a fingerprint the gate runs anyway |
| A suppression, `cfg(false)`, an ignored test, a deleted assertion, a lint allowed or checks switched off in a manifest, also in new untracked files | Shown to the user on pass |
| Two sessions in one worktree | Announced at session start; state updates under a lock keep both records |
| fsmap not installed, an older fsmap without `fsmap hook`, or a hook that crashes | Session start says the harness is OFF; the other hooks stay out of the way; a crash is reported (template test, not in the mutation run) |

## Setting it up in a Rust repository

1. Install fsmap where the agent runs:
   `cargo install --locked --git https://github.com/d-j-lee/filesystemMap fsmap`. In a Claude Code
   cloud environment, put that line in the environment's setup script.
2. Copy from this repository's [`harness/`](../harness/):
   - `settings.json` → `.claude/settings.json` (merge by hand if you already have one);
   - `agents/rust-reviewer.md` → `.claude/agents/rust-reviewer.md`;
   - `AGENTS.md` → `AGENTS.md`, and add `@AGENTS.md` at the top of `CLAUDE.md`.

   The hook commands are POSIX shell (`fsmap hook … || { … }`); on Windows, Claude Code runs them
   with Git Bash.
3. Add the lints from this repository's `Cargo.toml` (`[lints.clippy]`), its `clippy.toml`, and a
   `deny.toml` (start from this repository's and adjust the licence list).
4. Commit the project's own `.cargo/config.toml`, `rustfmt.toml`, `clippy.toml` and
   `rust-toolchain.toml` if it has them: untracked, they fail the gate.
5. Optional: extra steps, and kinds the machines' own setup needs, in `Cargo.toml`:

   ```toml
   [package.metadata.fsmap.verify]   # or [workspace.metadata.fsmap.verify]
   extra = [["node", "--check", "web/app.js"]]
   trust = ["linker"]   # e.g. a linker wrapper in ~/.cargo/config.toml on the build machines
   ```

   Only `extra` and `trust` are accepted; anything else, or a malformed entry, is an error.
   `trust` covers settings outside the repository (cargo home, directories above it, rustup
   overrides, the environment), never untracked files inside it. sccache, mold, lld, `--cfg`
   and registry mirrors need no trust.
6. In CI, keep explicit fmt/clippy/test steps (the gate of record) and add a job that runs
   `fsmap verify --mode full --strict`.

Check the first session: Claude's context should start with `[harness] fsmap … hooks are active`.
`fsmap hook stats` summarises what the hooks did (blocks, passes, skips, median times) from the
log in `<git-dir>/fsmap-harness/log.jsonl`.

## Limits

- **A guardrail, not a security boundary.** The agent can edit what the hooks read (its state
  directory, `.claude/settings.local.json`, which git usually ignores). `PATH`, the installed
  toolchain and settings outside the files above are trusted. The ask rules make the obvious
  edits visible; CI is the gate of record.
- **Tracked weakening is reported, not blocked.** A lint allowed in `Cargo.toml`,
  `debug-assertions = false` in a profile, or a committed `.cargo/config.toml` with a runner
  passes the gate with a note to the user, because CI runs the same files and would pass too.
  The ask rules make the agent's edits to these files ask first, but a shell command can still
  write them. Review the note.
- **What the pre-push check cannot see.** A git alias for `push` in the git configuration, a
  script file or a shell function that pushes, a push of a ref other than the checked-out
  one, and shell forms the detector does not model: it is a heuristic over shell syntax, and
  six review rounds each found new ones. When it cannot read a command to its end it falls
  back to "mentions a push", which errs toward gating. A git `pre-push` hook as a second
  detector is a ROADMAP seed. A new untracked source outside every target's directory (`#[path]`, `include!`, a path
  dependency that is not a workspace member) is not counted as a build input. The stop gate
  still runs at the end of the turn.
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
  not (common with Git LFS) can make the gate run again for nothing. Likewise a
  `--skip-worktree` file under `core.autocrlf` can count as changed for a push, since it is
  compared with the index without filters.

## Evidence

The design was reviewed by an independent agent before implementation (3 blockers, 8 major
findings), and the implementation four times after it, each time by a fresh agent: 2 blockers
and 6 major findings, then 1 blocker and 10 major findings, then 8 findings and a list of minor
ones from a review scoped to whether the code matched the threat model above, then 1 blocker
and 7 major findings from a convergence check of that round's fixes, then 3 major and 4 minor
findings, and no blocker, from a convergence check of those, then 3 major findings and no
blocker from a last check limited to blockers and majors. Among them were
bypasses of the integrity check, legitimate configurations it failed, fixes whose tests did not
fail without them, and claims in this guide that were not true. What was adopted, and what was
not, is in D-15.

A `rustup override set` to a linked toolchain was tried live in this container: `fsmap verify`
refused before any cargo call.

The hooks took effect in the Claude Code cloud session that wrote them, without a restart. Its log
shows:

- the first `Edit` after `.claude/settings.json` existed running `post-edit` (cargo check, median
  1.1 s over 10 edits), and later blocking an edit that broke two callers;
- an automatic compaction firing `SessionStart` with source `compact`, which left the baseline
  alone and told Claude the tree had unverified changes;
- the first turn end firing `Stop` while a review subagent ran in the background: the gate was
  postponed (`skip-background`), which also confirms that Claude Code lists subagents in the
  Stop payload's `background_tasks`.

- the pre-push hook running the full gate three times on commands that only mentioned a push
  inside a commit message or a heredoc (37, 42 and 56 s), with the installed build from before
  heredoc bodies were treated as data. The third run blocked the command and caught a
  formatting slip in a commit that had just been made.

A gate run at a live turn end had not been logged yet when this was written.
