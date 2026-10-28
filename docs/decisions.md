# Decision log

Each entry: the call, what it was weighed against, and what it costs us. Newest last.
Superseded entries stay, marked **Superseded by D-xx**, so the reasoning trail survives.

## D-01 Rust, single crate (lib + bin)

- **Decision**: one package with `src/lib.rs` (all logic) and `src/main.rs` (CLI only).
- **Why**: the project is the worked example for "Rust is affordable once AI writes most
  of it" — see [rust-ai-playbook.md](rust-ai-playbook.md). Parallel walk + hashing +
  graph work is where Rust's speed and fearless concurrency pay; a single static binary
  is easy to drop onto any server.
- **Alternatives**: Cargo workspace (core / cli / mcp) — deferred until a second
  front-end (MCP server, WASM) actually exists; splitting earlier adds churn for no user.
- **Cost**: the lib's public API is not yet stable; `publish = false` until it is.

## D-02 One map, many lenses (zones)

- **Decision**: a single tree that covers *everything* on disk, with each node tagged by
  zone (`source`, `dependency`, `artifact`, `vcs`, `ignored`). Disk-usage questions
  use the whole tree; code questions filter to `source`.
- **Why**: the useful answers live in the overlap — "62 % of this repo's footprint is
  ignored build output", "a 300 MB binary is tracked in git".
- **Alternatives**: separate "disk" and "code" modes (dust vs tokei) — loses the overlap.

## D-03 Shell out to the `git` CLI instead of linking libgit2 / gitoxide

- **Decision**: run `git ls-files` and `git log --numstat -z` and parse NUL-delimited output.
- **Why**: zero C build deps, fast compile, exact parity with the user's git config
  (`.mailmap`, `safe.directory`, sparse checkouts, partial clones); `-z` output is
  stable and unambiguous.
- **Alternatives**: `git2` (C dependency, slower compile), `gix` (pure Rust, heavy
  compile, API still moving). Revisit if we need blame or object-level access.
- **Cost**: requires `git` on `PATH` for git features (gracefully skipped otherwise).

## D-04 Import extraction by comment-stripping + regex, not tree-sitter (v1)

- **Decision**: per-language regexes over comment-stripped text; resolution against the
  scanned file set (relative paths, Rust `crate::`/workspace crates, Python packages,
  JS/TS relative + workspace packages, Go module path, Java/Kotlin packages, C/C++
  quoted and angle includes).
- **Why**: imports are syntactically shallow; the hard part is *resolution*, which
  tree-sitter does not do. Regex keeps the binary small and the build C-free.
- **Alternatives**: tree-sitter grammars (+5–10 MB binary, C toolchain, minutes of
  build), LSP/compiler front-ends (exact but per-language heavy).
- **Cost**: heuristic — a few false edges (imports in strings) and misses (macro-built
  includes, dynamic imports with variables). Documented as such in the report.
  Upgrade path: swap the extractor per language behind the same `Extractor` trait.

## D-05 Self-contained HTML with a hand-written canvas viewer

- **Decision**: the report is one `.html` file: template + vanilla JS + CSS compiled
  into the binary, snapshot JSON embedded. No CDN, no server.
- **Why**: works offline, attachable to a ticket or chat, survives air-gapped
  corporate networks, nothing to install for the reader.
- **Alternatives**: D3 from CDN (blocked offline / by CSP), a local web server
  (friction), a TUI (not shareable).
- **Cost**: we own ~1.5k lines of JS (squarified treemap, DSM, tables).

## D-06 Default analysis window: latest 10 000 non-merge commits — **Superseded by D-08, D-09**

- **Why**: bounded cost on monorepos (Linux, Chromium) while covering years of history
  on normal repos. Files untouched inside the window are reported as "unchanged since
  at least <window start>", which is the honest statement.
- **Override**: `--git-max-commits`, `--since`.

## D-07 Stay on one filesystem by default

- **Why**: scanning `/` must not wander into `/proc`, network mounts or bind-mount
  loops. `--cross-fs` opts in. (Differs from `du`, matches `du -x`.)

## D-08 Review round 1 (adversarial design review) — what changed

An independent reviewer attacked the first draft. Accepted:

