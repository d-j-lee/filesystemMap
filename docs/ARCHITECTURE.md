# fsmap architecture

> How fsmap is put together and why. Usage: [README](../README.md). Decisions with
> alternatives: [decisions.md](decisions.md). JSON contract: [snapshot-schema.md](snapshot-schema.md).
> Unbuilt ideas: [ROADMAP.md](ROADMAP.md).

## 1. What fsmap is

One binary that turns a directory into a **map**. The directory can be a source repository, a
home folder, or a build server's disk. The map covers sizes, file kinds, code volume, git state
and history, import structure, and a ranked list of things worth acting on.

| Output | For | Command |
|---|---|---|
| Self-contained interactive HTML | humans exploring, sharing | `fsmap report` |
| Token-budgeted Markdown | AI agents (CLAUDE.md, prompts) | `fsmap context` |
| Pre-/post-edit briefing, Claude Code hook | agents and reviewers | `fsmap brief` |
| Versioned JSON snapshot | scripts, diffing | `fsmap scan`, `fsmap diff` |
| Findings: text, JSON, SARIF, Markdown | CI gates with a baseline ratchet | `fsmap insights` |
| Read-deny suggestions | agents | `fsmap agent-ignore` |

## 2. Pipeline

```
scan ─▶ classify ─▶ git status ─▶ content ─▶ git history ─▶ imports ─▶ hotspots ─▶ roll-up ─▶ duplicates
                                                                                              │
              findings ◀───────────────────────────────────────────────────────────────────────┘
                 │
     prune (HTML only) ─▶ render: html · json · markdown · sarif · terminal
```

Each stage enriches one arena (`model::Tree`). The order matters:

- Origin zones come from names and markers. Git status then marks what is ignored.
- Only after both does the content stage decide which files to read (decision D-08).
- History needs the content stage's test and generated flags.
- Imports need history, to mark coupling pairs as linked or not.

## 3. Model

- **Arena tree** (`model::Tree`): `Vec<Node>` in DFS pre-order, children sorted by name, so
  output is deterministic.
  - A node's subtree is the contiguous range `[id, id + span)`, so subtree tests are O(1).
  - Directory roll-ups are one reverse pass.
  - The JSON is flat, with no recursion limits, and the viewer rebuilds it in one loop.
