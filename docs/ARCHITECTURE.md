# fsmap architecture

> Scope: how fsmap is put together and why. Usage lives in the README; decisions with
> alternatives live in [decisions.md](decisions.md); unbuilt ideas live in [ROADMAP.md](ROADMAP.md).

## 1. What fsmap is

One binary that turns a directory — a source repository, a home folder, a build server
disk — into a **map**: sizes, file kinds, code volume, git history, import structure and
a ranked list of things worth acting on. The map is rendered three ways:

| Output | For | Command |
|---|---|---|
| Self-contained interactive HTML | humans exploring, sharing | `fsmap report` |
| Token-budgeted Markdown | AI agents (CLAUDE.md, prompts) | `fsmap context` |
| Versioned JSON snapshot | scripts, CI, diffing | `fsmap scan --json` |
| Terminal tree / findings | quick look, CI gates | `fsmap tree`, `fsmap insights` |

## 2. Pipeline

```
          ┌──────────┐   ┌───────────┐   ┌──────────┐   ┌──────────┐   ┌───────────┐
 path ──▶ │  scan    │──▶│ classify  │──▶│ content  │──▶│   git    │──▶│   deps    │
          │ parallel │   │ lang/cat  │   │ lines,cx │   │ status,  │   │ imports → │
          │ walk     │   │ zone      │   │ binary   │   │ history  │   │ graph,SCC │
          └──────────┘   └───────────┘   └──────────┘   └──────────┘   └───────────┘
                                                                              │
          ┌──────────┐   ┌───────────┐   ┌──────────┐                        │
 outputs ◀│ render   │◀──│  prune    │◀──│ insights │◀── dupes ◀─────────────┘
          │html/md/  │   │ node      │   │ rules →  │
          │json/tty  │   │ budget    │   │ findings │
          └──────────┘   └───────────┘   └──────────┘
```

Every stage reads and enriches one arena (`Tree`), so stages are independent and
optional (`--no-git`, `--no-deps`, `--no-dupes`).

## 3. Core model

- **Arena tree** (`model::Tree`): `Vec<Node>` in DFS pre-order; a node's subtree is the
  contiguous range `[id, id + subtree_len)`. Parent links are indices. This makes
  roll-ups a single reverse pass, makes subtree tests O(1), and serialises flat
  (no recursion limits, streams well, trivially rebuilt in JS).
- **Node** carries filesystem facts (bytes, disk usage, mtime, kind), classification
  (language, category, zone), content metrics (lines, code lines, indentation
  complexity), git facts (status, commits, churn, last change, owner) and graph
  degree. Directory values are recursive roll-ups.
- **Zone** is the key idea for mixing "filesystem" and "code" views in one map:
  `source` (yours, analysed deeply), `dependency` (vendored / package-manager output),
  `artifact` (build output, caches), `vcs` (`.git`), `ignored` (git-ignored, other).
  Expensive analyses (lines, imports, hotspots, duplicates) run on the `source` zone
  only, so a 2 GB `node_modules` costs a stat per file, not a read.

## 4. Stages

| Stage | Module | Notes |
|---|---|---|
| Scan | `scan` | rayon fork-join over `read_dir`; `lstat` only; no symlink following; hard links counted once (unix `dev`,`ino`); stays on one filesystem by default; errors counted, never fatal. |
| Classify | `classify` | extension/filename tables → language + category; directory-name rules → zone. |
| Content | `content` | parallel reads of source-zone text files ≤ 4 MiB; NUL-byte binary sniff; lines / blank / indentation complexity (Tornhill's language-neutral proxy). |
| Git | `git` | shells out to `git` (see D-03). `ls-files` for tracked / ignored status; `log --numstat -z` for commits, churn, renames, authors, co-change. |
| Deps | `deps` | per-language import extraction on comment-stripped text, then resolution to files inside the scan (D-04). Graph → SCCs (cycles), fan-in / fan-out, directory-level roll-up. |
| Dupes | `dupes` | size bucket → 16 KiB prefix xxh3 → full xxh3-128. Only buckets with ≥2 members are read. |
| Insights | `insights` | independent rules; each emits `Finding { severity, title, why, action, paths }`. |
| Prune | `prune` | multi-metric top-K with per-directory aggregation so the HTML stays fast on 1M-file scans while every metric still sums correctly. |
| Render | `report::*` | HTML embeds the snapshot JSON (escaped) in a template compiled into the binary. |

## 5. Scoring definitions

- **Hotspot** = `norm(log(1+commits)) × norm(log(1+complexity))` over source files,
  normalised by the repo maximum. Log scale keeps one generated file from flattening
  everything else.
- **Change coupling** between files A and B = `shared / avg(commits_A, commits_B)`,
  counted only over commits touching 2..=`coupling_max_files` files; reported when
  `shared ≥ min_shared`. Flagged as *hidden* when the files live in different
  directories and no import edge links them.
- **Owner** = author with the most added lines (+1 per commit so binary edits count).
  *Knowledge risk* = owner share ≥ 75 % and owner inactive ≥ 180 days relative to the
  repository's latest commit (not wall-clock, so archived repos are not all red).

## 6. Performance budget

| Concern | Approach |
|---|---|
| Walk | one `lstat` per entry, parallel per directory |
| Reads | only source-zone text; size cap; parallel |
| Git | one `ls-files` pair + one `log` capped by `--git-max-commits` (default 10 000) |
| HTML | node budget (default 50 000) + canvas rendering; depth-culled drawing |

## 7. Safety and privacy

- No `unsafe` (`#![forbid(unsafe_code)]`).
- fsmap never writes inside the scanned tree except the output file you name.
- The report contains paths, sizes, author names and metrics — never file contents.
  Secret-looking files are flagged by **name only**.
