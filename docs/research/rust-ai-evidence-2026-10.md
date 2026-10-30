# Rust × AI — evidence snapshot (2026-10-28)

> A dated reference sheet, not a living document. It backs the claims in
> [../rust-ai-playbook.md](../rust-ai-playbook.md). Re-check before quoting after 2027.
>
> **Provenance.** Compiled by a research sub-agent from primary sources, then spot-checked:
> rows marked **✔** were re-fetched and confirmed verbatim in this session; the rest are
> "as reported by the source", confidence noted. `[interp]` = interpretation, not a source claim.

## 1. Adoption: loved vs. used

| Fact | Source | |
|---|---|---|
| Rust used by **18.0 %** (2,542 / 14,099) of all respondents, **12th** most used | [Stack Overflow Survey 2026](https://survey.stackoverflow.co/2026/technology/data/languages) (2026-10-06) | ✔ |
| 2026: admired 67.4 %, desired 26.3 % | [SO 2026 admired/desired](https://survey.stackoverflow.co/2026/technology/data/da-language) | high |
| 2025: admired 72.4 %, used by 14.8 % | [SO 2025](https://survey.stackoverflow.co/2025/technology/) | high |
| JetBrains Rust survey 2025: 26 % use Rust professionally; 78 % actively use AI assistants | [JetBrains RustRover blog](https://blog.jetbrains.com/rust/2026/02/11/state-of-rust-2025/) (2026-02-11) | high |
| State of Rust 2025: slow compilation a "big problem" for 27.3 % (+54.7 % "could be improved"); people "moving their questions to LLM tooling" | [Rust blog](https://blog.rust-lang.org/2026/03/02/2025-State-Of-Rust-Survey-results/) (2026-03-02) | high |

`[interp]` Two-thirds of users want to keep using Rust, yet fewer than one in five developers
use it: the gap is adoption cost, which is exactly what AI assistance compresses.

## 2. Industry moves

| Fact | Source | |
|---|---|---|
| Android: memory-safety share of vulnerabilities **below 20 %** for the first time; Rust ≈ **0.2** vs C/C++ ≈ **1,000** memory-safety vulns per MLOC; Rust changes **~4× lower rollback**, **~25 % less review time** | [Google, "Rust in Android: move fast and fix things"](https://blog.google/security/rust-in-android-move-fast-fix-things/) (2025-11-13) | ✔ |
| Linux kernel: Maintainers Summit agreed Rust "is no longer experimental" | [LWN](https://lwn.net/Articles/1049831/) (2025-12-10) | high |
| Chromium: Rust font stack (Fontations/Skrifa) for web fonts since Chrome 133, FreeType removed from Blink in 145 | [Chrome for Developers](https://developer.chrome.com/blog/memory-safety-fonts) | high |
| Chromium: C++ JSON parser fully replaced by Rust (Q2 2025); Rust PNG codec replaces libpng (Q4 2025); Site-Isolation policy code moving to Rust, experiment on Canary (Q1 2026) | [Chrome Security quarterly updates](https://www.chromium.org/Home/chromium-security/quarterly-updates/) | high |
| Git: Rust optional since 2.49, default-on planned for 2.55, mandatory planned for 3.0 (may slip) | [git/Documentation/BreakingChanges.adoc](https://raw.githubusercontent.com/git/git/master/Documentation/BreakingChanges.adoc) | high |
| Ubuntu: sudo-rs default from 25.10; rust-coreutils in 26.04 with `cp/mv/rm` still GNU pending TOCTOU fixes | [Ubuntu Discourse](https://discourse.ubuntu.com/t/an-update-on-rust-coreutils/80773) | high |
| Microsoft "eliminate C/C++ by 2030, 1 engineer/1 month/1M lines" was one engineer's research framing, publicly walked back ("Windows is NOT being rewritten in Rust with AI") | [InfoWorld](https://www.infoworld.com/article/4111553/microsoft-is-not-rewriting-windows-in-rust.html) (2025-12-24) | high |

## 3. Counter-examples

| Fact | Source | |
|---|---|---|
| TypeScript's native compiler was ported to **Go**: a port, not a rewrite; structural similarity and GC suit the codebase | [typescript-go discussion #411](https://github.com/microsoft/typescript-go/discussions/411) | high |
| Ladybird rejected Rust in 2024, adopted it in 2026 after Swift's C++ interop stalled | [Ladybird blog](https://ladybird.org/posts/adopting-rust/) (2026-02-23) | high |

## 4. LLMs and agents writing Rust

| Fact | Source | |
|---|---|---|
| Iterating an LLM on compiler feedback fixes real Rust compile errors with **~74 %** peak accuracy (RustAssistant) | [arXiv 2308.05177](https://arxiv.org/abs/2308.05177) | ✔ |
| CRUST-Bench (C→safe Rust, 100 repos): o1 single-shot 15 %; compiler+test repair loop (o3) 48 % pass; an agent at $2 budget "matches, rather than exceeds" the simple loop | [arXiv 2504.15254](https://arxiv.org/html/2504.15254v3) | high |
| Bun: **535,496 lines** of Zig ported to Rust in **11 days** by a pre-release Claude model via Claude Code workflows (~64 agents at peak, ~$165k API); all 6 platforms green, 0 tests skipped; ~4 % of code in `unsafe`; 19 regressions, all fixed. "Compiler errors are a better feedback loop than a style guide." Self-reported; Bun is owned by Anthropic. | [bun.com/blog/bun-in-rust](https://bun.com/blog/bun-in-rust) (2026-07-08) | ✔ |
| Ladybird LibJS: ~25k lines C++→Rust in ~2 weeks with AI, verified by byte-identical AST/bytecode and 0 regressions on test262 | [Ladybird blog](https://ladybird.org/posts/adopting-rust/) | high |
| SWE-bench Multilingual: Rust had the highest resolve rate of the languages (58 %, 43 tasks); Multi-SWE-bench: Rust mid-pack | [swebench.com/multilingual](https://www.swebench.com/multilingual.html), [arXiv 2504.02605](https://arxiv.org/abs/2504.02605) | medium |
| Package hallucination: Rust 24.7 % vs Python 23.1 % vs JS 14.7 % | [arXiv 2501.19012](https://arxiv.org/html/2501.19012) | high |
| API drift: 56.1 % success on APIs from before the model's cutoff vs 32.5 % after; retrieval adds +13.5 pts | [RustEvo², arXiv 2503.16922](https://arxiv.org/abs/2503.16922) | high |

## 5. Tooling

| Fact | Source | |
|---|---|---|
| Claude Code supports LSP plugins; an official `rust-analyzer-lsp` plugin exists (rust-analyzer binary installed separately) | [Claude Code CHANGELOG](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md), [plugin page](https://claude.com/marketplace/plugins/rust-analyzer-lsp) | medium |
| Claude Code hooks: `hookSpecificOutput.additionalContext` adds context on PreToolUse/PostToolUse/SessionStart/UserPromptSubmit; read-deny rules use `permissions.deny` with gitignore-style `Read(...)` patterns | [hooks docs](https://code.claude.com/docs/en/hooks), [permissions docs](https://code.claude.com/docs/en/permissions) | medium (doc summaries) |
| Rust under tools you already use: uv / ruff / ty (Astral), Polars 2.x, pydantic-core, Vite 8 bundles with Rolldown + Oxc, Turbopack default in Next.js 16, Rspack 2 | PyPI / npm / project blogs as of 2026-10-28 | high |

## Caveats

- "Most admired" measures retention among users, not popularity.
- Benchmarks disagree on how hard Rust is for LLMs (best on SWE-bench Multilingual, mid on Multi-SWE-bench, worst on package hallucination).
- Android's decline is credited to memory-safe languages in general, not Rust alone.
- Bun and Ladybird figures are self-reported.
