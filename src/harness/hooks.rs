//! Claude Code hooks (decision D-15). Each reads the hook's JSON from stdin and prints at
//! most one JSON object; the exit code is always 0. Any problem that keeps a hook from
//! doing its job becomes a `systemMessage` the user sees: the gate never fails open
//! silently (review finding M3).
//!
//! | Hook | Event | What it does |
//! |---|---|---|
//! | `session-start` | SessionStart | records where the session started; tells Claude the toolchain, dependency sources and rules |
//! | `post-edit` | PostToolUse `Edit\|Write` | `cargo check` on the edited package; errors back to Claude; the fsmap brief |
//! | `stop` | Stop | the gate: `fsmap verify` when the tree changed; blocks the stop on new failures |
//! | `prompt` | UserPromptSubmit | the user wrote: reset the failure streak |
//! | `pre-bash` | PreToolUse `Bash` | the gate before `git push` / `gh pr create` / `gh pr merge` |

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::verify::{self, Config, Mode, Report};
use super::{proc, state};
use crate::brief;

/// Budget for the stop and pre-push gates; the settings give the hook 900 s.
const GATE_BUDGET: Duration = Duration::from_secs(600);
/// Budget for the post-edit check.
const CHECK_BUDGET: Duration = Duration::from_secs(60);
/// Consecutive blocking failures before the gate lets go and asks for a report.
const MAX_STRIKES: u32 = 3;
/// Background task types that edit the tree; the gate waits for them.
const AGENT_TASKS: &[&str] = &["subagent", "workflow", "teammate", "cloud session"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Event {
    SessionStart,
    PostEdit,
    Stop,
    Prompt,
    PreBash,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "session-start",
            Event::PostEdit => "post-edit",
            Event::Stop => "stop",
            Event::Prompt => "prompt",
            Event::PreBash => "pre-bash",
        }
    }
}

/// What a hook prints.
#[derive(Default)]
struct Out {
    /// Shown to the user.
    system: Vec<String>,
    /// Added to Claude's context.
    context: Vec<String>,
    /// PostToolUse/Stop `decision: "block"` with this reason.
    block: Option<String>,
    /// PreToolUse `permissionDecision: "deny"` with this reason.
    deny: Option<String>,
}

impl Out {
    fn render(self, event: Event) -> Option<String> {
        if self.system.is_empty()
            && self.context.is_empty()
            && self.block.is_none()
            && self.deny.is_none()
        {
            return None;
        }
        let hook_event = match event {
            Event::SessionStart => "SessionStart",
            Event::PostEdit => "PostToolUse",
            Event::Stop => "Stop",
            Event::Prompt => "UserPromptSubmit",
            Event::PreBash => "PreToolUse",
        };
        let mut v = json!({});
        if !self.system.is_empty() {
            v["systemMessage"] = Value::String(self.system.join("\n"));
        }
        if let Some(reason) = self.block {
            v["decision"] = json!("block");
            v["reason"] = Value::String(reason);
        }
        let mut specific = json!({ "hookEventName": hook_event });
        if !self.context.is_empty() {
            specific["additionalContext"] = Value::String(self.context.join("\n\n"));
        }
        if let Some(reason) = self.deny {
            specific["permissionDecision"] = json!("deny");
            specific["permissionDecisionReason"] = Value::String(reason);
        }
        if specific.as_object().is_some_and(|o| o.len() > 1) {
            v["hookSpecificOutput"] = specific;
        }
        Some(v.to_string())
    }
}

/// Run one hook on its stdin; returns what to print (nothing for "no opinion").
pub fn run(event: Event, stdin: &str) -> Option<String> {
    let input: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let out = match handle(event, &input) {
        Ok(out) => out,
        Err(e) => Out {
            system: vec![format!(
                "[harness] {} hook did not run: {e:#}",
                event.name()
            )],
            ..Out::default()
        },
    };
    out.render(event)
}

fn handle(event: Event, input: &Value) -> Result<Out> {
    let project = project_dir(input);
    match event {
        Event::SessionStart => session_start(&project, input),
        Event::PostEdit => post_edit(&project, input),
        Event::Stop => stop(&project, input),
        Event::Prompt => prompt(&project, input),
        Event::PreBash => pre_bash(&project, input),
    }
}

