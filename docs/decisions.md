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
- **Also excluded**: TS `import type` / `export type` and inline `{ type X }` clauses (erased
  by the compiler), and any edge touching test code (tests depend on everything; on hono a
  single `types.test.ts` importing the package root closed a 13-module "cycle").
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

## D-13 Review round 2 (adversarial code review) — what changed

An independent reviewer attacked the implementation with crafted fixtures and the same real
repositories. Every item below has a regression test in `tests/review_regressions.rs`.

| Finding | Severity | Fix |
|---|---|---|
| `git status` / `git diff` against the work tree run `filter.<x>.clean` from an untrusted repo's config → code execution on scan | blocker | Work-tree commands go through `git::git_worktree`, which neutralises every configured filter driver (`-c filter.X.clean=` …). Verified: the hostile driver no longer runs. |
| A file renamed onto a path that once held a deleted file inherited that file's history | major | Rename destinations are tombstoned; alias values are stored fully resolved so tombstones never cut a file renamed *through* a path. |
| Scanning a subdirectory lost history from before a move into it (`log -- <prefix>` turns the move into an add) | major | The main `git log` is never pathspec-limited; paths outside the scan are dropped on join. |
| Partial clones: `-M` needs blobs (fails with lazy fetch off); `remote.*.promisor` not detected | major | Detect both markers; partial clones use exact renames (`-M100%`); fall back to it on failure. |
| Headline totals computed on the pruned tree | major | Totals always come from the full tree. |
| Go imports target package directories → never "linked", fan-in 0 → false hidden coupling | major | An edge to a directory reaches its direct files for linking and fan-in. |
| Empty/broken nested `.git` made git fall back to the parent → parent history counted twice | major | Nested work trees are kept only when `rev-parse --show-toplevel` names that directory. |
| `brief --hook` outside a work tree re-analysed on every call | major | Silent no-op outside git. |
| JS regex `/\/*$/` opened a block comment, swallowing code and imports | minor | Regex literals recognised from the preceding token. |
| tsconfig without `baseUrl` resolved bare specifiers to local folders | minor | Bare-specifier fallback only with `baseUrl`; `paths` relative to the config dir otherwise. |
| Git-ignored scan root showed everything as untracked | minor | The prefix itself maps to the scan root. |
| `--since` reaching before 1970 silently dropped history | minor | Such windows mean "all". |
| File names could inject Markdown (headings, code-span breaks) into agent-facing output | minor | Names are sanitised (no control characters, no backticks) in Markdown/brief/hook text and finding text. |
| A root directory named `{{DATA}}` broke the HTML template | minor | Single-pass template fill. |
| Hook ignored projects reached through a symlink | minor | Hook paths are canonicalised (via the parent for files not yet written). |
| Submodule/worktree `.git` *file* counted as untracked source | minor | Classified as VCS. |
| Pinned nodes could exceed the node budget | minor | Pins are prioritised and stop at half the budget. |

## D-14 Review round 3 (documentation audit) — what changed

An independent agent compared every claim in the current-state docs with the code and the
binary (CLI help, real `scan` output on ripgrep/flask/hono/redis, crafted fixtures). It
reported 26 mismatches between docs and behaviour and 4 code defects. Docs were corrected in
place; each code defect has a regression test in `tests/review_regressions.rs` that fails on
the previous code.

| Finding | Fix | Alternative and why not |
|---|---|---|
| A tracked `dist/` (next to `package.json`) or `target/` was returned to source, so committing build output raised no finding. `target`'s reason string was identical to the "ambiguous" one by accident. | Only `build/` is settled by git tracking (hono keeps build scripts there). `target/`, `dist/`, `out/` and .NET `bin/`/`obj/` stay artifacts and are reported when committed. The two reasons are distinct constants with a unit test. | Keep all five ambiguous and fix the docs: rejected, the evidence (hono) only covers `build/`, and a committed `dist/` is the case the ratchet exists for. Repos that commit `dist/` on purpose (GitHub Actions) baseline it. |
| `deps` was vendored at any depth, so fsmap's own `src/deps/` counted as third-party and its cycles would have been downgraded. | `deps` is vendored only beside a C/C++ build file (`Makefile`, `CMakeLists.txt`, `configure`, `meson.build`) and is a dependency directory beside `mix.exs`/`rebar.config`. redis's `deps/` is unchanged. | Drop `deps` entirely: rejected, redis's `deps/` (1.9M tokens of vendored code) would become "source". |
| `--authors none` still wrote `repos[].history.truckAuthors`. | Cleared with the other author data. | — |
| Coupling pairs serialised `"linked": null`; the schema says absent. | Omitted when unknown. Additive per the schema rule (readers treating null and absent alike are unaffected), so the schema stays `fsmap/1`. | Document `null`: rejected, absent is the convention for every other optional field. |
| `diff` printed a raw timestamp and suggested a non-existent `scan --json`. | Dates and a readable span; the hint names `scan`. | — |
| `brief --hook` ran git 4 times per edit (HEAD read 3×). | One `rev-parse --show-toplevel --short=12 HEAD`; warm hook on ripgrep 19.8 → 7.9 ms (median of 30). Found while re-measuring the documented 14 ms. | — |

Doc corrections, by kind:

- **Stale after later changes**: per-repository hotspot ranking, the prune pin budget, weak
  edges (TS type-only, test code), partial-clone history, test files in CLAUDE.md, the
  `git_worktree` rule, line counts and timings.
- **Wrong from the start**: the `agent-ignore` description (it emits directories only, and
  costs only in the gitignore format), the "I/O-agnostic" ROADMAP claim, which tests found
  D-09.
- **Unsourced playbook claims**: re-checked against primary sources in
  [research/rust-ai-evidence-addendum-2026-10-28.md](research/rust-ai-evidence-addendum-2026-10-28.md),
  or marked as rules of thumb. The CRUST-Bench "3×" compared different models and was
  removed.
- **Process**: CLAUDE.md now treats doc numbers as measurements to re-run, and asks for an
  independent claim-by-claim audit before large docs changes.
