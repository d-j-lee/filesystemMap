# Snapshot schema `fsmap/1`

The JSON written by `fsmap scan` and embedded in every HTML report. Field names are
camelCase. On nodes, edges and findings, fields that are zero, `false` or empty are omitted
to keep large snapshots small: read a missing number as `0` and a missing flag as `false`.
Other objects carry all their fields, except the optional ones marked `?` below.

Compatibility rule: additive changes keep `fsmap/1`; renaming or removing a field, or
changing a meaning, bumps the schema to `fsmap/2`.

## Top level

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `"fsmap/1"` |
| `tool` | string | `"fsmap <version>"` |
| `root` | string | Absolute path that was scanned |
| `generatedAt` | int | Unix seconds |
| `options` | object | `git`, `window` (prose, e.g. `"730 days before the newest commit"`), `maxCommits`, `deps`, `duplicates`, `crossFs`, `authors` (`names`/`hashed`/`none`) |
| `totals` | object | Whole-tree sums, exact even when `nodes` is pruned. See *Totals* |
| `flagNames` | string[12] | Name of bit *i* of every node's `flags` |
| `languages` | `{name, color}[]` | Indexed by a node's `lang` |
| `nodes` | Node[] | The tree, DFS pre-order; index = node id; `0` is the root |
| `zoneRoots` | `{node, zone, reason}[]` | Where the origin changes, and why |
| `authors` | string[] | Indexed by `owner` and author lists; absent with `--authors none` or when no history was read |
| `repos` | Repo[] | Every git work tree found |
| `reposNotAnalysed` | int? | Work trees beyond `--max-repos` |
| `hotspots` | Hotspot[] | Top 200 by score |
| `coupling` | Pair[] | Change-coupling pairs (top 300) |
| `activity` | `[node, int[24]][]` | Monthly commit counts for directories ≤ 3 levels below each repo root; index 0 = month ending at the repo's newest commit |
| `truck` | `{node, factor, authors, files}[]` | Truck factor per repo and per top-level directory |
| `deps` | Graph? | Import graph (absent with `--no-deps`) |
| `duplicates` | `{bytes, nodes, wasted}[]` | Identical-content groups, largest waste first (top 200) |
| `findings` | Finding[] | Ranked findings |
| `scanIssues` | `{count, samples: [path, error][]}` | Unreadable entries |
| `timingsMs` | object | Milliseconds per stage |
| `pruned` | `{originalNodes, keptNodes}`? | Present when the tree was folded to a node budget |

## Totals

Sums over regular files (`dirs` counts directories).

| Field | Meaning |
|---|---|
| `files`, `dirs` | Counts |
| `bytes`, `disk` | Apparent and allocated size |
| `lines`, `code`, `comments` | Line counts of analysed text |
| `tokens` | Rough LLM token cost of analysed text (bytes / 4) |
| `byZone`, `byCategory`, `byGit`, `byLanguage` | `{name: {files, bytes, disk, code}}`; `byGit` keys are `tracked`, `untracked`, `ignored`, `none` (outside a work tree) |
| `hardlinkShared` | Bytes at paths whose inode was already counted at another path |

## Node

| Field | Meaning |
|---|---|
| `name` | Entry name (lossy UTF-8; see flag `nonUtf8`) |
| `parent` | Parent id; absent on the root |
| `kind` | `dir` · `file` · `symlink` · `special` · `rest` (folded "N smaller items") |
| `zone` | Origin: `source` (absent) · `vendored` · `dependency` · `artifact` · `vcs` |
| `cat` | `code` · `docs` · `config` · `data` · `image` · `media` · `archive` · `binary` · `font` · `generated` · `other` (absent) |
| `lang` | Index into `languages` |
| `flags` | Bitmask, names in `flagNames` (`binary`, `test`, `generated`, `mount`, `unreadable`, `repo`, `hardlinkDup`, `tooLarge`, `manifest`, `placeholder`, `nonUtf8`, `loop`) |
| `git` | `tracked` · `untracked` · `ignored`; absent outside work trees |
| `bytes` | Apparent size (directories: sum; hard links counted at every path) |
| `disk` | Allocated size (directories: sum; each hard-linked inode counted once) |
| `files` | Regular files below a directory (absent on files: a file is 1) |
| `mtime` | Newest modification time below (unix seconds) |
| `lines`, `code`, `comments` | Line counts (directories: sums); `code` = lines with something outside comments |
| `cx` | Indentation complexity: Σ logical indent of code lines |
| `commits` | Revisions inside the window (excludes merges, bots, ignore-revs, commits touching > 50 files); directories: sum over files |
| `recent` | Σ 0.5^(age / 90 days) over those revisions, age measured from the repo's newest commit |
| `lastCommit` | Newest commit touching the node (unix seconds) |
| `owner` | Top author by degree-of-authorship (index into `authors`) |
| `authors` | Number of developers with authorship (normalised DOA > 0.75) |
| `hotspot` | percentile(recent) × percentile(cx) among analysed code files of the same repository with history and `cx` > 0; 0–1 |
| `fanIn`, `fanOut` | Distinct internal files importing this file / imported by it |

## Repo

`node`, `path`, `prefix?` (scan root's path inside the repo), `head`, `shallow`, `partial`,
`tracked`, `untracked`, `ignoredBytes`, `error?`,
`hygiene` = `{dirty, branch, upstream, ahead, behind, stashes, unpushedBranches[], worktrees, prunableWorktrees}`,
`history` = `{commits, counted, skippedBots, skippedIgnoreRevs, skippedLarge, skippedShallow, first, last, truncated, authors, activeAuthors, truckFactor, truckAuthors[]}`
(`truckAuthors` is empty with `--authors none`).

`prefix` and `error` appear only when set. `head`, `hygiene` and `history` are `null` when
git could not provide them (no commits yet, an error).

## Hotspot, Pair, Graph, Finding

- **Hotspot**: `node`, `changePct`, `complexityPct`, `score`.
- **Pair**: `a`, `b`, `shared` (commits touching both), `confAb` = P(b changes | a changes),
  `confBa`, `lift`, `linked?` (`true` = an import path of ≤ 2 hops exists, `false` = none,
  absent = a language without an extractor, or `--no-deps`), `testPair`.
- **Graph**: `edges` = `{from, to, weight, up?, weak?}[]` (`up`: child module referencing its
  parent module file; `weak`: every import behind it is deferred or type-only, or it touches
  test code),
  `cycles` = `{parent, members[], weight, cuts: [from, to, weight][], cut, cutFiles: [from, to][]}[]`
  (`cuts`: a feedback arc set — removing those lifted edges makes the members acyclic;
  `cut`: the lightest of them),
  `stats` = `{lang, files, imports, internal, external}[]`.
- **Finding**: `id` (stable: `rule:hash(paths)`), `rule`, `severity` (`high`/`medium`/`low`/`info`),
  `title`, `detail`, `action`, `command?`, `nodes[]`, `paths[]`, `bytes?`, `new?` (with `--baseline`).

## Recipes

```bash
# Paths of the 10 largest files
fsmap scan . | jq -r '
  .nodes as $n
  | def p(i): if $n[i].parent == null then "" else (p($n[i].parent) + "/" + $n[i].name) end;
  [range(0; $n|length) | select($n[.].kind == "file")] | sort_by(-($n[.].disk // 0)) | .[:10][] | p(.) | ltrimstr("/")'

# Findings at or above medium, as TSV
fsmap scan . | jq -r '.findings[] | select(.severity == "high" or .severity == "medium") | [.severity, .rule, .title] | @tsv'
```