| Finding | Change |
|---|---|
| Git model assumed one repo; contradicts "whole filesystem" | Discover every work tree in the scan (`.git` dir or file); analyse each with `git -C <toplevel>`; submodule gitlinks excluded from history. |
| 10 k-commit window ⇒ "inactive owner" can never fire on busy repos | Window is time-based (`--since`, default 2 years) with a commit cap only as a safety limit; hotspots use recency-weighted revisions so a long window does not dilute "now". |
| Stage order: zones needed git state before content | Pipeline is scan → classify → **git status** → content → history → deps. `zone` now means *origin* only; git state is a separate field; analysis is gated on both. |
| User git config can break parsing / run programs in untrusted repos | Fixed invocation prefix: `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, `core.fsmonitor=false`, `--no-show-signature --no-relative --no-textconv --no-ext-diff`, inherited `GIT_DIR`-style env removed. |
| `--numstat` lazily downloads every blob in partial clones; slow on monorepos | History uses `--raw` (tree diffs only). Line churn is dropped; size comes from the current file instead. |
| Shallow clones: boundary commit shows every file as added | Commits listed in `.git/shallow` are skipped; the repo is marked `shallow`. |
| Renames joined across re-created paths | Newest→oldest walk with an alias map; an `A` (creation) event tombstones the path so older events cannot leak in. |
| Hotspot max-normalisation lets one outlier set the scale | Percentile × percentile of recency-weighted revisions (half-life 90 days) and indentation complexity; flagged only when both ≥ P80. Generated/lock/minified files excluded. |
| Coupling `shared/avg` hides direction | Directional confidence `conf(A→B)=n_AB/n_A` plus lift; "hidden" only when both languages have extractors, no import path ≤ 2 hops, and not a test↔subject pair. |
| Ownership by top committer is naive | Degree-of-Authorship (Fritz et al.; Avelino et al. 2016) and truck factor per repo / top-level directory. Knowledge risk fires only when a file has no active author **and** is a hotspot or has high fan-in. |
| Mass reformat / bot / ignore-rev commits distort everything | `.git-blame-ignore-revs` and `[bot]` authors are skipped; commits touching > 50 files don't count as revisions; coupling uses commits touching ≤ 30 files. |
| Mounts, placeholders, hostile names | Linux mount points from `/proc/self/mountinfo` are never stat'ed unless `--cross-fs`; cloud placeholder files are never read; names are escaped in HTML (`textContent` only) and terminal output. |

Adopted from the reviewer's novelty list (each is cheap given the shared snapshot):
`fsmap brief` (pre-edit briefing incl. "files that usually change with this one" — the
*you forgot B* check), a **baseline ratchet** (stable finding IDs + `--baseline` + exit
codes, so CI fails only on *new* cycles/findings — the Chromium `checkdeps` idea without
a rules language), **multi-repo hygiene** (dirty trees, unpushed branches, stashes,
worktrees, rebuildable GB), and an **agent cost map** (tokens per directory + paths agents
should skip).

Rejected / deferred, with reason:
- *Default authors to hashed*: kept `names` as default because "who to ask" is the main
  use; `--authors hashed|none` exists for sharing outside the team. Emails are never output.
- *`--first-parent`*: would attribute merged PRs to whoever pressed merge; we use
  `--no-merges` and accept that fix-up commits inflate revisions slightly.
- *Drop duplicates / C++ / Java resolution from v1*: kept — cheap, heuristic, labelled as such.
- *Side-table node layout (≤ 64 B/node)*: deferred to ROADMAP; current ~100 B/node is fine
  below ~5 M entries.

## D-09 The history window is measured from each repository's newest commit

- **Decision**: `--since 2y` (default) means two years before the newest commit of *that*
  repository, not two years before today. Absolute dates (`--since 2024-01-01`) and `all`
  remain available.
- **Why**: the end-to-end tests caught this. A wall-clock window gives a repository with no
  recent commits (archived, vendored, a fixture) no history at all. Every history-derived
  metric then silently goes to zero. The compiler cannot see this class of bug.
- **Cost**: one extra `git log -1` per repository.

## D-10 Deferred imports do not form cycles

- **Decision**: these imports are recorded as *weak* edges and excluded from cycle detection
  and the dependency matrix:
  - Python imports that are indented (inside functions or `if TYPE_CHECKING:`);
  - JS/TS dynamic `import()`.
- **Why**: on flask, the first version reported a 15-module "cycle". All of it came from
  `TYPE_CHECKING` imports, and none of it could fail at import time. Cycles are reported only
  where they have runtime meaning.
- **Related**: references from a child module to its own parent module file (Rust `super::`
  into `mod.rs`/`lib.rs`, Python `__init__.py`) are marked *up* and excluded the same way.
  Module cycles inside one Rust crate are reported, but at low severity, because they compile.

## D-11 Viewer: findings first, map second, system fonts, validated palette

- **Decision**: the report opens on ranked findings with evidence and copyable commands. The
  treemap is the second tab.
- **Why**: the reviewer noted that pictures don't drive decisions and that small hot files
  are invisible by area. Tables and the scatter cover what the treemap hides.
- **Map defaults**: a repository opens as a code map (size = code lines, colour = language).
  Any other directory opens as a disk map.
- **Look**: system font stacks keep the report offline and CSP-clean. Colours come from a
  colour-blind-validated categorical palette with fixed slot order. Text is never coloured
  by series.

## D-12 Language of the documents

- **Decision**: technical documents (README, architecture, schema, decision log, agent
  guide, CLAUDE.md) are in English. They sit next to code identifiers, and agents and
  reviewers outside the team read them. The personal adoption guide
  ([rust-ai-playbook.md](rust-ai-playbook.md)) is in Korean, the owner's working language.
- **Revisit** if the repository's audience is only the owner's team.
