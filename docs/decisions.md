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
place. A second independent agent then reviewed the fix commit itself and found two more
problems, one of them a regression the first fix introduced (the `deps/` row). Each code
defect has a test in `tests/review_regressions.rs` or `tests/cli.rs` that fails on the
previous code; the hook's git-call count was checked with `strace`, not a test.

| Finding | Fix | Alternative and why not |
|---|---|---|
| A tracked `dist/` (next to `package.json`) or `target/` was returned to source, so committing build output raised no finding. `target`'s reason string was identical to the "ambiguous" one by accident. | Only `build/` is settled by git tracking (hono keeps build scripts there). `target/`, `dist/`, `out/` and .NET `bin/`/`obj/` stay artifacts and are reported when committed. The two reasons are distinct constants with a unit test. | Keep all five ambiguous and fix the docs: rejected, the evidence (hono) only covers `build/`, and a committed `dist/` is the case the ratchet exists for. |
| (second review) A JavaScript GitHub Action commits `dist/` on purpose; actions/checkout got a medium finding advising `git rm`, which would break the action. | `dist/` beside `action.yml` stays an artifact (its bundle is not hand-written: treating it as source added 30k "code" lines to checkout) with its own reason, and is not reported as committed. | Return it to source: rejected for the line-count distortion. Ask users to baseline it: rejected, the advice itself was wrong. |
| `deps` was vendored at any depth, so fsmap's own `src/deps/` counted as third-party and its cycles would have been downgraded. | `deps` stays vendored unless a sibling marks it as a module of its parent (`lib.rs`, `main.rs`, `mod.rs`, `deps.rs`, `__init__.py`, `index.{js,ts,…}`); beside `mix.exs`/`rebar.config` it is a dependency directory. | First fix, superseded within this PR: vendored only beside `Makefile`/`CMakeLists.txt`/`configure`/`meson.build`. The second review showed it turned the vendored C in gevent (`setup.py`), better-sqlite3 (`binding.gyp`) and autotools checkouts into "source" (gevent: source code 184k → 327k lines). It was checked on redis alone. Dropping `deps` entirely: rejected for the same reason. |
| `--authors none` still wrote `repos[].history.truckAuthors`. | Cleared with the other author data. | — |
| Coupling pairs serialised `"linked": null`; the schema says absent. | Omitted when unknown. Additive per the schema rule (readers treating null and absent alike are unaffected), so the schema stays `fsmap/1`. | Document `null`: rejected, absent is the convention for every other optional field. |
| `diff` printed a raw timestamp and suggested a non-existent `scan --json`. | Dates and a readable span; the hint names `scan`. | — |
| `brief --hook` ran git 4 times per edit (HEAD read 3×). | One `rev-parse --show-toplevel --short=12 HEAD`; warm hook on ripgrep 19.8 → 7.9 ms (median of 30). Found while re-measuring the documented 14 ms. | — |
| (second review) `agent-ignore --format gitignore` wrote `/deps/  # reason`; gitignore has no trailing comments, so no rule matched anything. | The reason goes on its own comment line; a test runs the output through `git check-ignore`. | — |
| (second review, minor) `diff NEW OLD` printed "under a day apart" for a 40-day gap; "1 days". | Absolute span with a note when NEW is older; singular day. | — |

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
- **Lesson**: a fix to a classification heuristic is itself a heuristic change. The narrowed
  `deps/` rule was checked on one repository and regressed on others; it took a review of
  the fix commit to catch it. Before/after zone and finding diffs now cover eight
  repositories (ripgrep, flask, hono, redis, gevent, better-sqlite3, actions/checkout,
  fsmap).

## D-15 An agent harness inside fsmap: `fsmap verify` and Claude Code hooks

