# Rust × AI — evidence addendum (2026-10-28)

> Dated, not a living document. It adds sources for playbook claims that the
> [first snapshot](rust-ai-evidence-2026-10.md) did not cover; the D-14 documentation audit
> found them unsourced. Every row was fetched and read in this session (**✔**). Quotes are
> verbatim. Chromium files were read through the `github.com/chromium/chromium` mirror.

| Fact | Source | |
|---|---|---|
| Bun port: "Claude interpreted 'let's get all the crates to compile' as 'stub out the functions with compilation errors'"; reviewers were told "If you need a paragraph-long comment to justify why the workaround is OK, the code is wrong — fix the code." The fix was a prompt change, not a lint. | [bun.com/blog/bun-in-rust](https://bun.com/blog/bun-in-rust) | ✔ |
| Chromium "Rule of 2": "Pick no more than 2 of: untrustworthy inputs; unsafe implementation language; and high privilege." Rust is listed among memory-safe languages; its `unsafe` "functions as a separate unsafe language subset". | [chromium docs/security/rule-of-2.md](https://chromium.googlesource.com/chromium/src/+/main/docs/security/rule-of-2.md) | ✔ |
| Chromium's 2023 starting point: "support the use of third-party Rust libraries from C++", and "interop in only a single direction, from C++ to Rust, for now" (2023-01-12). | [Google Security Blog](https://security.googleblog.com/2023/01/supporting-use-of-rust-in-chromium.html) | ✔ |
| Chromium today documents first-party Rust libraries, uses cxx for interop and supports Crubit for calling Rust from C++. | [chromium docs/rust/README.md](https://chromium.googlesource.com/chromium/src/+/main/docs/rust/README.md) | ✔ |
| Chromium `DEPS` files "specify which files the sources in a directory tree may include", checked by checkdeps; "The code review changes and OWNERS policy changes launched on March 24, 2021" ("Mandatory Code Review and Native OWNERS"). | [buildtools/checkdeps/README.md](https://chromium.googlesource.com/chromium/src/+/main/buildtools/checkdeps/README.md), [docs/code_reviews.md](https://chromium.googlesource.com/chromium/src/+/main/docs/code_reviews.md) | ✔ |
| chromiumoxide: Rust crate for driving Chrome over the DevTools Protocol (0.9.1 on crates.io). Playwright MCP: Microsoft's MCP server for browser automation through accessibility snapshots. | [crates.io/crates/chromiumoxide](https://crates.io/crates/chromiumoxide), [github.com/microsoft/playwright-mcp](https://github.com/microsoft/playwright-mcp) | ✔ |
| Claude Code permissions: a `Read` deny rule blocks Claude's file tools from reading a path; a leading `/` in a rule is relative to the settings source (the project, for project settings). | [code.claude.com/docs/en/permissions](https://code.claude.com/docs/en/permissions) | ✔ |

Correction to the first snapshot's use in the playbook: CRUST-Bench's 15 % (o1, single
shot) and 48 % (o3, compile-and-test loop) come from different models, so they do not
isolate the effect of the loop. The playbook no longer calls it a 3× gain.
