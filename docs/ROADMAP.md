# Roadmap

Seeds, not promises: ideas that fit the architecture, with the hook where they would
attach. Promote one by opening an issue that cites it; delete it when it stops making sense.
Ordered by expected value per effort.

## Next

| Idea | Why | Attach point |
|---|---|---|
| **MCP server** (`fsmap mcp`, `rmcp` crate) | Agents ask "what changes with this file?", "who owns this?", "is there a cycle if I import X?" as tool calls instead of reading Markdown | `brief::Index` already holds partners, importers and imports per file; add findings to it, then expose `brief`, `partners`, `importers`, `findings` |
| **Incremental cache keyed by HEAD** | `report`/`context` on monorepos in seconds; `brief` already caches | Persist `history::Output` + content metrics per blob id in `$XDG_CACHE_HOME/fsmap` |
| **Diff treemap** (`fsmap diff --html`) | "What grew since last week" is the most useful disk view | Two snapshots → per-node delta as a colour mode in the viewer |
| **Chromium / monorepo profile** | The project's origin story; stress test for 400k-file trees | Partial clone (`--filter=blob:none`), `--git-max-commits`, GN `deps` extractor (below) |
| **Upward-dependency rules** (`fsmap rules infer` / `check`) | Chromium-style `include_rules` without writing them by hand: infer layers from today's DSM, fail on new upward edges | Lifted sibling graph in `deps::find_cycles` |

## Later

| Idea | Why | Attach point |
|---|---|---|
| Harness: per-package tests | Large workspaces: run the tests of changed packages and their reverse dependencies at Stop, the whole workspace in CI (D-15) | `cargo metadata` resolve graph → `verify::plan` |
| Harness: workspaces below the root | A repository with several Cargo workspaces, or one in a subdirectory, has no gate today; the git pre-push shim names one workspace, the last session's | Find workspaces from changed paths in `hooks::gate`; the shim passes every registered workspace, and `check_push` judges the ones a push changes |
| Harness: user-only bypass for work in progress | A keyword in the user's prompt pauses the stop gate for one turn; only the user can type it | `hooks::prompt` sets a per-turn flag in `state` |
| Harness: wider fingerprint | Path dependencies outside the repository and non-Rust `include_str!` inputs do not re-trigger the gate | `cargo metadata` path packages → `state::fingerprint` |
| Harness: gate in a clean worktree | Verifies a push on exactly the commit it sends (each commit it publishes, not only the tips), and keeps ignored files away from the gate. Set aside in D-16 after a review: absolute `include`s and links still reach the original repository, so the placement code stays; it needs a lock, submodule and sparse handling, and a second cold build per cloud session (about +50 s, 0.9 GB here) | `git read-tree -u` into a private directory (no registered worktree), keyed per git directory |
| Harness: verdicts by tree everywhere | The stop and pre-bash gates key on a fingerprint that includes HEAD, so a commit after a pass reruns the gate before a push; git's hook already accepts a verified tree (D-16) | `hooks::gate`: accept a recorded tree verdict for the work tree before running |
| Harness: hold the session's own config changes | A committed `.cargo/config.toml` or toolchain file is reported, not judged (CI runs it too); one the session itself changed could block once, like failures in untouched files, so the agent has to say why | `hooks::gate`: judge config files in `touched` with `integrity`'s rules |
| Harness: baseline diagnostics | Tell a failure the session caused from one it found, soundly: run clippy and fmt once on a worktree of the session's start and compare diagnostics | `git worktree add` under the state directory, cached per start commit |
| Harness: faster self-tests | The 67 end-to-end hook tests make this repository's own gate about 63 s; a shared prebuilt fixture would cut most of it | `tests/harness.rs` fixture |
| Harness: does the gate help? | Measure blocks, false blocks, time added and bypasses across real sessions before claiming value | `fsmap hook stats` over many logs |
| tree-sitter extractors | Fewer heuristic misses (macro-built includes, JS re-exports) | Swap per language behind `deps::extract::extract` |
| Exact package graphs (`cargo metadata`, `go list`, `compile_commands.json`) | Ground truth where available | Feed `Resolver` instead of manifest parsing |
| GN / Bazel `deps` edges | Build-level architecture of Chromium-like trees | New `ImportLang::Gn` |
| WASM build of the core | Drag a folder into the browser; nothing installed, nothing uploaded | `scan` over the File System Access API; `content` and `dupes` read files and `git` spawns processes, so those need a file-source trait first (history would be unavailable or use a JS git) |
| PyO3 bindings | `fsmap.scan(path)` in notebooks | `analysis::run` → Python dicts |
| Side-table node layout (≤ 64 B/node) | 10 M-entry scans (whole disks) in < 1 GB RAM | Move content/git/deps fields out of `model::Node` |
| Worktree grouping by `--git-common-dir` | Avoid re-reading identical history for each linked worktree | `git::discover` |
| `fsmap.toml` overrides | Zone/rule overrides per repo (e.g. "`build/` is source here") | `classify::zone_for_dir`, `insights` |
| Largest historical blobs | Explain a bloated `.git` precisely | `vcs-bloat` rule: `rev-list --objects` + `cat-file --batch-check`, time-boxed |
| TUI (`ratatui`) | Explore over SSH without a browser | Same snapshot, terminal treemap |

## Considered and declined

- **Embedded Chromium for AI browsing** (the other idea from the initial brainstorm):
  existing CDP/Playwright tooling covers it; owning a browser wrapper buys little.
- **Default-hashed author names**: see D-08 — names stay default; `--authors hashed` exists.
