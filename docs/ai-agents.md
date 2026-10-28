# Using fsmap with AI coding agents

Three integrations, from zero setup to automatic. The examples target Claude Code; the
outputs are plain Markdown/JSON, so any agent that reads files or runs commands can use them.

## 1. A repo map in the agent's context — `fsmap context`

```bash
fsmap context . --budget 4000 > REPO_MAP.md
```

Then reference it from `CLAUDE.md` (or `AGENTS.md`):

```markdown
Read REPO_MAP.md before large changes: layout, entry points, module dependencies,
hotspots (edit with tests), files that change together, and paths not worth reading.
```

Properties that matter for agents:

- **Deterministic**: the same tree produces byte-identical output, so the map can sit in a
  cached prompt prefix. Regenerate it after large changes, not on every turn.
- **Budgeted**: `--budget` caps the size; the layout expands the largest code areas first.
- **Contents-free**: paths, metrics and history only — never file contents or commit
  messages, which could carry prompt injection.

## 2. "You forgot B" — `fsmap brief`

```bash
fsmap brief src/parser.rs          # what to know before touching a file
fsmap brief --diff                 # check the working tree against HEAD
fsmap brief --diff origin/main     # check a whole branch
```

`--diff` lists files that change with your changed files in at least half of their
commits but are missing from the change set — the check no linter does. Results are cached per
repository and `HEAD`, so repeated calls take milliseconds.

### As a Claude Code hook

`brief --hook` reads the hook event from stdin and, only when the file is notable (has
co-change partners, is generated, is a hotspot, or is widely imported), prints
`hookSpecificOutput.additionalContext` for Claude. Otherwise it prints nothing. It never
fails the hook.

`.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [{ "type": "command", "command": "fsmap brief --hook" }]
      }
    ]
  }
}
```

PostToolUse is the documented place for context-only output. After Claude edits
`client/api.py`, it is told, for example, that `proto/schema.txt` changed in 100 % of that
file's commits. Hook field names follow the Claude Code hooks documentation as of
2026-10 (`https://code.claude.com/docs/en/hooks`). If a future version renames them, check
that page first.

## 3. Keep agents out of bulky paths — `fsmap agent-ignore`

```bash
fsmap agent-ignore .                     # Claude Code permissions.deny entries
fsmap agent-ignore . --format gitignore  # .cursorignore / .aiderignore / similar
```

The rules cover whole directories the analysis classified as dependency, build output or
vendored code (the outermost one of each, 64 KiB or more). `--format gitignore` adds the
reason and an approximate token cost to each line as a comment; the `claude` format is plain
JSON for `settings.json`.

Generated files and large data files inside source directories are *not* turned into rules:
they sit next to hand-written files, and a directory-wide deny would hide those too.
`fsmap context` lists them in its "Skip when reading" section instead.

Paste the `claude` output into the **project's** `.claude/settings.json`: a leading `/` in a
rule is relative to the settings file's project, so the same rule in user settings would
point somewhere else. Review the list first. A `Read(...)` deny stops Claude's file tools
from reading those paths, which in practice also gets in the way of editing them
(permissions documentation, `https://code.claude.com/docs/en/permissions`, as of 2026-10).

## 4. In CI — the ratchet

```bash
fsmap insights . --write-baseline .fsmap-baseline.json   # once, commit the file
fsmap insights . --baseline .fsmap-baseline.json --fail-on medium
```

Only findings that are **new** relative to the baseline fail the build: a new import cycle, a
newly committed `dist/` or `target/`, a secret-looking file. Existing debt does not block work but
cannot grow. `--format sarif` feeds GitHub code scanning.
