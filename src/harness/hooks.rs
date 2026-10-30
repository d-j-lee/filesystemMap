//! Claude Code hooks (decision D-15). Each reads the hook's JSON from stdin and prints at
//! most one JSON object; the exit code is always 0. A problem that keeps a hook from doing
//! its job becomes a `systemMessage` the user sees, a panic included; when the gate cannot
//! run, the stop is blocked and a push is denied (review findings M3, final review B2).
//!
//! | Hook | Event | What it does |
//! |---|---|---|
//! | `session-start` | SessionStart | records where the session started; tells Claude the toolchain, dependency sources and rules |
//! | `post-edit` | PostToolUse `Edit\|Write` | `cargo check` on the edited package; errors back to Claude; the fsmap brief |
//! | `stop` | Stop | the gate: `fsmap verify` when the tree changed; blocks the stop on new failures |
//! | `prompt` | UserPromptSubmit | the user wrote: reset the failure streak |
//! | `pre-bash` | PreToolUse `Bash` | the gate before `git push` / `gh pr create` / `gh pr merge` |
//! | `git-pre-push` | git's pre-push hook (D-16) | lets a push from Claude Code through only when the gate verified what it publishes |

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::githook::{self, Shim};
use super::verify::{self, Config, Mode, Report};
use super::{proc, shell, state};
use crate::brief;

/// Budget for the stop and pre-push gates; the settings give the hook 900 s.
const GATE_BUDGET: Duration = Duration::from_secs(600);
/// Budget for the post-edit check.
const CHECK_BUDGET: Duration = Duration::from_secs(60);
/// Consecutive blocking failures before the gate lets go and asks for a report.
const MAX_STRIKES: u32 = 3;
/// The same when every failure is in files the session did not change: one block, so
/// Claude sees the failures, then a report instead of edits to unrelated files.
const UNTOUCHED_STRIKES: u32 = 2;
/// How long a post-edit check waits for one already running in the same worktree.
const CHECK_WAIT: Duration = Duration::from_secs(30);
/// `start_head` of a session that began before the repository's first commit: its
/// changes are judged against the empty tree, not against a HEAD it created itself
/// (second review M6).
const UNBORN: &str = "unborn";
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(event, &input)));
    let failed = |what: String| failure_out(event, &input, &what);
    let out = match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => failed(format!("did not run: {e:#}")),
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            failed(format!("crashed ({what}); its check did not run"))
        }
    };
    out.render(event)
}

/// What a hook that failed or crashed prints. Publishing is the irreversible step: a
/// pre-push check that could not run denies the push (third review).
fn failure_out(event: Event, input: &Value, what: &str) -> Out {
    if event == Event::PreBash && publishing_command(input) {
        Out {
            deny: Some(format!(
                "fsmap verify must pass before this is pushed or opened as a PR.\n\n\
                 [harness] the pre-push check {what}. Run `fsmap verify`, and tell the user \
                 the hook failed."
            )),
            ..Out::default()
        }
    } else {
        Out {
            system: vec![format!("[harness] {} hook {what}", event.name())],
            ..Out::default()
        }
    }
}

/// Whether a pre-bash input's command publishes; a detector that crashes falls back to
/// whether the command mentions a push at all.
fn publishing_command(input: &Value) -> bool {
    input["tool_input"]["command"].as_str().is_some_and(|cmd| {
        std::panic::catch_unwind(|| shell::publishes(cmd))
            .unwrap_or_else(|_| shell::mentions_publish(cmd))
    })
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
    dunce::canonicalize(&p).unwrap_or(p)
}

