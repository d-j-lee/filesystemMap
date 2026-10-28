# Rust × AI — evidence for the integrated playbook (2026-10-29)

> Dated, not a living document. When the playbook was merged with two parallel write-ups
> (an artifact page with its `rust-ai-harness`, and another session's `rust-ai-template`), every
> external claim they added was checked against its primary source by a fact-checking sub-agent,
> with verbatim quotes. **✔** = additionally re-fetched and confirmed by the playbook's author in
> this session. Earlier snapshots: [2026-10](rust-ai-evidence-2026-10.md),
> [2026-10-28 addendum](rust-ai-evidence-addendum-2026-10-28.md).

## Correction to the earlier snapshots

**CRUST-Bench.** Both earlier files pair o1's single-shot 15 % with o3's 48 %, and the
2026-10-28 addendum concluded that the loop's effect was therefore not isolated. That was wrong.
The paper's own Table 4 (v2/v3) gives same-model numbers; its v3 body text and the arXiv abstract
metadata still carry a stale "o1 … 15 %" sentence from v1, which is where the mix-up came from.

| OpenAI o3 on CRUST-Bench (C → safe Rust, 100 repos) | Build | Test pass |
|---|---|---|
| Single shot | 35 % | 19 % |
| + 3 rounds of compiler-error repair | 68 % | 31 % |
| + 3 rounds of compiler and test-failure repair | 63 % | **48 %** |

Source: [arXiv 2504.15254v3](https://arxiv.org/html/2504.15254v3), Table 4 and §5 ✔. Other
models in the table: Claude Opus 4 22 % → 40 %, o1 15 % → 37 %, Claude 3.7 Sonnet 13 % → 32 %.
The paper's "matches, rather than exceeds" sentence compares a $2 SWE-agent run (Claude 3.7,
32 %) with Claude 3.7 plus test repair (32 %), not with o3.

## New rows

| Fact | Source | |
|---|---|---|
| 2025 State of Rust survey: 48.8 % say their organisation makes non-trivial use of Rust (38.7 % in 2023, 45.5 % in 2024); top worries "not enough usage in the tech industry" 42.1 % and "becomes too complex" 41.6 %; 7,156 responses. Respondents are mostly Rust users, so this is not "half of all companies". | [Rust blog, 2026-03-02](https://blog.rust-lang.org/2026/03/02/2025-State-Of-Rust-Survey-results/), organisation figures in the [full report PDF](https://raw.githubusercontent.com/rust-lang/surveys/main/surveys/2025/annual-survey/report/annual-survey-2025-report.pdf) p. 42 | sub-agent |
| Linux kernel: at the 2025-12-10 Maintainers Summit the Rust experiment was declared done ("Rust is here to stay"); `CONFIG_RUST` is still off by default. | [LWN](https://lwn.net/Articles/1049831/) | sub-agent |
| Android: in Q1–Q3 2025 Rust's **net** lines (additions minus deletions) in first-party platform code slightly surpassed C++'s. The ~1000× density figure is ~5 M lines of Rust with one near-miss (0.2 per MLOC) against ~1,000 per MLOC historically for C/C++. Google "speculate[s]" that the 2023→2024 review-time shift came from growing Rust expertise. | [Google Security Blog, 2025-11-13](https://security.googleblog.com/2025/11/rust-in-android-move-fast-fix-things.html) | sub-agent |
| RustAssistant's ~74 % peak is GPT-4. | [Microsoft Research](https://www.microsoft.com/en-us/research/wp-content/uploads/2024/08/paper.pdf) | sub-agent |
| RustEvo²: retrieval's +13.5 pp (34.9 % → 48.4 %) applies to APIs released after the model's cutoff; APIs whose signature stayed but behaviour changed: 38.0 %. | [arXiv 2503.16922](https://arxiv.org/abs/2503.16922) | sub-agent |
| Repository-level Rust issue resolution (ICSE 2026): 43.7 % of compile errors come from repository structure, 32.6 % from type/trait rules. | [arXiv 2602.22764](https://arxiv.org/abs/2602.22764) | sub-agent |
| Post-generation compiler feedback cuts the non-compiling rate from 65.9 % to 20.7 % on average (7 models × 2 repository-level Rust tasks); feedback during generation reaches 13.1 %. | Mündler-Sasahara et al., "Generative Compilation", [arXiv 2607.13921](https://arxiv.org/abs/2607.13921) (2026-07) | sub-agent |
| METR: early-2025 RCT (16 experienced developers, own repos) — tasks took 19 % longer with AI while developers believed they were ~20 % faster. 2026-02-24 update: 10 returning developers −18 % task time, 47 new −4 %; both intervals include zero; METR thinks the speed-up is likely larger now but hard to measure because developers decline to work without AI. | [METR 2025-07-10](https://metr.org/blog/2025-07-10-early-2025-ai-experienced-os-dev-study/), [METR 2026-02-24](https://metr.org/blog/2026-02-24-uplift-update/) | sub-agent |
| Rust Project Goals 2026 "fast builds": parallel front end, Cranelift backend, Relink-Don't-Rebuild, TPDE backend, Wild linker, incremental redesign. | [goals.rust-lang.org](https://goals.rust-lang.org/2026/roadmap-fast-builds.html) | sub-agent |
| PyO3 `allow_threads` → `detach`: renamed and deprecated in 0.26.0 (2025-08-29), removed in 0.28.0 (2026-02-01); latest 0.30.1 (2026-10-14). | [PyO3 CHANGELOG](https://github.com/PyO3/pyo3/blob/main/CHANGELOG.md), crates.io | sub-agent |
| rmcp (official Rust MCP SDK) 3.8.0, published 2026-10-26; releases every few days. | crates.io API | sub-agent |
| Claude Code cloud sessions: no plugin language servers and no `/plugin`; the repository's `.claude/settings.json` hooks and permission rules apply **only in a session with one repository, outside a Project thread**; `.claude/agents` load outside Project threads; managed `allowManagedPermissionRulesOnly` overrides repo permission rules. `rust-analyzer-lsp` needs the `rust-analyzer` binary. | [cloud-environments](https://code.claude.com/docs/en/cloud-environments.md), [code-intelligence](https://code.claude.com/docs/en/plugins/code-intelligence.md) | sub-agent |
| Claude Code hooks: PostToolUse cannot block (exit 2 shows stderr to Claude); Stop `decision: "block"` keeps the turn going, with `stop_hook_active` and an 8-consecutive-continuation cap; SessionStart stdout is added to context; hooks run in parallel; default timeout 600 s. Permissions: ask rules prompt even in auto mode; argument-constraining Bash patterns are bypassable; only `Edit(path)`/`Read(path)` path rules match. | [hooks](https://code.claude.com/docs/en/hooks), [permissions](https://code.claude.com/docs/en/permissions) | ✔ |
| Stack Overflow 2026: Rust used by 18.0 % (12th), admired 67.4 %. | [survey](https://survey.stackoverflow.co/2026/technology/data/languages) | ✔ (2026-10 file) |
