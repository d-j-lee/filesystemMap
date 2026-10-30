# AGENTS.md — working rules for coding agents in this Rust repository

These rules are tool-neutral (Claude Code, Codex, Cursor and others read this file). They come
from the fsmap agent harness: <https://github.com/d-j-lee/filesystemMap> (`docs/agent-harness.md`).

## The loop

1. Edit, then `cargo check`. With the Claude Code hooks installed this happens after every edit
   and errors come back to you automatically.
2. Before you say a change is done, run the gate: `fsmap verify` (fmt, clippy `-D warnings`,
   tests, this repository's extra checks). With the hooks installed, finishing a turn runs it.
3. For a non-trivial change, ask the `rust-reviewer` subagent for an adversarial review after the
   gate passes, and address what it finds.

## Rules

- **Current over remembered.** Use the toolchain and dependency versions reported at session start.
  When unsure about a crate API, read its source for the version in `Cargo.lock` under
  `~/.cargo/registry/src/*/<crate>-<version>/` before using it.
- **Dependencies**: add them only with `cargo add`, and say why. Never edit versions by hand.
- **Types first.** For new behaviour, propose the types, traits and public signatures before the
  implementation. Model states as enums and validated values as newtypes.
- **Borrow errors**: fix the ownership design first. If you clone or reach for `Rc<RefCell<_>>`,
  `Arc<Mutex<_>>` or `Box::leak` to get past the borrow checker, say so and why.
- **Never suppress to get green.** No `#[allow]`; use `#[expect(lint, reason = "...")]` only when
  the lint is wrong for this code, with a reason that would convince a reviewer. Never skip, ignore,
  weaken or delete a test to make it pass.
- **Never push around the check.** No `--no-verify`, no `core.hooksPath` change, no unsetting
  `CLAUDECODE`. If the git pre-push hook refuses a push, make the gate pass; if the refusal looks
  wrong, tell the user.
- **Errors**: no `unwrap` on external input (files, network, user, other processes). Add context
  when propagating (`.context("reading the index")`). Libraries use typed errors (`thiserror`),
  binaries `anyhow`.
- **unsafe**: forbidden by default. If it seems necessary, stop and propose it first: why, the
  invariants, and how it will be isolated and tested.
- **async**: only when the problem needs it.
- **Stuck**: after three failed attempts at the same failure, stop. Report what fails, what you
  tried, and what you think the cause is.
- **Report honestly.** Say which commands you ran and what they showed. Say what you did not
  verify. A pre-existing or out-of-scope failure is reported, not hidden.