/// The project root: `CLAUDE_PROJECT_DIR`, else the hook's `cwd` (the agent may have
/// `cd`'d elsewhere, so the variable wins), canonicalised.
fn project_dir(input: &Value) -> PathBuf {
    let p = std::env::var_os("CLAUDE_PROJECT_DIR")
        .map(PathBuf::from)
        .or_else(|| input["cwd"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn session_id(input: &Value) -> String {
    input["session_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
}

/// The Cargo workspace for the project, with its verify configuration.
fn workspace(project: &Path) -> Option<(PathBuf, Config)> {
    let ws = verify::workspace_root(project).ok()?;
    let cfg = verify::config(&ws).unwrap_or_default();
    Some((ws, cfg))
}

// ---------------------------------------------------------------- session-start

fn session_start(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let source = input["source"].as_str().unwrap_or("startup");
    let sid = session_id(input);
    let Some((ws, cfg)) = workspace(project) else {
        out.system.push(format!(
            "[harness] no Cargo workspace at {}: the Rust gate is inactive in this session",
            project.display()
        ));
        return Ok(out);
    };
    let mut st = state::load(&ws);
    let fp = state::fingerprint(&ws, &cfg.trust);
    let now = state::now();
    let others = st.live_others(&sid, now);
    {
        let s = st.session(&sid);
        s.last_seen = now;
        if source == "startup" && s.start_fp.is_none() {
            s.start_fp.clone_from(&fp);
            s.start_head = state::head(&ws);
        }
    }
    if fp.is_some() && fp != st.last_pass_fp && source != "startup" {
        out.context.push(
            "[harness] the working tree has changes the gate has not passed yet; it runs when \
             you finish this turn."
                .into(),
        );
    }
    if !others.is_empty() {
        let msg = format!(
            "[harness] another Claude session used this worktree in the last hour ({}). Edits \
             from both sessions share one gate; prefer a separate worktree per session.",
            others.join(", ")
        );
        out.system.push(msg.clone());
        out.context.push(msg);
    }
    if !state::save(&ws, &mut st) {
        out.system.push(
            "[harness] cannot write the harness state: the gate will run on every stop".into(),
        );
    }
    out.context.push(briefing(&ws, &cfg));
    let findings = super::integrity::scan(&ws, &cfg.trust);
    if !findings.is_empty() {
        let list: Vec<String> = findings
            .iter()
            .map(|f| format!("{}:{} {}", f.file, f.line, f.kind))
            .collect();
        out.system.push(format!(
            "[harness] settings that weaken the gate (it will fail until they are removed or \
             trusted): {}",
            list.join(", ")
        ));
    }
    // Warm the brief index so the first post-edit is fast; failures only cost speed.
    if let Some((root, head)) = brief::locate(&ws) {
        let _ = brief::index(&root, Some(&head), false, &Default::default());
    }
    state::log(
        &ws,
        &json!({"t": now, "event": "session-start", "session": sid, "source": source}),
    );
    Ok(out)
}

fn briefing(ws: &Path, cfg: &Config) -> String {
    let mut lines = vec![format!(
        "[harness] fsmap {} hooks are active. After each edit `cargo check` runs on the \
         edited package and errors come back to you. Finishing a turn runs `fsmap verify` \
         (integrity, fmt, clippy -D warnings, tests{}); `git push` runs it first. Fix \
         causes: do not add #[allow]/#[expect], skip or weaken tests, or change cargo, \
         rustfmt or toolchain settings to get green.",
        env!("CARGO_PKG_VERSION"),
        if cfg.extra.is_empty() {
            ""
        } else {
            ", the repository's extra checks"
        }
    )];
    let version = |prog: &str| {
        let mut c = std::process::Command::new(prog);
        c.arg("-V").current_dir(ws);
        let o = proc::run(c, Duration::from_secs(20));
        o.success().then(|| o.output.trim().to_string())
    };
    let tools: Vec<String> = ["rustc", "cargo"]
        .iter()
        .filter_map(|p| version(p))
        .collect();
    if !tools.is_empty() {
        lines.push(format!(
            "Toolchain (use these, not remembered versions): {}",
            tools.join("; ")
        ));
    }
    let sources = dependency_sources(ws);
    if !sources.is_empty() {
        lines.push(format!(
            "Before using a crate API you are unsure of, read its real source:\n{}",
            sources.join("\n")
        ));
    }
    lines.join("\n")
}

/// `name version: dir` for the workspace's direct registry dependencies (at most 20).
fn dependency_sources(ws: &Path) -> Vec<String> {
    // The whole graph is needed (it is larger than proc::run keeps), and --offline cannot
    // hang on the network, so this reads the full output directly.
    let Ok(o) = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--offline"])
        .current_dir(ws)
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(meta) = serde_json::from_slice::<Value>(&o.stdout) else {
        return Vec::new();
    };
    let empty = Vec::new();
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages = meta["packages"].as_array().unwrap_or(&empty);
    let direct: std::collections::BTreeSet<&str> = packages
        .iter()
        .filter(|p| p["id"].as_str().is_some_and(|id| members.contains(&id)))
        .flat_map(|p| p["dependencies"].as_array().unwrap_or(&empty).iter())
        .filter(|d| {
            d["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("registry+"))
        })
        .filter_map(|d| d["name"].as_str())
        .collect();
    let mut out: Vec<String> = packages
        .iter()
        .filter(|p| p["name"].as_str().is_some_and(|n| direct.contains(n)))
        .filter(|p| {
            p["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("registry+"))
        })
        .filter_map(|p| {
            let dir = Path::new(p["manifest_path"].as_str()?).parent()?;
            Some(format!(
                "  {} {}: {}",
                p["name"].as_str()?,
                p["version"].as_str()?,
                dir.display()
            ))
        })
        .collect();
    out.sort();
    out.dedup();
    out.truncate(20);
    out
}

// ---------------------------------------------------------------- post-edit

fn post_edit(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let Some(file) = input["tool_input"]["file_path"].as_str() else {
        return Ok(out);
    };
    let path = PathBuf::from(file);
    // The fsmap brief for notable files (co-change partners, hotspots, widely imported).
    if let Some((root, head)) = brief::locate(project)
        && let Some(idx) = brief::load_cached(&root, &head)
    {
        let owners = brief::CodeOwners::load(&root);
        if let Some(text) = brief::hook_context(&idx, owners.as_ref(), &root, file, "PostToolUse") {
            out.context.push(text);
        }
    }
    let is_rust = path.extension().is_some_and(|e| e == "rs")
        || path
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "build.rs");
    if !is_rust {
        return Ok(out);
    }
    let Some((ws, cfg)) = workspace(project) else {
        return Ok(out);
    };
    let Some(manifest) = package_manifest(&ws, &path) else {
        return Ok(out);
    };
    let Some(_lock) = CheckLock::acquire(&ws) else {
        // A check is already running; its result covers this edit too.
        return Ok(out);
    };
    let mut c = verify::step_command(&ws, "cargo", &cfg);
    c.args([
        "check",
        "--all-targets",
        "--message-format=short",
        "--offline",
    ]);
    if ws.join("Cargo.lock").is_file() {
        c.arg("--locked");
    }
    c.arg("--manifest-path").arg(&manifest);
    let o = proc::run(c, CHECK_BUDGET);
    state::log(
        &ws,
        &json!({"t": state::now(), "event": "post-edit", "session": session_id(input),
                "ms": o.ms, "ok": o.success(), "timed_out": o.timed_out}),
    );
    if o.timed_out {
        out.system.push(format!(
            "[harness] cargo check took over {} s and was stopped; errors will surface at the \
             end-of-turn gate",
            CHECK_BUDGET.as_secs()
        ));
        return Ok(out);
    }
    if let Some(e) = o.start_error {
        out.system
            .push(format!("[harness] could not run cargo check: {e}"));
        return Ok(out);
    }
    if o.success() {
        return Ok(out);
    }
    let errors = error_lines(&o.output, &ws);
    let rel = path
        .strip_prefix(&ws)
        .unwrap_or(&path)
        .display()
        .to_string();
    if errors.is_empty() {
        // Not a compile error in the code: the check itself could not run (offline with
        // missing dependencies, a broken manifest).
        out.context.push(format!(
            "[harness] cargo check could not check {rel}:\n{}",
            verify::tail(&o.output, 8)
        ));
        return Ok(out);
    }
    let total = errors.len();
    let shown: Vec<&String> = errors.iter().take(20).collect();
    out.block = Some(format!(
        "cargo check after editing {rel}: {total} error(s){}\n{}",
        if total > shown.len() {
            format!(", first {}", shown.len())
        } else {
            String::new()
        },
        shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    ));
    Ok(out)
}

/// The nearest `Cargo.toml` at or above the edited file, inside the workspace.
fn package_manifest(ws: &Path, file: &Path) -> Option<PathBuf> {
    let file = std::fs::canonicalize(file).ok().or_else(|| {
        Some(
            std::fs::canonicalize(file.parent()?)
                .ok()?
                .join(file.file_name()?),
        )
    })?;
    if !file.starts_with(ws) {
        return None;
    }
    file.ancestors()
        .skip(1)
        .take_while(|d| d.starts_with(ws))
        .map(|d| d.join("Cargo.toml"))
        .find(|m| m.is_file())
}

/// Error diagnostics from `--message-format=short` output, de-duplicated (a library and
/// its test target report the same error twice), in order, relative to the workspace.
fn error_lines(output: &str, ws: &Path) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    verify::sanitize(output)
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            let (path, level) = verify::diagnostic(l)?;
            if level != "error" {
                return None;
            }
            let shown = verify::rel_path(ws, path).unwrap_or_else(|| path.replace('\\', "/"));
            Some(format!("{shown}{}", &l[path.len()..]))
        })
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

/// At most one post-edit check per worktree at a time (parallel edits queue on cargo's
/// build lock otherwise). Stale after two minutes.
struct CheckLock(PathBuf);

impl CheckLock {
    fn acquire(ws: &Path) -> Option<CheckLock> {
        let path = state::dir(ws).join("check.lock");
        if let Ok(meta) = std::fs::metadata(&path)
            && meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(120))
        {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()
            .map(|_| CheckLock(path))
    }
}

impl Drop for CheckLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------- stop / pre-bash

/// The result of running the gate for one hook call.
enum Gate {
    /// Nothing to do: the tree is unchanged since the session started or last passed.
    Unchanged,
    /// Passed, with notes for the user (pre-existing failures, added suppressions).
    Pass(Vec<String>),
    /// New failures: the reason to give Claude.
    Fail(String),
}

fn gate(ws: &Path, cfg: &Config, st: &mut state::State, sid: &str) -> Result<Gate> {
    let fp = state::fingerprint(ws, &cfg.trust)
        .context("could not read the git status of the working tree")?;
    let session = st.session(sid).clone();
    if st.last_pass_fp.as_deref() == Some(fp.as_str())
        || session.start_fp.as_deref() == Some(fp.as_str())
    {
        return Ok(Gate::Unchanged);
    }
    if st.last_fail_fp.as_deref() == Some(fp.as_str())
        && let Some(reason) = st.last_fail_reason.clone()
    {
        return Ok(Gate::Fail(reason));
    }
    let exe = std::env::current_exe().context("locating the fsmap binary")?;
    let steps = verify::plan(ws, Mode::Quick, &exe, cfg);
    let report = verify::run(ws, Mode::Quick, &steps, cfg, GATE_BUDGET, |_| {});
    let touched = touched_files(ws, session.start_head.as_deref());
    let (blocking, preexisting) = split_failures(&report, &touched);
    if blocking.is_empty() {
        st.last_pass_fp = Some(fp);
        st.last_fail_fp = None;
        st.last_fail_reason = None;
        let mut notes = Vec::new();
        if !preexisting.is_empty() {
            notes.push(format!(
                "[harness] gate passed for this session's changes; still failing in files it \
                 did not touch: {}",
                preexisting.join(", ")
            ));
        }
        if let Some(s) = suppression_summary(ws, session.start_head.as_deref()) {
            notes.push(s);
        }
        return Ok(Gate::Pass(notes));
    }
    let reason = failure_reason(&report, &blocking);
    st.last_fail_fp = Some(fp);
    st.last_fail_reason = Some(reason.clone());
    Ok(Gate::Fail(reason))
}

fn stop(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let sid = session_id(input);
    let Some((ws, cfg)) = workspace(project) else {
        return Ok(out);
    };
    let mut st = state::load(&ws);
    let now = state::now();
    st.session(&sid).last_seen = now;
    let active = input["stop_hook_active"].as_bool().unwrap_or(false);
    if active && st.session(&sid).released {
        state::save(&ws, &mut st);
        return Ok(out);
    }
    let agents: Vec<&str> = input["background_tasks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["type"].as_str())
                .filter(|t| AGENT_TASKS.contains(t))
                .collect()
        })
        .unwrap_or_default();
    if !agents.is_empty() {
        out.system.push(format!(
            "[harness] gate not run: background agents are still working ({}); it runs when \
             the turn really ends",
            agents.join(", ")
        ));
        log_gate(&ws, &sid, "skip-background", 0);
        state::save(&ws, &mut st);
        return Ok(out);
    }
    let started = std::time::Instant::now();
    let result = gate(&ws, &cfg, &mut st, &sid)?;
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Gate::Unchanged => log_gate(&ws, &sid, "unchanged", ms),
        Gate::Pass(notes) => {
            let s = st.session(&sid);
            s.failures = 0;
            s.released = false;
            out.system.extend(notes);
            log_gate(&ws, &sid, "pass", ms);
        }
        Gate::Fail(reason) => {
            let s = st.session(&sid);
            s.failures += 1;
            if s.failures >= MAX_STRIKES {
                s.released = true;
                out.context.push(format!(
                    "[harness] the gate failed {} times in a row. Stop trying to fix it now. \
                     Tell the user exactly what fails, what you tried, and what you think \
                     the cause is.\n\n{reason}",
                    s.failures
                ));
                out.system.push(format!(
                    "[harness] gate still failing after {} attempts: Claude was asked to stop \
                     and report",
                    s.failures
                ));
                log_gate(&ws, &sid, "released", ms);
            } else {
                out.block = Some(reason);
                log_gate(&ws, &sid, "block", ms);
            }
        }
    }
    if !state::save(&ws, &mut st) {
        out.system.push(
            "[harness] cannot write the harness state: the gate will run on every stop".into(),
        );
    }
    Ok(out)
}

fn pre_bash(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let Some(cmd) = input["tool_input"]["command"].as_str() else {
        return Ok(out);
    };
    if !publishes(cmd) {
        return Ok(out);
    }
    let Some((ws, cfg)) = workspace(project) else {
        return Ok(out);
    };
    let sid = session_id(input);
    let mut st = state::load(&ws);
    let started = std::time::Instant::now();
    let result = gate(&ws, &cfg, &mut st, &sid)?;
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Gate::Unchanged | Gate::Pass(_) => log_gate(&ws, &sid, "push-pass", ms),
        Gate::Fail(reason) => {
            out.deny = Some(format!(
                "fsmap verify must pass before this is pushed or opened as a PR.\n\n{reason}"
            ));
            log_gate(&ws, &sid, "push-block", ms);
        }
    }
    state::save(&ws, &mut st);
    Ok(out)
}