**Context.** Two other workstreams built Rust × AI harnesses next to fsmap: a template
repository (`rust-ai-template`, lost with its session) and a harness repository
(`rust-ai-harness`, not reachable from here; its design survives as a published page). Both had
the verification layer (the compiler and tests in the agent's loop, a gate before "done") but no
knowledge of the repository. fsmap has that knowledge (`brief`, `context`, `insights`) but put
nothing in the agent's loop. The owner chose to fold both into this repository and to
re-implement the harness from its design rather than port code.

**Decision.** The harness is part of the fsmap binary: `fsmap verify` (the gate) and
`fsmap hook session-start|post-edit|stop|prompt|pre-bash|stats`. Templates live in `harness/`
(settings, `AGENTS.md`, a reviewer subagent); this repository uses copies of them, checked
equal by a test. Guide: [agent-harness.md](agent-harness.md).

- **One binary, installed from a trusted ref**, not shell scripts and not a binary built from
  the tree under test. Hooks are JSON in, JSON out, exit 0; every failure to run becomes a
  `systemMessage` the user sees.
- **One gate for people, hooks and CI.** CI keeps its explicit fmt/clippy/test steps as the gate
  of record and adds a job running `fsmap verify --mode full --strict`; a change that weakens
  `verify` cannot pass CI by grading itself.
- **Guardrail, not boundary.** The agent can edit what the hooks read. Ask rules make the
  obvious edits visible; the rest is documented as a limit.
- **Lints for agent-written code**: `allow_attributes(_without_reason)`, `unwrap_used`
  (tests exempt), `map_err_ignore`, `redundant_clone`, `clone_on_ref_ptr`, on top of the
  existing `todo`/`unimplemented`/`dbg_macro` denies. `cargo-deny` policy in `deny.toml`. An
  MSRV job keeps `rust-version = "1.88"` true (it is the exact floor: let-chains, and globset
  and sha2 declare 1.88).

**Design review.** An independent agent reviewed the draft before implementation, reproducing
the important findings on copies of the repository. Each fix has an end-to-end test in
`tests/harness.rs` (or `tests/review_regressions.rs`). Three of these fixes (B1, M3, M7) and
one claim about M8 turned out incomplete; the final review below records how.

| Finding | Fix | Alternative and why not |
|---|---|---|
| B1. `.cargo/config.toml` with `--cap-lints allow` and `runner = "true"` turned failing clippy and tests green; `.rustfmt.toml` `disable_all_formatting` did the same for fmt; `.git/info/exclude` hid the file from git. The same through `RUSTFLAGS`, `RUSTC_WRAPPER`, `CARGO_TARGET_*_RUNNER`, `[env]`. | An `integrity` step reads every cargo, rustfmt, clippy and rustup config in effect (ancestors, `CARGO_HOME`, XDG) and fails on settings that weaken the gate unless `trust` lists the kind. Steps run with the weakening variables removed and the build directory inside the repository. Config files and remaining variables are part of the fingerprint, ignored or not. Integrity failures always block, even in untouched files. | Detect `--config` flags only (the other harness's fix): rejected, files and variables are the general form. |
| B2. A self-hosting bootstrap (build the gate from this tree at session start) failed open after `cargo clean`, gave a session started mid-refactor no gate, and let one session weaken the next session's gate. | Cut. This repository uses `fsmap` from PATH like any other; the SessionStart command says loudly when it is missing. `tests/harness.rs` covers changes to the harness itself. | Build from `git archive <merge-base>` outside the tree: rejected for a 40 s start and races between sessions. |
| B3. SessionStart also fires on resume, compact and clear; re-recording the baseline there let a broken tree become "where the session started". | The baseline is written once, on `startup`, per session; a missing one means "unknown" and the gate runs. Compaction tells Claude the tree has unverified changes. | — |
| M1. A permanent release after three failures. | The next user prompt resets the streak; an unchanged failing tree re-blocks from the cached reason without re-running. | — |
| M2. Any background shell (a dev server) postponed the gate forever. | Only background agents (subagent, workflow, teammate, cloud session) postpone it, and the user is told. | — |
| M3. Failing open silently (timeouts, missing binary, unwritable state, git refusing the repository). | `systemMessage` on each; own deadlines below the hook timeouts; the whole process tree is killed on expiry (`kill -s KILL -- -<pgid>`, `taskkill /T`). | — |
| M4. Git-ignored files cargo still compiles (`tests/scratch.rs` behind `.git/info/exclude`) left the fingerprint unchanged. | Ignored `*.rs`, `Cargo.toml`, `Cargo.lock`, the config files, `rustc -vV` and the fsmap version are fingerprinted, with the contents of every dirty file and HEAD. | Path dependencies outside the repository and non-Rust `include_str!` inputs are not covered (documented). |
| M5. A submodule's own `filter.<x>.clean` ran during superproject `git status`, and in the existing `brief --diff`. | `--ignore-submodules=dirty` everywhere the work tree is compared (`git_worktree` sets it); submodules are fingerprinted separately with their filters neutralised. The `brief --diff` hole predated the harness and was fixed in the original PR with a regression test. | — |
| M6. CI gated by the code under review; dogfood on a shallow clone checks nothing. | Explicit CI steps stay; the `gate` job uses full history. | — |
| M7. Lints from a new stable toolchain or breakage from before the session blocked the agent in files it never touched. | fmt and clippy failures are attributed to files; failures only in untouched files are reported to the user, not blocked on. | Pin the toolchain: rejected for now, every fresh cloud session would download it, and attribution already keeps drift from blocking. Revisit if drift reaches touched files often. |
| M8. Two sessions in one worktree overwrote each other's state. | Per-session records, atomic writes, 14-day expiry, a warning when another session was active in the last hour. | Pid heartbeats: rejected, hooks are short-lived processes. |
| m1–m3. Post-edit used exit 2 (losing the brief), showed the last errors, ran unlocked, queued in parallel. | Exit 0 with `decision: block` and the brief as context; the first 20 errors, de-duplicated; `--offline --locked`; one check per worktree. | — |
| m6. A suppression keeps the gate green. | A pass shows the user added `#[allow]`/`#[expect]`/`#[ignore]`/`#[should_panic]`/`cfg(any())` and removed assertions. | Block on them: rejected, some are legitimate; the user decides. |
| m7. Gate output contains compiler and test output, against the rule that outputs never contain file contents. | **Exception, recorded here:** the gate's failure text (short-format diagnostics, the last 40 lines of a failing step, capped at 8 000 characters, control characters removed) goes only to the agent and user of the same session. Reports, snapshots and `context` keep the rule. | Test names and panic locations only: rejected, the assertion values are what an agent needs to fix a test. |
| m4, m5, m9, m10, m12 | Documented limits (running repository code, `settings.local.json`, LFS stat noise); `CARGO_TARGET_DIR` forced inside the repository; registry sources from `cargo metadata` `manifest_path`; an XDG state fallback. | `harness install --trust`: cut with the install command. |

**Not adopted from the review**, with reasons:

- *A `--deny` flag instead of quick/full modes*: equivalent today; the mode leaves room for
  slower checks that belong only in CI.
- *Test only changed packages and their reverse dependencies*: the target is single-crate and
  small-workspace repositories; seeded in the roadmap.
- *A per-turn bypass keyword only the user can type*: the user can already tell the agent to stop;
  seeded.
- *`redundant_clone` is a nursery lint* and can misfire under `-D warnings`: kept because it
  catches the copying agents use to get past the borrow checker, and it is clean on this code. It
  is the first lint to drop if it misfires.

**Cut on purpose**: `harness install` (copying three files is documented instead, as with
`agent-ignore`), the bootstrap launcher, and a Stop-time "forgotten co-change partner"
continuation (the post-edit brief already says it, and each forced turn costs a round trip).

**Found while implementing**, each now covered by a test:

- procps `kill -KILL -<pgid>` kills nothing; `kill -s KILL -- -<pgid>` does.
- A git-ignored weakening config was classified as a pre-existing failure, so the gate passed
  with `runner = "true"`. Integrity now always blocks.
- `dependency_sources` came back empty: the bounded output buffer kept the last 256 KiB of a
  400 KB `cargo metadata` document.
- Two end-to-end tests passed for the wrong reason: the hooks' own cache directory and a
  generated `Cargo.lock` changed the fingerprint. The fixture now ignores the former and commits
  the latter.
- Windows drive letters broke diagnostic parsing, and canonical path forms could turn a new
  failure into a "pre-existing" one; paths are compared as relative strings.

**Final review (of the implementation).** A second independent agent reviewed the finished
harness on scratch clones, without the author's context. It reproduced 2 blockers and 6 major
findings, and showed that this entry and the guide claimed more than the code did: "each guard's
test fails without it" was false for three guards, "never fails open silently" was false, and
the M8 fix lost records under concurrency. Each finding below has a test that fails on the
previous code.

| Finding | Fix | Alternative and why not |
|---|---|---|
| B1'. The integrity step read TOML line by line against a denylist. Bypasses, each turning a failing step green while integrity said `ok`: an `[alias]` for `clippy`/`fmt`, a fake `linker`, `#` inside a rustflags string, a TOML escape, a nested `src/rustfmt.toml`, a dotted `toolchain.path`, `CARGO_ALIAS_*` and `*_LINKER` variables. A toolchain `path` was also detected only after cargo had already run the fake cargo. | Parse with the `toml` crate (the standard parser; a hand-written one was the defect) and judge against an allowlist: named kinds for known shortcuts, `config:<table>` for anything unknown. Nested rustfmt and clippy files, ignored ones included. Toolchain files checked before the first cargo call; non-official channels refused (a `rustup toolchain link` target is any binary). Whole variable families removed (`CARGO_BUILD_*`, `CARGO_TARGET_*`, `CARGO_ALIAS_*`, `CARGO_PROFILE_*`, `RUSTUP_TOOLCHAIN`, `RUSTC_BOOTSTRAP` …). | Add each bypass to the denylist: rejected, the review found seven in one pass; an allowlist fails closed. Run steps from a neutral directory so repository configs are not read: rejected, it also drops legitimate settings (source replacement, toolchain pins) and diverges from CI. |
| B2'. When cargo could not locate the workspace (a broken manifest, a relative toolchain path) or the verify table was malformed, stop, pre-bash and post-edit printed nothing: a failing tree could be pushed. | A project with a Cargo.toml that cannot be read blocks the stop (three strikes) and denies a push, with the reason. The verify table is read directly with `toml`; unknown keys and bad entries are errors in the hooks and the CLI alike. | — |
| M1'. "Failures only in untouched files do not block" let new failures through: a change that leaves a helper without a caller makes dead code in the untouched file; `-D warnings` hides lints in dependent targets; after `/clear` committed changes counted as untouched. | The gate passes only when every step passes. When every failure is in files the session did not change (never integrity failures), the stop is blocked once and then released with a report, the user is told the gate did not pass, and a push stays denied. `start_head` is recorded the first time a session is seen. | Baseline diagnostics from a worktree at the session's start: sound, but a second build per session; seeded. Keep the old rule: rejected, it reported passes that were not. |
| M2'. A session that started on a broken commit could push it: "unchanged since the session started" also skipped the pre-push gate. | For a push only a recorded pass counts. | — |
| M3'. An fsmap from before the harness on `PATH` answers `fsmap hook …` with a usage error and exit 2, which Claude Code reads as "block" for prompts and tools. | The template runs `fsmap hook … 2>/dev/null \|\| true`; session start says the harness is OFF when the hook command is missing. Panics in a hook become a message. | A version check in every hook: more moving parts for the same result. |
| M4'. The deadline waited for output pipes that a background grandchild kept open (25 s past a 8 s budget). | Leftover group members are killed when the tool ends; the readers get a 2 s grace and are then abandoned. | — |
| M5'. A long gate run saved a snapshot loaded before it, erasing a record another session wrote meanwhile. | Outcomes are recorded through a locked read-modify-write of the freshest state. | Hold a lock for the whole gate: rejected, a session start would wait minutes. |
| M6'. Push detection denied `git stash push` and `git log --grep push` and missed `sh -c 'git push'` and `gh -R o/r pr create`. | A quote-aware reader of commands, wrappers, `sh -c`/`eval` scripts and git's and gh's global options (`harness::shell`). Git aliases and scripts that push remain unseen (documented). | — |
| Minor | Post-edit waited for nothing and said a running check "covers this edit"; it now waits up to 30 s. The suppression report runs `git diff` with `--no-textconv` and fixed prefixes, and covers `cfg_attr`, `rustfmt::skip`, manifest lint levels, config changes and new untracked files. Duplicate config paths, macOS and Windows rustfmt user directories, `stats` without cargo, a shared test module's `#![allow]` split into `allow(dead_code)` and `expect(unwrap_used)`. | Detect git aliases by reading `alias.*`: deferred; the stop gate still runs. |

**Mutation check, after the fixes.** 21 guards were removed one at a time and the matching test
run; each test failed. Two first looked wrong: one because the checking script mistook the
fixture's own "could not compile" output for the mutant's, and one, a real gap, because the test
used an `[alias]` that also broke the clippy step, so "integrity failures are never someone
else's" was never isolated. That test now uses an `[env]` table that breaks nothing else.

**Second implementation review.** A third fresh agent reviewed the fixes. It found 1 blocker and
10 major findings, and showed that six of the fixes above had no test that failed without them.
Two rounds in a row had found new ways around the integrity step, which made the threat model
the real question. It is now stated in the guide: an agent that takes the shortest path to green.
**Hidden configuration** (untracked or ignored files, configs outside the repository,
environment variables) is what makes the local gate disagree with CI, and fails. **Tracked
changes** to manifests and configs are in the diff and run the same in CI, and are reported to
the user on every pass. An agent determined to defeat the gate is out of scope; CI on a clean
checkout is the gate of record.

| Finding | Fix | Alternative and why not |
|---|---|---|
| B1''. `--force-warn` was on the safe list; a force-warned lint stays a warning under `-D warnings`. | Its own finding (`allow-flag`). | — |
| M1''. `-C opt-level` above 0 switches debug assertions and overflow checks off. | `opt-level=0` only; `debug-assertions`/`overflow-checks` only when explicitly on. | — |
| M2''. `[source]` replacement could swap in a modified dependency. | A `source` kind that vendoring repositories trust. | Hash vendored sources against `Cargo.lock`: cargo already does for registries; directory sources carry their own checksums. |
| M3''. A `clippy.toml` above the repository was never judged. | Counts as untracked. | — |
| M4''. `Cargo.toml` settings (`debug-assertions = false`, `test = false`, `[patch]`, lints allowed) passed silently. | Reported on pass (tracked: see the threat model). | Fail on them: a person decides whether a manifest change is legitimate; CI runs it too. |
| M5''. rustup accepts official-looking link names (`stable-evil`). | A channel whose rustup toolchain directory is a link is refused. | Validate the triple against `rustc --print target-list`: runs the toolchain being judged. |
| M6''. A session that started before the first commit treated its own commits as someone else's. | Changes are judged against the empty tree; a session that starts before the Cargo project exists also records where it started. | — |
| M7''. The pre-push gate checked the working tree while the push sent HEAD. | Push denied while Rust sources, manifests or `Cargo.lock` are uncommitted. | Verify a temporary worktree of HEAD: a second build per push; seeded (a clean-worktree gate). |
| M8''. Push detection missed reserved words, wrapper options, `sh -c --`, stdin shells, `git -c alias.p=push`, `git subtree push`; denied heredoc bodies and `--dry-run`. | Handled; a shell reading its script from stdin is gated when the command line mentions a push. | — |
| M9''. Six fixes had no test that failed without them. | Tests added; one of them first passed vacuously (the cut output gave an empty pid) and was fixed. | — |
| M10''. A git-ignored `vendor/` with its crates' `clippy.toml` files failed integrity. | Only files that can apply to a workspace member are judged. | — |
| Minor | Common settings pass by value: mold (`-fuse-ld`), clang/gcc linkers, sccache, `--cfg`, ordinary `[env]` variables. Without a fingerprint the gate runs every time. `RUSTFMT` is removed. `cfg_attr(…, ignore)`, `cfg(false)` and single-quoted lint levels are reported. Template hooks report crashes other than "missing" and "too old". | Value-level trust (`rustflag:--cfg x`): more surface than the known-good list needs today. |

**Mutation check, after the second round.** 45 guards removed one at a time; each matching test
failed. Live evidence from the same session: the pre-push hook of the previous build ran the full
gate three times on commands that only mentioned a push in a commit message or a heredoc (37, 42
and 56 s), the false positive this round removed; the third run also caught a formatting slip in
a commit that had just been made.

**Third implementation review.** A fourth fresh agent, scoped to whether the code now matches
the stated threat model, found that it did not in both directions. Hidden configuration still
had ways through, and tracked, legitimate configuration failed. The fix aligns the code with the
model rather than the other way round. Where a file is decides how it is judged: tracked files are
not judged (changes are reported on pass), hidden files inside the repository are judged and never
trusted, files outside it are judged and trustable. Environment variables are judged by value.

| Finding | Fix | Alternative and why not |
|---|---|---|
| 1. A hidden `[env]` could set `LD_PRELOAD`, `COMPILER_PATH` or a snapshot switch the name list missed (`UPDATE_GOLDENFILES`, `EXPECTORATE`). | In a hidden file every `[env]` entry fails; the environment list gained loader, compiler-driver and snapshot variables. | Extend the name list only: a list is never complete, and a hidden file has no legitimate reason to differ from CI. |
| 2. A `rustup override set` directory override was not checked. | Overrides for the root or its ancestors in rustup's `settings.toml` are judged like a toolchain channel. | Ask `rustup show active-toolchain`: a process per scan, and its text output changes between rustup versions. |
| 3. Push denied for files no build reads (`docs/book.toml`, `scratch/try.rs`), for `gh pr merge`, for `git push --delete`. | Build inputs are manifests, the lock file, cargo/rustfmt/clippy/toolchain files and `.rs` files in a directory a target reads (from `cargo metadata`); deleting refs, `--help` and `gh pr merge` are not publishing. | Keep the broad rule: each false denial teaches the agent to work around the hook. |
| 4. Tracked, committed configuration failed integrity: a `wasm32` runner, an embedded link script, `-fuse-ld=/usr/bin/mold`, a `rustdocflags` value counted as a flag, a vendored `[source]`, a tracked rustfmt `ignore`, a corporate mirror in `CARGO_HOME`. | Tracked files are not judged; target tables apply only to the host, the build target and `cfg(…)`; `-fuse-ld=` accepts absolute paths outside the repository; a flag's value travels with it; only local-directory sources count. | Judge tracked files too and document it: contradicts "CI runs it too", and fails legitimate repositories on first use. |
| 5. Trust applied per kind to every file, so trusting `runner` for one machine-wide config also let a hidden one in the repository through. | Trust applies only outside the repository; findings say whether they are trustable, and the message says which remedy applies. A trusted kind still applies to every file outside the repository: an agent writing one above it to get past the gate is the out-of-scope case. | Trust per file path: paths differ per machine. |
| 6. Hidden rustfmt configuration with ordinary options (`max_width` in an ignored `src/rustfmt.toml`) passed; `~/.rustfmt.toml` was not read when the home directory is not an ancestor. | Any hidden rustfmt file fails; a rustfmt file outside the repository fails (`rustfmt-outside`, trustable) when the repository has none of its own. | Judge rustfmt options one by one: almost every option changes what `--check` reports. |
| 7. A file already changed before the session was labelled "changed in this session". | The session hashes pending files at first sight; unchanged since, they are not its own, for failure labels and for the pass report. | Diff against a stash of the start state: writes to the repository. |
| 8. Harmless variables were removed wholesale (`CARGO_BUILD_JOBS`, `CARGO_PROFILE_*_DEBUG`, `RUSTFLAGS="--cfg tokio_unstable"`). | Judged by value with the same rules as config files. | — |
| 9. Minor: a crash of the pre-push hook let the push through; `"$(git push)"`, backticks in double quotes and a line after `echo "<<X"` were missed; `git push --help` was gated; a new repository's untracked root `clippy.toml` failed with no hint. | A failed or crashed pre-push check denies a publishing command; substitutions in double quotes are followed and heredoc operators are found only outside quotes; the message names the remedy (remove it, or ask the user to commit it). | Let the agent commit hidden files itself: turns a block into a report the user may not read. |

**Mutation check, after the third round.** 75 guards removed one at a time (the 45 before, their
anchors updated, and 30 new ones); each matching test failed. Two needed a second look: one anchor
no longer matched the code, and one test passed with "tracked files are not judged" removed, because
its tracked config held only a variable the rules accept anyway; it now also holds one they judge.
A live check in this container: a `rustup override set` to a linked toolchain made `fsmap verify`
refuse before any cargo call (exit 2); rustup's settings were restored byte for byte afterwards.

**Fourth implementation review (convergence check).** A fifth fresh agent checked whether the
third round's fixes closed their findings: six closed, three partly. It also found that the
round had opened new paths, each reproduced against both commits. The worst came from the new
quote-aware heredoc scan. Claude Code's own commit form, `git commit -m "$(cat <<'EOF' …
EOF)" && git push`, got past the push gate whenever the message held an apostrophe.

| Finding | Fix | Alternative and why not |
|---|---|---|
| B1. A heredoc inside `"$(…)"` was taken for quoted text; an apostrophe in its body opened a quote that swallowed the `&& git push` after it. | The heredoc scan keeps a stack of quoting contexts (`'…'`, `$'…'`, `"…"`, `$(…)`, backticks, `$((…))`). A command line that does not parse to its end, or nests past 16 levels, counts as publishing when it mentions a push, and a heredoc that never ends is not removed as data. | A real shell parser crate: a dependency for one guardrail, and its errors would need the same fallback. |
| M1. A tracked `.cargo/config.toml` could `include` an ignored file (stable in cargo 1.97, checked here), which was neither judged nor fingerprinted. | Includes are followed transitively; each file is placed and judged on its own and hashed into the fingerprint. | Fail every `include`: legitimate for machine-local settings. |
| M2. Narrowing the push check to target directories let a renamed, deleted or tracked-but-outside source through. | Every change to a tracked `.rs` file counts, a rename's original path included; only untracked files are filtered by target directory. | — |
| M3. Hiding changes that were pending at session start dropped them from the pass report, so a weakening made in an earlier session was never shown. | Listed apart ("already changed when this session started") instead of dropped. | — |
| M4. A tracked config marked `--skip-worktree` was neither judged nor reported. | `git ls-files -v` tags: skip-worktree and assume-unchanged files count as hidden. They are also hashed into the fingerprint and counted as uncommitted for a push. | — |
| M5. The target filter read `build.target` only as a string in the same file. | Build targets come from every configuration file (string or list) and `CARGO_BUILD_TARGET`; a hidden file has every `[target.*]` table judged. | — |
| M6. A `#` comment with an apostrophe hid the next line's push (from before this round). | Comments are skipped; the open-quote fallback covers what remains. | — |
| M7. A hidden `--cfg miri` with `#[cfg(not(miri))]` on a failing test passed (from before this round). | In a hidden file any rustflag fails. | Judge `--cfg` names: any name can gate a test. |
| Minor | A symlink inside the repository is placed where the link is. A hidden toolchain file fails whatever channel it pins. Nesting is bounded (20 000 levels overflowed the stack, and the wrapper let the crashed hook's command through). In single quotes inside `$(…)` a backslash is an ordinary character. `git push -o --delete` is a push. | — |

**Mutation check, after the fourth round.** 96 guards removed one at a time; each matching test
failed. The first pass showed why the check matters: the open-quote fallback can mask a guard
(a mis-parse still reports a push when the text mentions one), so four parser guards needed cases
where the fallback cannot save them (a delimiter line after the swallowed push, a non-push command
that mentions one). The skip-worktree push test was denied for the wrong reason (its edit also
failed the tests) and now uses a passing edit. While the run was going, a turn ended and the live
stop hook ran the gate on a tree with a mutant in place; it blocked. It also caught a real
regression in the same run: a rename's original path had leaked into `dirty_paths`, which an
existing test pinned. That was fixed in the code; the test is unchanged.

**Fifth implementation review (convergence check).** A sixth fresh agent confirmed every
previous finding closed. It found no blocker, and 3 major and 4 minor findings, five of them
regressions from the round before. Its explanation of one (that `git status` prints paths
relative to a subdirectory) did not hold when checked: the paths are relative to the work tree
top. The bug it found was real and older: dirty files were read from the workspace instead of
the top, so in a nested workspace a re-edited file never re-ran the gate.

| Finding | Fix | Alternative and why not |
|---|---|---|
| 1. A tracked symlink to an ignored config passed: git tracks only the link text, and placing links where they are (last round) made it Tracked. Edits to the target did not change the fingerprint. | A link is placed by the stricter of where it is and where it points; the fingerprint reads config files through links. | Refuse links: dotfile setups use them. |
| 2. A tracked file reached through `include` under another name was judged as hidden. Changes to it were not reported on pass. | Files not among the listed config names are asked about one by one; the pass report covers every config file, classified by name. | — |
| 3. Every unchanged `--skip-worktree` file counted as uncommitted, so a push in a sparse checkout was denied permanently. Nested workspaces never had their dirty files read. | Only entries present on disk whose content differs from the index blob (`git hash-object --no-filters`) count. Dirty files are read from the work tree top. | — |
| Minor | A heredoc whose output a shell or `eval` runs, or that is piped into a shell, is a script. `#` starts a comment only at a word boundary. `$((` in double quotes and `((` as a command are arithmetic. Delimiters are read as words with quotes and backslashes removed (`'END-MSG'`, `\EOF`), so a commit message heredoc with a dash in its delimiter is no longer denied. | — |

**Mutation check, after the fifth round.** 111 guards removed one at a time; each matching test
failed. Two guards were replaced this round (`shift-not-heredoc` by `shift-number`,
`build-config-report` by `classify-by-name`). Two older parser guards passed on the first try:
the new delimiter reading made their old cases safe without them. Each got a case only that guard
decides. A test was added for reading configs through links, which had none. During this round a
Python edit with an empty match range garbled `state.rs`; it was restored from the last commit
and redone with checked ranges. Nothing of it was committed.

**Sixth implementation review (last convergence check, blockers and majors only).** A seventh
fresh agent confirmed the fifth round's fixes. It found no blocker and 3 major findings, two of
them regressions from that round:

| Finding | Fix | Alternative and why not |
|---|---|---|
| 1. An uncommitted edit to a tracked file a config `include`s, under another name, was neither judged (tracked) nor counted as an uncommitted input, so a push sent a commit the gate never ran. | Every config file `integrity` finds is an uncommitted input for a push, whatever its name. | — |
| 2. Files were placed by asking the repository nearest to them, so a file in an ignored nested repository counted as tracked. | Files are placed by asking the project's work tree top about them (`:(literal)` paths). | — |
| 3. A heredoc fed to a shell behind a wrapper (`timeout 60 bash <<EOF`, `sudo bash`, `VAR=1 bash`, `… \| sudo -u x bash`), or read through `eval $(…)` or `bash <(…)`, was stripped as data (mostly from before). | One helper finds a command's program past reserved words, assignments and wrappers; push detection and the heredoc decision both use it. A pipe is followed to the end of its command only, and process substitutions count as substitutions. | — |

**Mutation check, after the sixth round.** 118 guards removed one at a time; each matching test
failed, after one new case (a `||` must not count as a pipe) gave the "pipe ends at its
command" guard a test only it decides. A corpus of 25 commands an agent commonly runs (commit
and PR heredocs with apostrophes, loops, pipelines, retries, tags) gave no false denial and no
missed push.

**Where the review loop stopped.** Six implementation reviews in a row found new ways around the
shell detector, the later ones mostly regressions from the round before. The integrity step
converged: the last two rounds found edge cases in file placement, not bypasses of the model.
The command-line detector did not, and cannot: it is a heuristic over a language it does not
fully parse. It now errs toward gating, by falling back to "mentions a push" when unsure. The
structural answer is a second detector that does not read shell at all: a git `pre-push` hook
(ROADMAP). The loop stopped here on purpose; the next round would most likely find more shell
forms, not a flaw in the model.

**Windows CI, first end-to-end run.** The hook tests had never run on Windows; until then the unit
tests failed first. Four failed for one cause. On Windows `std::fs::canonicalize` returns
verbatim paths (`\\?\C:\…`), which never compare equal to the plain paths cargo and git print,
and rustfmt prints verbatim paths itself. So post-edit found no package for the edited file and
checked nothing. A failure from rustfmt in a file from before the session could not be
attributed, so the gate blocked the turn as if the session had caused it. The same mismatch hid
a rustup directory override on Windows. Its test used the verbatim form as the key, so it passed
for the wrong reason.

The fix has three parts. Every canonical path now comes from `dunce::canonicalize`, which returns
the plain form when that names the same file. `clippy.toml` forbids std's. Printed paths are
simplified before they are compared, and an override key matches in either spelling.

The alternative was to strip the prefix by hand: no dependency, about 20 lines. It was not taken.
The cases where stripping changes which file a path names (reserved device names, paths over 260
characters) are the ones dunce already handles, and dunce has no dependencies of its own.

**Costs.** Every turn that changed the tree pays one gate run: on this repository 47–48 s with nothing
to rebuild (4-core container, 142 tests, three runs). Nearly all of it is `cargo test`, whose 43 end-to-end
hook tests each build a small Cargo project. Every Rust edit
pays a `cargo check` (median 1.1 s in the session that wired the hooks). The hooks run build scripts, proc macros and tests without asking, so they
are for repositories you trust. The ask rules add prompts when the agent edits manifests, lint
configs, workflows or `.claude/`.

**Evidence still missing.** Whether the gate reduces broken "done" claims, and how often it
blocks falsely, needs logs from real sessions. `fsmap hook stats` reads them.