- **Zone = origin**: `source`, `vendored`, `dependency`, `artifact`, `vcs`. It is decided by:
  1. names, with ambiguous names (`target`, `build`, `dist`) only next to a matching manifest;
  2. markers (`CACHEDIR.TAG`, `pyvenv.cfg`, a Cargo home's `registry/`);
  3. `.gitattributes` `linguist-vendored` / `linguist-generated`.

  The directory where the zone changes keeps its reason (`zoneRoots`).
- **Git state** is separate: `tracked` / `untracked` / `ignored`, per owning work tree.
- **Analysed** = origin `source`/`vendored` and not git-ignored and not a cloud placeholder.
  Only analysed files are read. A 2 GB `node_modules` costs one `lstat` per entry.

## 4. Stages

| Stage | Module | Key points |
|---|---|---|
| Scan | `scan` | <ul><li>rayon fork-join over `read_dir`, `lstat` only, symlinks never followed</li><li>hard links: each inode's disk usage is counted once, at its first path in sorted order</li><li>stays on one filesystem; Linux mount points from `/proc/self/mountinfo` are never stat'ed (a dead NFS mount can hang `lstat`)</li><li>bind-mount loop guard with `--cross-fs`</li><li>cloud placeholders flagged, never read</li></ul> |
| Classify | `classify` | <ul><li>71-language table (comment syntax, import extractor, colour)</li><li>categories; test and manifest heuristics</li></ul> |
| Git status | `git` | <ul><li>discovers every work tree in the scan (`.git` dir or file) plus the one enclosing the root</li><li>per tree: `ls-files` (tracked, then ignored with `--directory`), `check-attr` for linguist attributes, hygiene (`status --porcelain=v2`, stashes, unpushed branches, worktrees)</li><li>each node belongs to its deepest work tree</li></ul> |
| Content | `content`, `strip` | <ul><li>one parallel read per eligible file: NUL sniff, shebang, generated markers in the first 5 lines, minified detection</li><li>a single-pass comment stripper gives code/comment/blank counts and the stripped text for import extraction</li></ul> |
| History | `git::history` | <ul><li>`git log --no-merges -M --raw -z` (tree diffs only; partial clones never fetch blobs)</li><li>walked newest→oldest with a rename alias map; additions tombstone their path</li><li>skips shallow boundaries, `.git-blame-ignore-revs` and `[bot]` authors</li><li>large commits count for neither revisions nor coupling</li></ul> |
| Imports | `deps` | <ul><li>regex extraction over stripped text, then resolution against the scanned files: Rust crate roots from `Cargo.toml` targets, `mod`/`use`, workspace crates; Python package chains and source roots; JS/TS relative paths, tsconfig `paths`, workspace packages; Go modules; JVM packages; C/C++ quoted and angle includes; Protobuf/Mojom</li><li>deferred imports (indented Python, JS `import()`) are *weak*</li><li>child→parent-module references are *up*</li><li>neither kind counts toward cycles</li></ul> |
| Hotspots | `analysis` | percentile × percentile (§5) |
| Duplicates | `dupes` | size bucket → 16 KiB prefix xxh3 → full xxh3-128; hard links and placeholders excluded |
| Findings | `insights` | 19 rules (`insights::RULES`), stable ids, code-quality rules scoped to repos or manifest-rooted projects |
| Prune | `prune` | multi-metric top-K plus everything a finding/table references; each directory's remainder folds into one `rest` node so totals stay exact |
| Render | `report::*`, `snapshot` | HTML = template + JS/CSS compiled into the binary + escaped JSON; hash-based CSP |

## 5. Scoring definitions

- **Revisions**: non-merge commits in the window that are not from bots, not in
  `.git-blame-ignore-revs`, and touch ≤ 50 files.
- **Window**: by default 2 years back from each repository's **newest commit** (`--since`),
  with a safety cap of 30 000 commits per repository.
- **Recent change** = Σ 0.5^(age / 90 days) over revisions; age is measured from the repo's
  newest commit.
- **Hotspot score** = percentile(recent change) × percentile(indentation complexity) among
  analysed, non-generated code files with history. A file is **flagged** when both
  percentiles ≥ 0.8, commits ≥ 5, recent change ≥ 1, and it is not a test.
- **Change coupling** is computed over commits touching 2–30 files.
  - Confidence: conf(A→B) = n_AB / n_A. Lift = n_AB · N / (n_A · n_B).
  - A pair is kept when n_AB ≥ 5, lift ≥ 2, max confidence ≥ 0.3, and neither file is
    generated.
  - It is *hidden coupling* when both languages have extractors, no import path of ≤ 2 hops
    links the files, neither file is a test, and they sit in different directories.
- **Authorship**: Degree-of-Authorship (Fritz et al.), DOA = 3.293 + 1.098·FA + 0.164·DL −
  0.321·ln(1+AC).
  - A developer is an author when the normalised DOA > 0.75 and the absolute DOA ≥ 3.293.
  - **Truck factor** uses Avelino et al.'s greedy removal until more than half of the files
    are orphaned.
  - **Knowledge risk**: no author active within 180 days of the newest commit, **and** the
    file is a flagged hotspot or in the top 5 % by fan-in.
- **Cycles**: SCCs of the *lifted sibling graph*.
  - An import a→b becomes an edge between the two children of lca(a, b) that contain them.
  - So every cycle is reported at the level where it exists.
  - The suggested fix is a weighted feedback arc set (Eades–Lin–Smyth): the few, light
    edges whose removal makes the members acyclic. On hono it singles out
    `utils → helper` (1 import) against `helper → utils` (13).
  - Cycles inside one Rust crate are low severity, since they compile fine.

## 6. Measured performance

Measured 2026-10-28 on a 4-core cloud container, release build.

| Workload | Result |
|---|---|
| ripgrep: 2,241 commits, full history, imports, findings | 0.3 s |
| flask: 3.8 k commits | 0.28 s |
| `/usr`, 93 k entries, `--no-git` | scan 0.14 s; content 1.6 s warm / 10.8 s cold (456 MiB text); peak RSS 88 MB |
| `/usr --sizes-only` → HTML | 0.57 s, 6.6 MiB report |
| `brief --hook` with a warm cache | 14 ms |

## 7. Safety and privacy

- No `unsafe` (`[lints.rust] unsafe_code = "forbid"`).
- fsmap writes only the output you name, plus the `brief` cache under `$XDG_CACHE_HOME/fsmap`.
- Git runs with a fixed prefix so an untrusted repository's config cannot break parsing or
  execute programs (fsmonitor, textconv, external diff). `GIT_OPTIONAL_LOCKS=0` avoids
  `index.lock` races with concurrent agents. `GIT_NO_LAZY_FETCH=1` prevents network fetches
  in partial clones.
- Outputs contain paths, sizes, metrics and author names; never file contents or commit
  messages. Emails are never output. `--authors hashed|none` exists for sharing.
- HTML: data is embedded as inert JSON with `<` escaped; the viewer renders text through
  `textContent` only; a CSP allows only the bundled script's hash. Terminal output strips
  control characters from names.