/// Commands that publish work: `git push`, `gh pr create|merge`.
fn publishes(cmd: &str) -> bool {
    cmd.split(['\n', ';', '&', '|']).any(|part| {
        let words: Vec<&str> = part.split_whitespace().collect();
        let git_push = words
            .iter()
            .position(|w| *w == "git" || w.ends_with("/git"))
            .is_some_and(|i| words[i + 1..].contains(&"push"));
        let gh_pr = words.windows(3).any(|w| {
            (w[0] == "gh" || w[0].ends_with("/gh"))
                && w[1] == "pr"
                && (w[2] == "create" || w[2] == "merge")
        });
        git_push || gh_pr
    })
}

fn prompt(project: &Path, input: &Value) -> Result<Out> {
    let Some((ws, _)) = workspace(project) else {
        return Ok(Out::default());
    };
    let mut st = state::load(&ws);
    let s = st.session(&session_id(input));
    s.failures = 0;
    s.released = false;
    s.last_seen = state::now();
    state::save(&ws, &mut st);
    Ok(Out::default())
}

fn log_gate(ws: &Path, sid: &str, result: &str, ms: u64) {
    state::log(
        ws,
        &json!({"t": state::now(), "event": "gate", "session": sid, "result": result, "ms": ms}),
    );
}