fn session_id(input: &Value) -> String {
    input["session_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
}

/// The project as the gate sees it.
enum Ws {
    /// No Cargo.toml at or above the project: not a Rust project, nothing to gate.
    NotRust,
    Ready(PathBuf, Config),
    /// There is a Cargo.toml, but cargo cannot locate the workspace, the toolchain settings
    /// are unsafe, or the verify configuration is malformed: the gate cannot run, which
    /// must not look like a pass (final review B2).
    Broken(String),
}

fn workspace(project: &Path) -> Ws {
    match verify::workspace_root(project) {
        Ok(ws) => match verify::config(&ws) {
            Ok(cfg) => Ws::Ready(ws, cfg),
            Err(e) => Ws::Broken(format!("{e:#}")),
        },
        Err(e) if verify::nearest_manifest(project).is_some() => Ws::Broken(format!("{e:#}")),
        Err(_) => Ws::NotRust,
    }
}

fn broken_message(e: &str) -> String {
    format!("[harness] the gate cannot run, so nothing is verified until this is fixed: {e}")
}

// ---------------------------------------------------------------- session-start

/// Whether the session has no baseline in `dir`'s state yet (reading the state is cheap;
/// the snapshot of pre-existing changes is taken only then).
fn first_sight(dir: &Path, sid: &str) -> bool {
    state::load(dir)
        .sessions
        .get(sid)
        .is_none_or(|s| s.start_head.is_none())
}

fn session_start(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let source = input["source"].as_str().unwrap_or("startup");
    let sid = session_id(input);
    let (ws, cfg) = match workspace(project) {
        Ws::Ready(ws, cfg) => (ws, cfg),
        Ws::NotRust => {
            // The agent may create the Cargo project later in this session: remember where
            // the session started so that what it commits is judged as its own.
            let head = state::head(project);
            let dirty = first_sight(project, &sid).then(|| state::dirty_snapshot(project));
            let dirty = dirty.unwrap_or_default();
            state::update(project, |st| {
                let s = st.session(&sid);
                s.last_seen = state::now();
                if s.start_head.is_none() {
                    s.start_head = Some(head.unwrap_or_else(|| UNBORN.to_string()));
                    s.start_dirty = dirty;
                }
            });
            out.system.push(format!(
                "[harness] no Cargo workspace at {}: the Rust gate is inactive until there is one",
                project.display()
            ));
            return Ok(out);
        }
        Ws::Broken(e) => {
            let msg = broken_message(&e);
            out.system.push(msg.clone());
            out.context.push(msg);
            return Ok(out);
        }
    };
    let snapshot = state::load(&ws);
    let fp = state::fingerprint(&ws, &cfg.trust);
    let head = state::head(&ws);
    let dirty = first_sight(&ws, &sid).then(|| state::dirty_snapshot(&ws));
    let dirty = dirty.unwrap_or_default();
    let now = state::now();
    let others = snapshot.live_others(&sid, now);
    let saved = state::update(&ws, |st| {
        let s = st.session(&sid);
        s.last_seen = now;
        // The baseline that lets a session which changed nothing finish: only on startup,
        // never re-recorded by resume, compact or clear (review finding B3).
        if source == "startup" && s.start_fp.is_none() {
            s.start_fp.clone_from(&fp);
        }
        // Which files the session changed is judged from here; recorded the first time the
        // session is seen, whatever the source (final review M1).
        if s.start_head.is_none() {
            s.start_head = Some(head.clone().unwrap_or_else(|| UNBORN.to_string()));
            s.start_dirty = dirty;
        }
    });
    if fp.is_some() && fp != snapshot.last_pass_fp && source != "startup" {
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
    if !saved {
        out.system.push(
            "[harness] cannot write the harness state: the gate will run on every stop".into(),
        );
    }
    out.context.push(briefing(&ws, &cfg));
    let installed = githook::install(&ws, &cfg);
    // The push audit's first window opens when the session is first seen; a resume or a
    // compaction leaves an open window alone (review M3).
    if snapshot.sessions.get(&sid).is_none_or(|s| s.refs_at == 0) {
        let refs = githook::remote_refs(&ws);
        let audit = matches!(installed, Ok(Shim::Ours));
        state::update(&ws, |st| {
            let s = st.session(&sid);
            s.remote_refs = refs;
            s.refs_at = now;
            s.audit = audit;
        });
    }
    match installed {
        Ok(Shim::Ours) => out.context.push(match githook::Mode::current() {
            githook::Mode::Enforce => "[harness] a git pre-push hook checks your pushes: git \
                refuses one unless the gate passed on the files of every commit it sends. Do \
                not get around it (--no-verify, core.hooksPath, changing CLAUDECODE)."
                .to_string(),
            githook::Mode::Report => "[harness] a git pre-push hook records your pushes; one \
                the gate has not verified is reported to the user. Do not get around it \
                (--no-verify, core.hooksPath, changing CLAUDECODE)."
                .to_string(),
        }),
        Ok(other) => {
            if let Some(why) = other.off_reason() {
                out.system.push(format!(
                    "[harness] the git pre-push check is off here ({why}); pushes are judged \
                     only by reading the commands"
                ));
            }
        }
        Err(e) => out.system.push(format!(
            "[harness] could not install the git pre-push hook ({e:#}); pushes are judged \
             only by reading the commands"
        )),
    }
    let findings = super::integrity::scan(&ws, &verify::members(&ws), &cfg.trust);
    if !findings.is_empty() {
        let list: Vec<String> = findings
            .iter()
            .map(|f| format!("{} ({})", f.file, f.kind))
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
        let mut c = verify::clean_command(ws, prog);
        c.arg("-V");
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
    // hang on the network, so this reads the full output directly. Only the host's part:
    // offline, cargo fails on packages for other platforms that were never downloaded.
    let mut c = verify::clean_command(ws, "cargo");
    c.args(["metadata", "--format-version", "1", "--offline"]);
    if let Some(host) = super::integrity::host_triple(ws) {
        c.args(["--filter-platform", &host]);
    }
    let Ok(o) = c.stderr(std::process::Stdio::null()).output() else {
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
    let (ws, cfg) = match workspace(project) {
        Ws::Ready(ws, cfg) => (ws, cfg),
        Ws::NotRust => return Ok(out),
        Ws::Broken(e) => {
            out.context.push(broken_message(&e));
            return Ok(out);
        }
    };
    let Some(manifest) = package_manifest(&ws, &path) else {
        return Ok(out);
    };
    let Some(_lock) = CheckLock::acquire(&ws, CHECK_WAIT) else {
        // Another edit's check has held the lock for the whole wait: say so instead of
        // pretending this edit was checked (final review).
        out.context.push(
            "[harness] cargo check was busy with another edit; this edit's errors will show \
             at the next check or when the turn ends."
                .into(),
        );
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
    let file = dunce::canonicalize(file).ok().or_else(|| {
        Some(
            dunce::canonicalize(file.parent()?)
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

/// At most one post-edit check per worktree at a time (parallel edits would queue on
/// cargo's build lock anyway). A later edit waits for the running check, then checks the
/// tree as it is by then. Stale after two minutes.
struct CheckLock(PathBuf);

impl CheckLock {
    fn acquire(ws: &Path, wait: Duration) -> Option<CheckLock> {
        let path = state::dir(ws).join("check.lock");
        let give_up = std::time::Instant::now() + wait;
        loop {
            if let Ok(meta) = std::fs::metadata(&path)
                && meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(120))
            {
                let _ = std::fs::remove_file(&path);
            }
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .is_ok()
            {
                return Some(CheckLock(path));
            }
            if std::time::Instant::now() >= give_up {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for CheckLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------- stop / pre-bash

/// The result of running the gate for one hook call. Computed on a snapshot of the state
/// and recorded afterwards with `state::update`.
enum Gate {
    /// Nothing to do: the tree already passed, or (for a stop) the session changed nothing.
    Unchanged,
    /// Every step passed; notes for the user (added suppressions). `fp` is `None` when the
    /// tree could not be fingerprinted: the verdict is then not remembered. `verified`: the
    /// tree and context a commit of this work tree would publish (D-16).
    Pass {
        fp: Option<String>,
        notes: Vec<String>,
        verified: Option<String>,
    },
    /// Something failed. `untouched_only`: every failure is in files the session did not
    /// change, so after one block Claude reports instead of editing unrelated files.
    Fail {
        fp: Option<String>,
        reason: String,
        untouched_only: bool,
    },
}

/// Run the gate unless the tree's fingerprint already has a verdict. The gate passes only
/// when every step passes: failures in files the session did not change still fail it,
/// because a change can break code it never touched (final review M1). For a push
/// (`publishing`) only a recorded pass counts, not "unchanged since the session started"
/// (final review M2).
fn gate(
    ws: &Path,
    cfg: &Config,
    snapshot: &state::State,
    me: &state::Session,
    publishing: bool,
) -> Result<Gate> {
    // Without a fingerprint (no git, or git refusing the repository) the gate still
    // runs, every time: skipping it would fail open.
    let fp = state::fingerprint(ws, &cfg.trust);
    if let Some(fp) = fp.as_deref() {
        if snapshot.last_pass_fp.as_deref() == Some(fp)
            || (!publishing && me.start_fp.as_deref() == Some(fp))
        {
            return Ok(Gate::Unchanged);
        }
        if snapshot.last_fail_fp.as_deref() == Some(fp)
            && let Some(reason) = &snapshot.last_fail_reason
        {
            return Ok(Gate::Fail {
                fp: Some(fp.to_string()),
                reason: reason.clone(),
                untouched_only: snapshot.last_fail_untouched_only,
            });
        }
    }
    let exe = std::env::current_exe().context("locating the fsmap binary")?;
    // Taken before the steps run, like the fingerprint: what the verdict is about.
    let verified = githook::verified_key(ws, &cfg.trust);
    let steps = verify::plan(ws, Mode::Quick, &exe, cfg);
    let report = verify::run(ws, Mode::Quick, &steps, cfg, GATE_BUDGET, |_| {});
    let touched = touched_files(ws, me);
    if report.passed() {
        let notes = suppression_summary(ws, me).into_iter().collect();
        return Ok(Gate::Pass {
            fp,
            notes,
            verified,
        });
    }
    // Integrity and unattributable failures (tests, extra steps) never count as someone
    // else's.
    let untouched_only = report.failures().all(|f| {
        f.name != "integrity"
            && !f.files.is_empty()
            && f.files.iter().all(|file| !touched.contains(file))
    });
    Ok(Gate::Fail {
        fp,
        reason: failure_reason(&report, &touched),
        untouched_only,
    })
}

fn stop(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let sid = session_id(input);
    let now = state::now();
    let active = input["stop_hook_active"].as_bool().unwrap_or(false);
    let (ws, cfg) = match workspace(project) {
        Ws::Ready(ws, cfg) => (ws, cfg),
        Ws::NotRust => return Ok(out),
        Ws::Broken(e) => {
            // The gate cannot run: block like a failure, with the same three strikes.
            let reason = broken_message(&e);
            let mut released = false;
            let mut failures = 0;
            state::update(project, |st| {
                let s = st.session(&sid);
                s.last_seen = now;
                released = active && s.released;
                if !released {
                    s.failures += 1;
                    failures = s.failures;
                    s.released = failures >= MAX_STRIKES;
                }
            });
            if released {
                return Ok(out);
            }
            if failures >= MAX_STRIKES {
                out.context.push(release_text(failures, &reason));
                out.system.push(format!(
                    "[harness] gate NOT run after {failures} attempts: {e}"
                ));
            } else {
                out.block = Some(reason);
            }
            return Ok(out);
        }
    };
    let snapshot = state::load(&ws);
    let me = snapshot.sessions.get(&sid).cloned().unwrap_or_default();
    if active && me.released {
        touch(&ws, &sid, now);
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
        // The agents' work is the turn's: it goes on.
        state::update(&ws, |st| {
            let s = st.session(&sid);
            s.last_seen = now;
            s.in_turn = true;
        });
        return Ok(out);
    }
    if !snapshot.pending_notes.is_empty() {
        state::update(&ws, |st| out.system.append(&mut st.pending_notes));
    }
    let unchecked = push_audit(&ws, &cfg, &sid, &me);
    if !unchecked.is_empty() {
        let text = audit_text(&unchecked);
        out.system.push(text.clone());
        state::update(&ws, |st| {
            st.session(&sid).pending_context.push(format!(
                "{text}\nTell the user whether these need attention. Do not push around the \
                 check."
            ));
        });
    }
    let started = std::time::Instant::now();
    let verdict = gate(&ws, &cfg, &snapshot, &me, false)?;
    let ms = started.elapsed().as_millis() as u64;
    let saved = match verdict {
        Gate::Unchanged => {
            log_gate(&ws, &sid, "unchanged", ms);
            touch(&ws, &sid, now)
        }
        Gate::Pass {
            fp,
            notes,
            verified,
        } => {
            out.system.extend(notes);
            log_gate(&ws, &sid, "pass", ms);
            record_verified(&ws, verified);
            state::update(&ws, |st| {
                record_pass(st, fp);
                let s = st.session(&sid);
                s.failures = 0;
                s.released = false;
                s.last_seen = now;
            })
        }
        Gate::Fail {
            fp,
            reason,
            untouched_only,
        } => {
            let limit = if untouched_only {
                UNTOUCHED_STRIKES
            } else {
                MAX_STRIKES
            };
            let mut failures = 0;
            let saved = state::update(&ws, |st| {
                record_fail(st, fp, &reason, untouched_only);
                let s = st.session(&sid);
                s.last_seen = now;
                s.failures += 1;
                failures = s.failures;
                s.released = failures >= limit;
            });
            if failures >= limit {
                if untouched_only {
                    out.context.push(format!(
                        "[harness] the gate still fails, only in files this session did not \
                         change. Do not edit unrelated files to get green. Tell the user what \
                         fails and whether your change could have caused it.\n\n{reason}"
                    ));
                    out.system.push(
                        "[harness] gate NOT passed: failures remain in files this session did \
                         not change; Claude was asked to report them (a push stays blocked)"
                            .into(),
                    );
                } else {
                    out.context.push(release_text(failures, &reason));
                    out.system.push(format!(
                        "[harness] gate still failing after {failures} attempts: Claude was \
                         asked to stop and report"
                    ));
                }
                log_gate(&ws, &sid, "released", ms);
            } else {
                out.block = Some(reason);
                log_gate(&ws, &sid, "block", ms);
            }
            saved
        }
    };
    // A block makes the turn go on; otherwise it ends here, and pushes until the next prompt
    // are a person's.
    let goes_on = out.block.is_some();
    state::update(&ws, |st| st.session(&sid).in_turn = goes_on);
    if !saved {
        out.system.push(
            "[harness] cannot write the harness state: the gate will run on every stop".into(),
        );
    }
    Ok(out)
}

fn release_text(failures: u32, reason: &str) -> String {
    format!(
        "[harness] the gate failed {failures} times in a row. Stop trying to fix it now. Tell \
         the user exactly what fails, what you tried, and what you think the cause is.\n\n\
         {reason}"
    )
}

fn record_pass(st: &mut state::State, fp: Option<String>) {
    if fp.is_some() {
        st.last_pass_fp = fp;
        st.last_fail_fp = None;
        st.last_fail_reason = None;
        st.last_fail_untouched_only = false;
    }
}

/// Remember the tree a pass was about, for the git pre-push hook (D-16).
fn record_verified(ws: &Path, verified: Option<String>) {
    if let Some(key) = verified {
        state::update_pushes(ws, |p| state::remember(&mut p.verified, key));
    }
}

fn record_fail(st: &mut state::State, fp: Option<String>, reason: &str, untouched_only: bool) {
    if fp.is_some() {
        st.last_fail_fp = fp;
        st.last_fail_reason = Some(reason.to_string());
        st.last_fail_untouched_only = untouched_only;
    }
}

/// Note that the session was active; returns whether the state could be written.
fn touch(ws: &Path, sid: &str, now: i64) -> bool {
    state::update(ws, |st| st.session(sid).last_seen = now)
}

fn pre_bash(project: &Path, input: &Value) -> Result<Out> {
    let mut out = Out::default();
    let Some(cmd) = input["tool_input"]["command"].as_str() else {
        return Ok(out);
    };
    let bypass = shell::hook_bypass(cmd);
    let publishing = shell::publishes(cmd);
    if bypass.is_none() && !publishing {
        return Ok(out);
    }
    let deny = |why: String| Out {
        deny: Some(format!(
            "fsmap verify must pass before this is pushed or opened as a PR.\n\n{why}"
        )),
        ..Out::default()
    };
    let (ws, cfg) = match workspace(project) {
        Ws::Ready(ws, cfg) => (ws, cfg),
        Ws::NotRust => return Ok(out),
        Ws::Broken(e) if publishing => return Ok(deny(broken_message(&e))),
        Ws::Broken(_) => return Ok(out),
    };
    let sid = session_id(input);
    let shim = githook::shim_state(&ws, &cfg);
    if let Some(why) = bypass
        && shim == Shim::Ours
    {
        log_gate(&ws, &sid, "push-bypass", 0);
        return Ok(Out {
            deny: Some(format!(
                "[harness] refused: {why}. Run the command without that; if the check is \
                 wrong, tell the user."
            )),
            ..Out::default()
        });
    }
    if !publishing {
        return Ok(out);
    }
    // With the git hook enforcing, git judges what a push actually sends, so a commit made
    // in the same command (`git commit … && git push`) is fine and a command that only
    // mentions a push is not refused (D-16 review M4). A pull request has no git hook, and
    // in a cloud session the hook only reports (R1): there this hook still decides.
    let hook_decides = shim == Shim::Ours
        && githook::Mode::current() == githook::Mode::Enforce
        && !shell::opens_pr(cmd);
    if !hook_decides {
        let pending = match uncommitted_inputs(&ws) {
            Ok(p) => p,
            Err(e) => {
                log_gate(&ws, &sid, "push-error", 0);
                return Ok(deny(format!(
                    "[harness] could not list uncommitted build inputs: {e:#}"
                )));
            }
        };
        if !pending.is_empty() {
            log_gate(&ws, &sid, "push-uncommitted", 0);
            return Ok(deny(format!(
                "[harness] the gate checks the working tree, but these build inputs are not \
                 committed, so the push would send something the gate did not check: {}. \
                 Commit or stash them, then push.",
                pending.join(", ")
            )));
        }
    }
    let snapshot = state::load(&ws);
    let me = snapshot.sessions.get(&sid).cloned().unwrap_or_default();
    let started = std::time::Instant::now();
    // Publishing is the irreversible step: a gate that cannot run denies it.
    let verdict = match gate(&ws, &cfg, &snapshot, &me, true) {
        Ok(v) => v,
        Err(e) if hook_decides => {
            log_gate(&ws, &sid, "push-error", 0);
            out.context.push(format!(
                "[harness] the gate could not run ({e:#}); the git pre-push hook will refuse \
                 a push it has not verified."
            ));
            return Ok(out);
        }
        Err(e) => {
            log_gate(&ws, &sid, "push-error", 0);
            return Ok(deny(format!("[harness] the gate could not run: {e:#}")));
        }
    };
    let ms = started.elapsed().as_millis() as u64;
    match verdict {
        Gate::Unchanged => log_gate(&ws, &sid, "push-pass", ms),
        Gate::Pass {
            fp,
            notes,
            verified,
        } => {
            log_gate(&ws, &sid, "push-pass", ms);
            record_verified(&ws, verified);
            // Shown at the next Stop, which will find this tree already passed (review M3).
            state::update(&ws, |st| {
                record_pass(st, fp);
                for n in notes {
                    if !st.pending_notes.contains(&n) {
                        st.pending_notes.push(n);
                    }
                }
            });
        }
        Gate::Fail {
            fp,
            reason,
            untouched_only,
        } => {
            log_gate(&ws, &sid, "push-block", ms);
            state::update(&ws, |st| record_fail(st, fp, &reason, untouched_only));
            if hook_decides {
                out.context.push(format!(
                    "[harness] the gate fails on this tree, so the git pre-push hook will \
                     refuse a push of it:\n\n{reason}"
                ));
            } else {
                out = deny(reason);
            }
        }
    }
    Ok(out)
}

/// `fsmap hook git-pre-push`: the body of git's pre-push hook for a push from Claude Code
/// (D-16). `workspace` is the gated workspace relative to the pushing work tree's top (git
/// runs hooks there), `remote` git's first argument, `stdin` its ref updates. Returns
/// whether git may push and what to print on stderr. Anything that keeps the check from
/// running counts as unverified: publishing is the irreversible step.
pub fn git_pre_push(workspace: &str, remote: &str, stdin: &str) -> (bool, String) {
    // A person's push is not the harness's business (the shim checks this first).
    if std::env::var("CLAUDECODE").as_deref() != Ok("1") {
        return (true, String::new());
    }
    let mode = githook::Mode::current();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_push(workspace, remote, stdin, mode)
    }));
    let problems = match checked {
        Ok(Ok(problems)) => problems,
        Ok(Err(e)) => vec![format!("the check could not run: {e:#}")],
        Err(_) => vec!["the check crashed".to_string()],
    };
    if problems.is_empty() {
        return (true, String::new());
    }
    let list = problems.join("\n  ");
    match mode {
        githook::Mode::Enforce => (
            false,
            format!(
                "[harness] push refused, the gate has not verified what it sends:\n  {list}\n\
                 The gate runs when you end your turn and before a push command; push again \
                 once it has passed. Do not bypass this check; if it is wrong, tell the user."
            ),
        ),
        githook::Mode::Report => (
            true,
            format!(
                "[harness] this push was not verified by the gate:\n  {list}\nIt is reported \
                 to the user."
            ),
        ),
    }
}

/// The problems with a push; none when every commit it publishes is verified.
fn check_push(
    workspace_rel: &str,
    remote: &str,
    stdin: &str,
    mode: githook::Mode,
) -> Result<Vec<String>> {
    let updates = githook::parse_updates(stdin)?;
    let top = std::env::current_dir().context("the pushing work tree")?;
    let project = dunce::canonicalize(top.join(workspace_rel))
        .with_context(|| format!("the gated workspace {workspace_rel}"))?;
    let (ws, cfg) = match workspace(&project) {
        Ws::Ready(ws, cfg) => (ws, cfg),
        // No workspace in this checkout (an orphan `gh-pages`): a pushed commit that has one
        // was not checked here (review m3).
        Ws::NotRust => {
            let manifest = match workspace_rel.trim_matches('/') {
                "" | "." => "Cargo.toml".to_string(),
                rel => format!("{rel}/Cargo.toml"),
            };
            return Ok(updates
                .iter()
                .filter(|u| !u.local.is_zero())
                .filter(|u| githook::commit_has(&top, &u.local, &manifest))
                .map(|u| {
                    format!(
                        "{} ({}): this checkout has no Cargo workspace at {workspace_rel}, so \
                         the gate cannot have checked it; push it from a checkout that has",
                        u.remote_ref,
                        u.local.short()
                    )
                })
                .collect());
        }
        Ws::Broken(e) => return Ok(vec![broken_message(&e)]),
    };
    let pushes = state::load_pushes(&ws);
    let context = std::cell::OnceCell::new();
    let head = state::head(&ws);
    let head_ok = std::cell::OnceCell::new();
    let mut verified = Vec::new();
    let mut problems = Vec::new();
    for u in updates.iter().filter(|u| !u.local.is_zero()) {
        let Some(commit) = githook::commit_of(&ws, &u.local) else {
            problems.push((
                u.local.clone(),
                format!("{} ({}) is not a commit", u.remote_ref, u.local.short()),
            ));
            continue;
        };
        if githook::publishes_nothing_new(&ws, &commit, remote) {
            // Approved too, or the audit would take a new branch at a known commit for a
            // push around the hook (review m1).
            verified.push(commit);
            continue;
        }
        let key = githook::tree_of(&ws, &commit).map(|t| {
            format!(
                "{t} {}",
                context.get_or_init(|| state::context_digest(&ws, &cfg.trust))
            )
        });
        let tree_verified = key.is_some_and(|k| pushes.verified.contains(&k));
        // Where no tree was recorded: HEAD of a work tree whose fingerprint passed, with no
        // tracked change left out of the commit (a data file `include_str!` reads is not a
        // build input; review M1) and no build input uncommitted.
        let head_verified = || {
            head.as_deref() == Some(commit.as_str())
                && *head_ok.get_or_init(|| {
                    tracked_clean(&ws)
                        && uncommitted_inputs(&ws).is_ok_and(|p| p.is_empty())
                        && state::fingerprint(&ws, &cfg.trust).is_some_and(|fp| {
                            state::load(&ws).last_pass_fp.as_deref() == Some(fp.as_str())
                        })
                })
        };
        if tree_verified || head_verified() {
            verified.push(commit);
        } else {
            problems.push((
                commit.clone(),
                format!(
                    "{} → {} ({}): the gate has not passed on this commit's files",
                    u.local_ref,
                    u.remote_ref,
                    commit.short()
                ),
            ));
        }
    }
    state::update_pushes(&ws, |p| {
        for c in &verified {
            state::remember(&mut p.approved, c.as_str().to_string());
        }
        if mode == githook::Mode::Report {
            for (c, why) in &problems {
                state::remember(&mut p.unverified, (c.as_str().to_string(), why.clone()));
            }
        }
    });
    state::log(
        &ws,
        &json!({"t": state::now(), "event": "git-pre-push", "updates": updates.len(),
                "unverified": problems.len(),
                "mode": if mode == githook::Mode::Enforce { "enforce" } else { "report" }}),
    );
    Ok(problems.into_iter().map(|(_, why)| why).collect())
}

/// Whether the work tree has no change to a tracked file (submodules included).
fn tracked_clean(ws: &Path) -> bool {
    crate::git::run({
        let mut c = crate::git::git_worktree(ws);
        c.args(["status", "--porcelain=v2", "-z", "--untracked-files=no"]);
        c
    })
    .is_ok_and(|out| out.is_empty())
}

/// Pushes since the last audit that no pre-push run verified (D-16): pushed with
/// `--no-verify` or a hooksPath override, from outside Claude Code, or let through in report
/// mode, read from the reflogs of the remote-tracking refs that moved, each reported once.
/// Whether to look is decided by the hook's state when the window opened, so removing the
/// hook does not also remove the audit (review M4); its removal is reported. Then the window
/// moves to now: only an audit moves it (review M3).
fn push_audit(ws: &Path, cfg: &Config, sid: &str, me: &state::Session) -> Vec<String> {
    let refs = githook::remote_refs(ws);
    let shim = githook::shim_state(ws, cfg);
    let mut lines = Vec::new();
    if me.refs_at > 0 && me.audit {
        let pushes = state::load_pushes(ws);
        let mut found: Vec<(String, String, String)> = Vec::new();
        for (name, oid) in githook::pushed_since(ws, &me.remote_refs, &refs) {
            if pushes.approved.contains(&oid) || pushes.reported.contains(&oid) {
                continue;
            }
            let why = pushes
                .unverified
                .iter()
                .find(|(o, _)| *o == oid)
                .map_or_else(
                    || {
                        "the pre-push check did not run (--no-verify, a hooksPath override, \
                         a removed hook, or a push from outside Claude Code)"
                            .to_string()
                    },
                    |(_, w)| w.clone(),
                );
            found.push((name, oid, why));
        }
        if !found.is_empty() {
            state::update_pushes(ws, |p| {
                for (_, oid, _) in &found {
                    state::remember(&mut p.reported, oid.clone());
                }
            });
        }
        lines.extend(found.into_iter().map(|(name, oid, why)| {
            let short = name.strip_prefix("refs/remotes/").unwrap_or(&name);
            format!("  {short} → {}: {why}", &oid[..oid.len().min(10)])
        }));
        if let Some(why) = shim.off_reason() {
            lines.push(format!(
                "  the git pre-push hook stopped working during the session: {why}"
            ));
        }
    }
    let at = state::now();
    let audit = shim == Shim::Ours;
    state::update(ws, |st| {
        let s = st.session(sid);
        s.remote_refs = refs;
        s.refs_at = at;
        s.audit = audit;
    });
    lines
}

fn audit_text(lines: &[String]) -> String {
    format!(
        "[harness] pushes the gate did not verify:\n{}",
        lines.join("\n")
    )
}

fn prompt(project: &Path, input: &Value) -> Result<Out> {
    let sid = session_id(input);
    let Ws::Ready(ws, cfg) = workspace(project) else {
        // A broken project keeps its strikes in the project's state directory.
        state::update(project, |st| reset(st, &sid));
        return Ok(Out::default());
    };
    let mut out = Out::default();
    // Pushes since the last audit, when a turn no Stop ended (one the user interrupted) left
    // them in the window. After a turn ended they are a person's: the window only moves.
    let me = state::load(&ws)
        .sessions
        .get(&sid)
        .cloned()
        .unwrap_or_default();
    let look = state::Session {
        audit: me.audit && me.in_turn,
        ..me
    };
    let unchecked = push_audit(&ws, &cfg, &sid, &look);
    if !unchecked.is_empty() {
        let text = audit_text(&unchecked);
        out.system.push(text.clone());
        out.context.push(format!(
            "{text}\nTell the user whether these need attention. Do not push around the check."
        ));
    }
    state::update(&ws, |st| {
        reset(st, &sid);
        let s = st.session(&sid);
        s.in_turn = true;
        out.context.append(&mut s.pending_context);
    });
    Ok(out)
}

fn reset(st: &mut state::State, sid: &str) {
    let s = st.session(sid);
    s.failures = 0;
    s.released = false;
    s.last_seen = state::now();
}

fn log_gate(ws: &Path, sid: &str, result: &str, ms: u64) {
    state::log(
        ws,
        &json!({"t": state::now(), "event": "gate", "session": sid, "result": result, "ms": ms}),
    );
}

/// Files changed since the session started (or since HEAD), untracked ones included,
/// minus those already changed then and untouched since, relative to the workspace root
/// with forward slashes. Compared as strings: absolute paths differ in form (Windows
/// `\\?\` canonical paths vs cargo's), and a mismatch would turn a new failure into a
/// "pre-existing" one.
fn touched_files(ws: &Path, me: &state::Session) -> Vec<String> {
    let Some(root) = toplevel(ws) else {
        return Vec::new();
    };
    let prefix = crate::git::run({
        let mut c = crate::git::git(ws);
        c.args(["rev-parse", "--show-prefix"]);
        c
    })
    .map(|o| String::from_utf8_lossy(&o).trim().to_string())
    .unwrap_or_default();
    brief::changed_files(&root, &diff_base(ws, me.start_head.as_deref()))
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !preexisting(&root, me, f))
        .filter_map(|f| f.strip_prefix(&prefix).map(String::from))
        .collect()
}

/// `rel` (relative to the work tree top at `root`) was already changed when the session
/// started and has not changed since.
fn preexisting(root: &Path, me: &state::Session, rel: &str) -> bool {
    me.start_dirty
        .get(rel)
        .is_some_and(|h| *h == state::content_hash(&root.join(rel)))
}

/// The work tree's top directory (works before the first commit, unlike `brief::locate`).
fn toplevel(ws: &Path) -> Option<PathBuf> {
    crate::git::run({
        let mut c = crate::git::git(ws);
        c.args(["rev-parse", "--show-toplevel"]);
        c
    })
    .ok()
    .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
}

/// What the session's changes are compared with: where it started, the empty tree when
/// it started before the first commit, else HEAD.
fn diff_base(ws: &Path, start_head: Option<&str>) -> String {
    match start_head {
        Some(UNBORN) => crate::git::run({
            let mut c = crate::git::git(ws);
            // stdin is empty: the id of the empty tree, whatever the hash function.
            c.args(["hash-object", "-t", "tree", "--stdin"]);
            c
        })
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_else(|_| "HEAD".into()),
        Some(h) => h.to_string(),
        None => "HEAD".into(),
    }
}

/// Changed or untracked files that can change what the gate checks, relative to the work
/// tree top: manifests, the lock file, cargo, rustfmt, clippy and toolchain configuration,
/// and Rust sources. A push sends HEAD; with these pending, the gate's verdict on the work
/// tree says nothing about HEAD (second review M7). Every change to a tracked `.rs` file
/// counts (renames, deletions, `--skip-worktree` included); an untracked one only where a
/// target reads it, so a scratch file does not (third review, finding 3; fourth, M2).
fn uncommitted_inputs(ws: &Path) -> Result<Vec<String>> {
    let top = toplevel(ws).context("finding the work tree")?;
    let top = dunce::canonicalize(&top).unwrap_or(top);
    let status = crate::git::run({
        let mut c = crate::git::git_worktree(ws);
        c.args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
        ]);
        c
    })
    .context("reading git status")?;
    let sources = verify::source_dirs(ws);
    let read_by_a_target = |rel: &str| {
        let path = top.join(rel);
        sources.as_ref().is_none_or(|dirs| {
            dirs.iter().any(|(d, recursive)| {
                if *recursive {
                    path.starts_with(d)
                } else {
                    path.parent() == Some(d.as_path())
                }
            })
        })
    };
    // Cargo configuration under any name (an `include`d file, a link's target) is a build
    // input too (sixth review).
    let configs: Vec<String> = super::integrity::config_files(ws)
        .into_iter()
        .filter_map(|p| {
            let p = dunce::canonicalize(&p).ok()?;
            Some(
                p.strip_prefix(&top)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        })
        .collect();
    let mut entries: Vec<(String, bool)> = state::status_entries(&status)
        .into_iter()
        .map(|e| (e.path, e.untracked))
        .collect();
    entries.extend(
        state::hidden_index_entries(ws)
            .into_iter()
            .map(|p| (p, false)),
    );
    let mut out: Vec<String> = entries
        .into_iter()
        .filter(|(p, untracked)| {
            let name = p.rsplit('/').next().unwrap_or(p);
            matches!(
                name,
                "Cargo.toml"
                    | "Cargo.lock"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | "rustfmt.toml"
                    | ".rustfmt.toml"
                    | "clippy.toml"
                    | ".clippy.toml"
            ) || configs.contains(p)
                || p.ends_with(".cargo/config")
                || p.ends_with(".cargo/config.toml")
                || (p.ends_with(".rs") && (!*untracked || read_by_a_target(p)))
        })
        .map(|(p, _)| p)
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// A manifest line that switches checks or tests off (second review M4): manifests are
/// tracked, so this is reported on a pass rather than failed.
fn manifest_weakening(line: &str) -> Option<&'static str> {
    static RE: std::sync::LazyLock<[(regex::Regex, &'static str); 4]> =
        std::sync::LazyLock::new(|| {
            let re = |p: &str| regex::Regex::new(p).expect("a valid pattern");
            [
                (
                    re(r#"=\s*\{?[^#]*['"]allow['"]"#),
                    "lint allowed in a manifest",
                ),
                (
                    re(r"^\s*(debug-assertions|overflow-checks)\s*=\s*false"),
                    "checks switched off in a manifest",
                ),
                (
                    re(r"^\s*(test|autotests|harness|doctest)\s*=\s*false"),
                    "tests switched off in a manifest",
                ),
                (re(r"^\s*\[patch"), "dependency patched in a manifest"),
            ]
        });
    RE.iter().find(|(re, _)| re.is_match(line)).map(|(_, k)| *k)
}

/// The block reason: every failing step with the end of its output, and, where the
/// failure names files, whether this session changed them.
fn failure_reason(report: &Report, touched: &[String]) -> String {
    let mut text = String::from(
        "fsmap verify failed. Fix the cause, then finish again. Do not add #[allow] or \
         #[expect], skip, ignore or weaken tests, or change cargo, rustfmt or toolchain \
         settings to get green. If a failure is in a file you did not change and your change \
         did not cause it, tell the user instead of editing unrelated files.\n",
    );
    for f in report.failures() {
        text.push_str(&format!("\n## {} — `{}`\n", f.name, f.cmd));
        if !f.files.is_empty() {
            let files: Vec<String> = f
                .files
                .iter()
                .map(|file| {
                    let state = if touched.contains(file) {
                        "changed in this session"
                    } else {
                        "not changed in this session"
                    };
                    format!("{file} ({state})")
                })
                .collect();
            text.push_str(&format!("Files: {}\n", files.join(", ")));
        }
        text.push_str(&f.tail);
        text.push('\n');
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

/// What the change adds that weakens checks, shown to the user on a pass (review finding
/// m6): lint suppressions (`cfg_attr` forms included), `rustfmt::skip`, ignored or
/// should-panic tests, `cfg(any())`, removed assertions, lints allowed in a manifest, and
/// changed lint, format, cargo or toolchain configuration (tracked configuration is not
/// judged, so this is where it shows). New untracked files count as added lines; files
/// already changed when the session started and untouched since do not count.
fn suppression_summary(ws: &Path, me: &state::Session) -> Option<String> {
    let root = toplevel(ws)?;
    let base = diff_base(ws, me.start_head.as_deref());
    let mut pathspecs: Vec<String> = [
        "*.rs",
        ":(glob)**/Cargo.toml",
        ":(glob)**/clippy.toml",
        ":(glob)**/.clippy.toml",
        ":(glob)**/rustfmt.toml",
        ":(glob)**/.rustfmt.toml",
        ":(glob)**/.cargo/config",
        ":(glob)**/.cargo/config.toml",
        ":(glob)**/rust-toolchain",
        ":(glob)**/rust-toolchain.toml",
    ]
    .map(String::from)
    .into();
    // Files a cargo configuration includes, under any name (fifth review).
    let top = dunce::canonicalize(&root).unwrap_or_else(|_| root.clone());
    for path in super::integrity::config_files(ws) {
        let path = dunce::canonicalize(&path).unwrap_or(path);
        if let Ok(rel) = path.strip_prefix(&top) {
            pathspecs.push(format!(
                ":(literal){}",
                rel.to_string_lossy().replace('\\', "/")
            ));
        }
    }
    let diff = crate::git::run({
        let mut c = crate::git::git_worktree(&root);
        c.args([
            "diff",
            "--unified=0",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--ignore-submodules=dirty",
            &base,
            "--",
        ])
        .args(&pathspecs);
        c
    })
    .ok()?;
    let mut lines: Vec<(String, char, String)> = Vec::new();
    let mut file = String::new();
    for line in String::from_utf8_lossy(&diff).lines() {
        if let Some(f) = line.strip_prefix("+++ b/") {
            file = f.to_string();
        } else if line.starts_with("+++") || line.starts_with("---") {
            continue;
        } else if let Some(t) = line.strip_prefix('+') {
            lines.push((file.clone(), '+', t.to_string()));
        } else if let Some(t) = line.strip_prefix('-') {
            lines.push((file.clone(), '-', t.to_string()));
        }
    }
    // Untracked files are not in the diff; all their lines are new.
    let untracked = crate::git::run({
        let mut c = crate::git::git(&root);
        c.args(["ls-files", "-z", "--others", "--exclude-standard", "--"])
            .args(&pathspecs);
        c
    })
    .unwrap_or_default();
    for f in untracked.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let rel = String::from_utf8_lossy(f).into_owned();
        if let Ok(text) = std::fs::read_to_string(root.join(&rel)) {
            lines.extend(text.lines().map(|l| (rel.clone(), '+', l.to_string())));
        }
    }
    // Hits in files already changed when the session started (and untouched since) are
    // reported apart: still reported, since nothing else will (fourth review, M3).
    let mut hits: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    let mut pending: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for (file, sign, text) in &lines {
        let name = file.rsplit('/').next().unwrap_or(file);
        let kind = if name.contains("clippy") || name.contains("rustfmt") {
            "lint or format configuration changed"
        } else if !file.ends_with(".rs") && name != "Cargo.toml" {
            "build configuration changed"
        } else if name == "Cargo.toml" {
            match manifest_weakening(text) {
                Some(kind) if *sign == '+' => kind,
                _ => continue,
            }
        } else if *sign == '+' {
            let attr = text.contains("#[") || text.contains("#![");
            if attr && text.contains("cfg_attr") && text.contains("ignore") {
                "ignored test"
            } else if attr && (text.contains("allow(") && !text.contains("expect(")) {
                "allow"
            } else if attr && text.contains("expect(") {
                "expect"
            } else if text.contains("rustfmt::skip") {
                "rustfmt::skip"
            } else if text.contains("#[ignore") {
                "ignored test"
            } else if text.contains("#[should_panic") {
                "should_panic"
            } else if text.contains("cfg(any())") || text.contains("cfg(false)") {
                "code compiled out"
            } else {
                continue;
            }
        } else if text.contains("assert") {
            "removed assertion"
        } else {
            continue;
        };
        let bucket = if preexisting(&root, me, file) {
            &mut pending
        } else {
            &mut hits
        };
        bucket.entry(kind).or_default().push(file.clone());
    }
    if hits.is_empty() && pending.is_empty() {
        return None;
    }
    let list = |hits: &std::collections::BTreeMap<&str, Vec<String>>| {
        hits.iter()
            .map(|(k, files)| {
                let mut f = files.clone();
                f.sort();
                f.dedup();
                format!("{} {k} ({})", files.len(), f.join(", "))
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut text = String::from("[harness] this change weakens checks — review before accepting:");
    if !hits.is_empty() {
        text.push(' ');
        text.push_str(&list(&hits));
    }
    if !pending.is_empty() {
        text.push_str(if hits.is_empty() { " " } else { ". " });
        text.push_str("Files already changed when this session started: ");
        text.push_str(&list(&pending));
    }
    Some(text)
}

// ---------------------------------------------------------------- stats

/// A summary of the outcome log for the project at `dir`.
pub fn stats(dir: &Path) -> Result<String> {
    // The log lives with the git directory; cargo is not needed to read it.
    let ws = verify::workspace_root(dir).unwrap_or_else(|_| dir.to_path_buf());
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
    fn a_failed_pre_push_check_denies_only_publishing_commands() {
        let input = |cmd: &str| json!({"tool_input": {"command": cmd}});
        let push = failure_out(
            Event::PreBash,
            &input("git push origin main"),
            "crashed (x)",
        );
        assert!(push.deny.is_some_and(|d| d.contains("crashed (x)")));
        let other = failure_out(Event::PreBash, &input("cargo test"), "crashed (x)");
        assert!(other.deny.is_none() && !other.system.is_empty());
        let stop = failure_out(Event::Stop, &input("git push"), "crashed (x)");
        assert!(stop.deny.is_none());
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
