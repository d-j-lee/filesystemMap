# CLAUDE.md — working on fsmap

fsmap is a Rust CLI. It maps a directory into an arena tree, enriches it (classification,
content metrics, git status and history, imports, duplicates), derives findings, and renders
HTML/JSON/Markdown/SARIF. Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) before structural
changes.

## Commands

```bash
cargo check                                   # inner loop
cargo clippy --all-targets -- -D warnings     # must be clean
cargo fmt                                     # rustfmt defaults
cargo test                                    # unit + end-to-end (tests/cli.rs) + review regressions
node --check viewer/app.js                    # viewer syntax (embedded via include_str!)
cargo build --release && ./target/release/fsmap report . -q -o /tmp/r.html
./target/release/fsmap insights . --fail-on high   # dogfood: the repo must stay clean
```

Iterate in that order: get `cargo check` clean, then clippy, then tests. Do not jump to a
release build to "see if it works".

## Map

| Path | Role |
|---|---|
| `src/model.rs` | `Tree`/`Node` arena (pre-order, `span`), zones, flags, roll-up |
| `src/scan.rs` | Parallel walk, hard links, mounts, placeholders |
| `src/classify.rs` | Language table (`#[rustfmt::skip]`), categories, zones + reasons |
| `src/strip.rs` | Comment stripper + line stats (one pass) |
| `src/content.rs` | Reads files once; generated/binary detection; import extraction |
| `src/git/` | `mod.rs`: hardened `git()` runner, discovery, status, hygiene. `history.rs`: `--raw` log parsing, renames, DOA, truck factor, coupling |
| `src/deps/` | `extract.rs`: regexes. `mod.rs`: resolver per ecosystem, lifted-graph cycles |
| `src/analysis.rs` | Pipeline order, hotspot percentiles, author modes |
| `src/insights.rs` | Rules → `Finding` (stable ids), baseline |
| `src/prune.rs`, `src/snapshot.rs` | Node budget; JSON contract and id remapping |
| `src/report/` | terminal, html (CSP), context (agents), sarif |
| `src/brief.rs`, `src/diff.rs` | Briefing/hook/cache; snapshot diff |
| `src/dupes.rs`, `src/util.rs` | Duplicate detection; formatting and sanitising helpers |
| `viewer/` | Report UI, vanilla JS, no dependencies |
| `tests/cli.rs` | End-to-end against a generated git fixture |
| `tests/review_regressions.rs` | One test per defect found by independent reviews (D-13, D-14) |

## Rules

- **No `unsafe`** (forbidden in `Cargo.toml`). No `todo!`/`unimplemented!`/`dbg!` (denied).
- **Every git call goes through `git::git()`.** It pins safe config for untrusted repositories.
  Commands that compare the work tree with the index (`status`, `diff` against the work tree)
  use `git::git_worktree()`, which also disables the repository's filter drivers (D-13).
  Never call `Command::new("git")` directly in library code.
- **Determinism**: sort anything that reaches an output; `context` must stay byte-identical for
  the same tree (there is a test).
- **Never put file contents or commit messages into outputs.** Paths and metrics only.
- **Viewer**: render data with `textContent`/`h()` only — never `innerHTML` with data. Colours
  come from CSS tokens; any new colour needs a light and a dark value.
- **Snapshot schema**: additive changes keep `fsmap/1`; anything else bumps it. Update
  `docs/snapshot-schema.md` in the same change.
- **Heuristics need evidence**: when changing a metric or rule, run it on a real repository
  (e.g. a clone of ripgrep or flask) and compare against git ground truth, not only unit tests.
  Real defects were found this way (D-10 on flask, D-14 on fsmap itself); the end-to-end
  fixture caught another (D-09).

## Keeping the docs alive

- `README.md`, `docs/ARCHITECTURE.md` and `docs/snapshot-schema.md` describe **the current
  state**. Edit them in place; they are not changelogs.
- `docs/decisions.md` is append-only. Add `D-NN` entries with alternatives and costs; mark
  replaced ones **Superseded by D-xx** instead of deleting them.
- `docs/ROADMAP.md` holds seeds. Delete ideas that stop making sense; move shipped ones into
  the architecture doc.
- `docs/research/` is dated and never edited after the fact. Add a new dated file instead.
- Numbers in the current-state docs (timings, counts, example output) are measurements. When
  a change can move one, re-run the command and update it with its conditions (machine,
  sample size). Drift here was the main finding of the D-14 audit.
- Before a large docs change, have an independent agent compare every claim with the code
  and the binary. It finds what the author no longer sees.