/// Files changed since the session started (or since HEAD), untracked ones included,
/// relative to the workspace root with forward slashes. Compared as strings: absolute
/// paths differ in form (Windows `\\?\` canonical paths vs cargo's), and a mismatch would
/// turn a new failure into a "pre-existing" one.
fn touched_files(ws: &Path, start_head: Option<&str>) -> Vec<String> {
    let Some((root, _)) = brief::locate(ws) else {
        return Vec::new();
    };
    let prefix = crate::git::run({
        let mut c = crate::git::git(ws);
        c.args(["rev-parse", "--show-prefix"]);
        c
    })
    .map(|o| String::from_utf8_lossy(&o).trim().to_string())
    .unwrap_or_default();
    brief::changed_files(&root, start_head.unwrap_or("HEAD"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.strip_prefix(&prefix).map(String::from))
        .collect()
}

/// Failing steps that block (integrity, unattributable, or pointing at a touched file) and the files
/// of failures that only concern untouched files (pre-existing, review finding M7).
fn split_failures<'a>(report: &'a Report, touched: &[String]) -> (Vec<&'a str>, Vec<String>) {
    let mut blocking = Vec::new();
    let mut preexisting = Vec::new();
    for f in report.failures() {
        // A weakening setting makes every other result meaningless, wherever it lives (a
        // git-ignored .cargo/config.toml is never "touched"); trusting it is explicit.
        let mine = f.name == "integrity"
            || f.files.is_empty()
            || f.files.iter().any(|file| touched.contains(file));
        if mine {
            blocking.push(f.name.as_str());
        } else {
            preexisting.extend(f.files.iter().map(|file| format!("{} ({})", file, f.name)));
        }
    }
    (blocking, preexisting)
}

fn failure_reason(report: &Report, blocking: &[&str]) -> String {
    let mut text = String::from(
        "fsmap verify failed. Fix the cause, then finish again. Do not add #[allow] or \
         #[expect], skip, ignore or weaken tests, or change cargo, rustfmt or toolchain \
         settings to get green. If a failure predates your change or is out of scope, say so \
         to the user instead.\n",
    );
    for f in report
        .failures()
        .filter(|f| blocking.contains(&f.name.as_str()))
    {
        text.push_str(&format!("\n## {} — `{}`\n{}\n", f.name, f.cmd, f.tail));
    }
    if text.len() > 8000 {
        let mut cut = 8000;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (run `fsmap verify` for the rest)");
    }
    text
}

/// What the change adds that weakens checks: lint suppressions, ignored or should-panic
/// tests, `cfg(any())`, and removed assertions (review finding m6). Shown to the user.
fn suppression_summary(ws: &Path, start_head: Option<&str>) -> Option<String> {
    let (root, _) = brief::locate(ws)?;
    let diff = crate::git::run({
        let mut c = crate::git::git_worktree(&root);
        c.args([
            "diff",
            "--unified=0",
            "--no-ext-diff",
            "--no-color",
            "--ignore-submodules=dirty",
            start_head.unwrap_or("HEAD"),
            "--",
            "*.rs",
        ]);
        c
    })
    .ok()?;
    let diff = String::from_utf8_lossy(&diff);
    let mut file = String::new();
    let mut hits: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for line in diff.lines() {
        if let Some(f) = line.strip_prefix("+++ b/") {
            file = f.to_string();
            continue;
        }
        let (added, text) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => (true, &line[1..]),
            Some(b'-') if !line.starts_with("---") => (false, &line[1..]),
            _ => continue,
        };
        let kind = if added {
            if text.contains("#[allow(") || text.contains("#![allow(") {
                "allow"
            } else if text.contains("#[expect(") || text.contains("#![expect(") {
                "expect"
            } else if text.contains("#[ignore") {
                "ignored test"
            } else if text.contains("#[should_panic") {
                "should_panic"
            } else if text.contains("cfg(any())") {
                "cfg(any())"
            } else {
                continue;
            }
        } else if text.contains("assert") {
            "removed assertion"
        } else {
            continue;
        };
        hits.entry(kind).or_default().push(file.clone());
    }
    if hits.is_empty() {
        return None;
    }
    let parts: Vec<String> = hits
        .iter()
        .map(|(k, files)| {
            let mut f = files.clone();
            f.sort();
            f.dedup();
            format!("{} {k} ({})", files.len(), f.join(", "))
        })
        .collect();
    Some(format!(
        "[harness] this change weakens checks — review before accepting: {}",
        parts.join("; ")
    ))
}

// ---------------------------------------------------------------- stats

/// A summary of the outcome log for the project at `dir`.
pub fn stats(dir: &Path) -> Result<String> {
    let ws = verify::workspace_root(dir)?;
    let log = state::read_log(&ws);
    let mut counts: std::collections::BTreeMap<String, (usize, Vec<u64>)> = Default::default();
    for e in &log {
        let key = match e["event"].as_str() {
            Some("gate") => format!("gate {}", e["result"].as_str().unwrap_or("?")),
            Some("post-edit") => format!(
                "post-edit {}",
                if e["ok"].as_bool().unwrap_or(false) {
                    "ok"
                } else {
                    "errors"
                }
            ),
            Some(other) => other.to_string(),
            None => continue,
        };
        let entry = counts.entry(key).or_default();
        entry.0 += 1;
        if let Some(ms) = e["ms"].as_u64() {
            entry.1.push(ms);
        }
    }
    let mut out = format!(
        "fsmap harness log: {} events in {}\n",
        log.len(),
        state::dir(&ws).display()
    );
    for (k, (n, mut ms)) in counts {
        ms.sort_unstable();
        let median = ms.get(ms.len() / 2).copied();
        out.push_str(&format!(
            "  {k:<22} {n:>5}{}\n",
            median.map_or(String::new(), |m| format!(
                "   median {:.1} s",
                m as f64 / 1000.0
            ))
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_commands() {
        assert!(publishes("git push origin main"));
        assert!(publishes("cargo test && git push -u origin x"));
        assert!(publishes("/usr/bin/git -C . push"));
        assert!(publishes("gh pr create --draft"));
        assert!(!publishes("git status; git log --oneline"));
        assert!(!publishes("echo push"));
        assert!(!publishes("gh pr view 1"));
    }

    #[test]
    fn error_lines_dedupe_and_relativise() {
        let ws = Path::new("/w");
        let out = "/w/src/a.rs:3:5: error[E0308]: mismatched types\n\
                   src\\b.rs:1:2: error[E0425]: cannot find value\n\
                   src/a.rs:9:1: warning: unused\n\
                   /w/src/a.rs:3:5: error[E0308]: mismatched types\n\
                   error: could not compile `x` (lib) due to 1 previous error\n";
        assert_eq!(
            error_lines(out, ws),
            [
                "src/a.rs:3:5: error[E0308]: mismatched types",
                "src/b.rs:1:2: error[E0425]: cannot find value"
            ]
        );
        assert_eq!(
            verify::diagnostic(r"C:\w\src\a.rs:3:5: error[E0308]: x"),
            Some((r"C:\w\src\a.rs", "error"))
        );
    }

    /// The full dependency graph is larger than proc::run keeps; reading it truncated
    /// silently produced no paths.
    #[test]
    fn dependency_sources_of_this_crate() {
        let lines = dependency_sources(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(
            lines.iter().any(|l| l.trim_start().starts_with("regex ")),
            "{lines:?}"
        );
    }

    #[test]
    fn out_renders_per_event() {
        let o = Out {
            block: Some("r".into()),
            context: vec!["c".into()],
            ..Out::default()
        };
        let v: Value = serde_json::from_str(&o.render(Event::PostEdit).unwrap()).unwrap();
        assert_eq!(v["decision"], "block");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "c");
        let o = Out {
            deny: Some("no".into()),
            ..Out::default()
        };
        let v: Value = serde_json::from_str(&o.render(Event::PreBash).unwrap()).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(Out::default().render(Event::Stop).is_none());
    }
}
