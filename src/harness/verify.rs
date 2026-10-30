//! `fsmap verify`: the single gate shared by people, agent hooks and CI (decision D-15).
//!
//! Steps for a Cargo workspace: `integrity` (configuration that would make the other steps
//! pass without the code changing), fmt, clippy `-D warnings`, tests, and cargo-deny in
//! full mode. A repository adds its own checks and trusted settings in `Cargo.toml`:
//!
//! ```toml
//! [workspace.metadata.fsmap.verify]   # or [package.metadata.fsmap.verify]
//! extra = [["node", "--check", "viewer/app.js"], ["{fsmap}", "insights", ".", "--fail-on", "high", "-q"]]
//! trust = ["rustc-wrapper", "env:RUSTC_WRAPPER"]   # kinds the integrity step accepts
//! ```
//!
//! `{fsmap}` is replaced by the running fsmap binary. Every step runs even after a failure,
//! so one round reports everything that is wrong. Steps run without the environment
//! variables that weaken them and with the build directory inside the repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{integrity, proc};

/// How much of a failing step's output is kept.
const TAIL_LINES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// integrity, fmt, clippy, tests and the repository's extra steps (the stop gate).
    Quick,
    /// Quick plus `cargo deny check`.
    Full,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    /// How people read it: `ok`, `FAIL`, `skip`.
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "ok",
            Status::Fail => "FAIL",
            Status::Skip => "skip",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StepResult {
    pub name: String,
    pub cmd: String,
    pub status: Status,
    pub ms: u64,
    /// Last lines of output for a failure, or the reason for a skip. Control characters
    /// are removed: this text reaches an agent as part of a hook message.
    pub tail: String,
    /// Files the failure points at (fmt, clippy, integrity), relative to the root. Empty
    /// when the output cannot be attributed (tests, extra steps).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub root: String,
    pub mode: &'static str,
    pub steps: Vec<StepResult>,
    /// Environment variables removed before running the steps.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_env: Vec<String>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.steps.iter().all(|s| s.status != Status::Fail)
    }

    pub fn failures(&self) -> impl Iterator<Item = &StepResult> {
        self.steps.iter().filter(|s| s.status == Status::Fail)
    }
}

/// `[*.metadata.fsmap.verify]` of the workspace.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub extra: Vec<Vec<String>>,
    pub trust: Vec<String>,
}

/// The Cargo workspace root containing `dir`. Toolchain files are checked first: rustup
/// obeys them as soon as cargo runs, so a `path` override would run its own `cargo`
/// before any other check (final review B1).
pub fn workspace_root(dir: &Path) -> Result<PathBuf> {
    let trust = nearest_manifest(dir)
        .and_then(|m| config_of(&m).ok())
        .map(|c| c.trust)
        .unwrap_or_default();
    let unsafe_toolchain = integrity::toolchain_scan(dir, &trust);
    if !unsafe_toolchain.is_empty() {
        let list: Vec<String> = unsafe_toolchain
            .iter()
            .map(|f| format!("{}: {} ({})", f.file, f.text, f.kind))
            .collect();
        let remedy = if unsafe_toolchain.iter().all(|f| f.trustable) {
            "Remove them, or list the kind under [*.metadata.fsmap.verify] trust if this \
             machine needs them"
        } else {
            "A toolchain file git does not track is what CI never sees: remove it, and leave \
             committing one to the user"
        };
        bail!(
            "refusing to run cargo: the toolchain settings would run binaries of the \
             repository's choosing: {}. {remedy}",
            list.join("; ")
        );
    }
    let out = clean_command(dir, "cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .stderr(Stdio::piped())
        .output()
        .context("failed to run cargo (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "no Cargo project at {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    manifest
        .parent()
        .map(Path::to_path_buf)
        .context("manifest path has no parent")
}

/// The directories of the workspace's member packages (`cargo metadata --no-deps`), or the
/// root alone when cargo cannot say.
pub fn members(root: &Path) -> Vec<PathBuf> {
    let found: Vec<PathBuf> = metadata(root)
        .and_then(|meta| {
            meta["packages"].as_array().map(|ps| {
                ps.iter()
                    .filter_map(|p| {
                        Some(
                            Path::new(p["manifest_path"].as_str()?)
                                .parent()?
                                .to_path_buf(),
                        )
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    if found.is_empty() {
        vec![root.to_path_buf()]
    } else {
        found
    }
}

/// Where the workspace's targets read Rust sources from, canonical: each target's root
/// file's directory with everything below it (`src/`, `tests/`, `examples/`), and for a
/// build script only the files beside it (`true` marks the recursive ones). `None` when
/// cargo cannot tell.
pub fn source_dirs(root: &Path) -> Option<Vec<(PathBuf, bool)>> {
    let meta = metadata(root)?;
    let mut out = Vec::new();
    for t in meta["packages"]
        .as_array()?
        .iter()
        .filter_map(|p| p["targets"].as_array())
        .flatten()
    {
        let src = Path::new(t["src_path"].as_str()?);
        let dir = dunce::canonicalize(src.parent()?).ok()?;
        let build_script = t["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|k| k == "custom-build"));
        out.push((dir, !build_script));
    }
    Some(out)
}

fn metadata(root: &Path) -> Option<serde_json::Value> {
    clean_command(root, "cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
}

/// The nearest `Cargo.toml` at or above `dir`.
pub fn nearest_manifest(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .map(|d| d.join("Cargo.toml"))
        .find(|m| m.is_file())
}

/// The workspace's verify configuration, from `[workspace.metadata.fsmap.verify]` or
/// `[package.metadata.fsmap.verify]` of the root manifest.
pub fn config(root: &Path) -> Result<Config> {
    config_of(&root.join("Cargo.toml"))
}

/// Read a manifest's verify table directly, so that no cargo runs before the toolchain
/// check and a malformed entry is an error rather than silently dropped (final review B2).
fn config_of(manifest: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let doc: toml::Table = text
        .parse()
        .with_context(|| format!("parsing {}", manifest.display()))?;
    let table = ["workspace", "package"]
        .iter()
        .filter_map(|k| doc.get(*k))
        .filter_map(|t| t.get("metadata")?.get("fsmap")?.get("verify"))
        .next();
    let Some(table) = table else {
        return Ok(Config::default());
    };
    let table = table.as_table().context("fsmap.verify must be a table")?;
    let strings = |v: &toml::Value| -> Option<Vec<String>> {
        v.as_array()?
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect()
    };
    let mut cfg = Config::default();
    for (key, v) in table {
        match key.as_str() {
            "extra" => {
                let list = v
                    .as_array()
                    .context("fsmap.verify.extra must be an array of commands")?;
                for cmd in list {
                    match strings(cmd) {
                        Some(argv) if !argv.is_empty() => cfg.extra.push(argv),
                        _ => bail!(
                            "fsmap.verify.extra entries must be non-empty arrays of strings: {cmd}"
                        ),
                    }
                }
            }
            "trust" => {
                cfg.trust = strings(v).context("fsmap.verify.trust must be an array of strings")?;
            }
            other => bail!("unknown key fsmap.verify.{other} (expected extra, trust)"),
        }
    }
    Ok(cfg)
}

/// The steps `verify` runs in `root` (a workspace root), after the in-process integrity
/// step.
pub fn plan(root: &Path, mode: Mode, fsmap_exe: &Path, cfg: &Config) -> Vec<Step> {
    let locked = root.join("Cargo.lock").is_file();
    let lock_flag: &[&str] = if locked { &["--locked"] } else { &[] };
    let step = |name: &str, args: &[&str]| Step {
        name: name.to_string(),
        argv: args.iter().map(|s| (*s).to_string()).collect(),
    };
    let mut steps = vec![
        step(
            "fmt",
            &["cargo", "fmt", "--all", "--", "--check", "--color", "never"],
        ),
        step(
            "clippy",
            &[
                &["cargo", "clippy", "--all-targets", "--message-format=short"][..],
                lock_flag,
                &["--", "-D", "warnings"],
            ]
            .concat(),
        ),
        step(
            "test",
            &[&["cargo", "test"][..], lock_flag, &["--quiet"]].concat(),
        ),
    ];
    if mode == Mode::Full {
        steps.push(step("deny", &["cargo", "deny", "check"]));
    }
    for (i, argv) in cfg.extra.iter().enumerate() {
        steps.push(Step {
            name: format!("extra-{}", i + 1),
            argv: argv
                .iter()
                .map(|a| {
                    if a == "{fsmap}" {
                        fsmap_exe.to_string_lossy().into_owned()
                    } else {
                        a.clone()
                    }
                })
                .collect(),
        });
    }
    steps
}

/// Run the integrity check and every step in `root` within `budget`; `progress` sees each
/// result as it finishes. A step that runs past the remaining budget is killed and fails.
pub fn run(
    root: &Path,
    mode: Mode,
    steps: &[Step],
    cfg: &Config,
    budget: Duration,
    mut progress: impl FnMut(&StepResult),
) -> Report {
    let started = Instant::now();
    let removed_env = removed_env(root, &cfg.trust);
    let mut results = Vec::with_capacity(steps.len() + 1);
    let integrity = integrity_step(root, &cfg.trust);
    progress(&integrity);
    results.push(integrity);
    for step in steps {
        let left = budget.saturating_sub(started.elapsed());
        let r = run_step(root, step, cfg, left);
        progress(&r);
        results.push(r);
    }
    Report {
        root: root.to_string_lossy().into_owned(),
        mode: match mode {
            Mode::Quick => "quick",
            Mode::Full => "full",
        },
        steps: results,
        removed_env,
    }
}

/// The environment as text (values lossily: a weakening value stays weakening).
pub fn env_vars() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.to_string_lossy().into_owned())))
        .collect()
}

/// Weakening environment variables set right now for work in `root`, minus trusted ones.
fn removed_env(root: &Path, trust: &[String]) -> Vec<String> {
    let vars = env_vars();
    integrity::weakening_env(
        vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        trust,
        root,
    )
}

/// A command that runs like a gate step: in `root`, without the weakening environment
/// variables or a git hook's repository, with the build directory inside the repository.
pub fn step_command(root: &Path, program: &str, cfg: &Config) -> Command {
    let mut c = Command::new(program);
    // Panic message and location are what an agent needs; backtraces push them out of
    // the kept tail.
    c.current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .env("RUST_BACKTRACE", "0");
    // Run from a git hook in a linked worktree, a test that runs `git init` and `git commit`
    // in a scratch directory would otherwise write to this repository.
    for k in crate::git::REPOSITORY_ENV {
        c.env_remove(k);
    }
    for k in removed_env(root, &cfg.trust) {
        c.env_remove(k);
    }
    if let Some(t) = target_dir(root, &cfg.trust) {
        c.env("CARGO_TARGET_DIR", t);
    }
    c
}

/// A command for the gate's own cargo and rustc calls (finding the workspace, reading
/// metadata, the toolchain version): in `dir`, without the weakening variables or a git
/// hook's repository.
pub fn clean_command(dir: &Path, program: &str) -> Command {
    let mut c = Command::new(program);
    c.current_dir(dir);
    for k in crate::git::REPOSITORY_ENV {
        c.env_remove(k);
    }
    for k in removed_env(dir, &[]) {
        c.env_remove(k);
    }
    c
}

/// The build directory the steps use: inside the repository, unless the repository trusts
/// `target-dir`. Sharing one across copies lets a copy pass on another copy's artefacts.
fn target_dir(root: &Path, trust: &[String]) -> Option<PathBuf> {
    if trust.iter().any(|t| t == "target-dir") {
        return None;
    }
    match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(t) if t.is_absolute() && t.starts_with(root) => None,
        _ => Some(root.join("target")),
    }
}

fn integrity_step(root: &Path, trust: &[String]) -> StepResult {
    let started = Instant::now();
    let findings = integrity::scan(root, &members(root), trust);
    let mut files: Vec<String> = findings.iter().map(|f| f.file.clone()).collect();
    files.sort();
    files.dedup();
    let tail = findings
        .iter()
        .map(|f| format!("{}: {} — {}", f.file, f.kind, sanitize(&f.text)))
        .collect::<Vec<_>>()
        .join("\n");
    StepResult {
        name: "integrity".into(),
        cmd: "cargo/rustfmt/rustup settings that weaken the gate".into(),
        status: if findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        ms: started.elapsed().as_millis() as u64,
        tail: {
            let mut tail = tail;
            if findings.iter().any(|f| !f.trustable) {
                tail.push_str(
                    "\nFiles inside the repository that git does not track are what CI never \
                     sees: remove them. If the project needs one, committing it is the user's \
                     decision; ask them.",
                );
            }
            if findings.iter().any(|f| f.trustable) {
                tail.push_str(
                    "\nSettings outside the repository: remove them, or list the kind under \
                     [*.metadata.fsmap.verify] trust if this machine needs them.",
                );
            }
            tail
        },
        files,
    }
}

fn run_step(root: &Path, step: &Step, cfg: &Config, left: Duration) -> StepResult {
    let cmd = step.argv.join(" ");
    let result = |status, ms, tail: String, files| StepResult {
        name: step.name.clone(),
        cmd: cmd.clone(),
        status,
        ms,
        tail,
        files,
    };
    if step.name == "deny" && !cargo_subcommand_exists(root, "deny") {
        return result(
            Status::Skip,
            0,
            "cargo-deny is not installed (cargo install cargo-deny --locked)".into(),
            Vec::new(),
        );
    }
    if left.is_zero() {
        return result(
            Status::Fail,
            0,
            "not run: the time budget was used up by earlier steps".into(),
            Vec::new(),
        );
    }
    let Some((program, args)) = step.argv.split_first() else {
        return result(Status::Fail, 0, "empty command".into(), Vec::new());
    };
    let mut c = step_command(root, program, cfg);
    c.args(args);
    let o = proc::run(c, left);
    if let Some(e) = &o.start_error {
        return result(
            Status::Fail,
            o.ms,
            format!("could not start `{program}`: {e}"),
            Vec::new(),
        );
    }
    if o.timed_out {
        return result(
            Status::Fail,
            o.ms,
            format!(
                "killed after {:.0} s (time budget)\n{}",
                o.ms as f64 / 1000.0,
                tail(&o.output, 10)
            ),
            Vec::new(),
        );
    }
    if o.success() {
        return result(Status::Pass, o.ms, String::new(), Vec::new());
    }
    let files = match step.name.as_str() {
        "fmt" | "clippy" => attributed_files(root, &o.output),
        _ => Vec::new(),
    };
    result(Status::Fail, o.ms, tail(&o.output, TAIL_LINES), files)
}

/// Files named by rustfmt (`Diff in /abs/path.rs:12:`) and short-format compiler
/// diagnostics (`src/a.rs:3:5: error[E0308]: …`), relative to `root`.
fn attributed_files(root: &Path, output: &str) -> Vec<String> {
    let mut files: Vec<String> = output
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let path = if let Some(rest) = l.strip_prefix("Diff in ") {
                rest.rsplitn(3, ':').nth(2)?
            } else {
                diagnostic(l)?.0
            };
            rel_path(root, path)
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// A compiler diagnostic in `--message-format=short` form, `path:line:col: error…` or
/// `…: warning…`: the path and the level. The path may start with a drive letter (`C:\…`),
/// so splitting at the first colon is wrong.
pub fn diagnostic(line: &str) -> Option<(&str, &str)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(.+?):\d+:\d+: (error|warning)").expect("a valid pattern")
    });
    let c = RE.captures(line)?;
    Some((c.get(1)?.as_str(), c.get(2)?.as_str()))
}

/// `path` relative to `root` with forward slashes; `None` for a path outside `root`.
pub fn rel_path(root: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    // Rooted, not only absolute: on Windows `/elsewhere/d.rs` is rooted but has no drive,
    // and is no more relative to the workspace than `C:\elsewhere\d.rs`. rustfmt prints
    // verbatim paths there (`\\?\C:\…`) and cargo plain ones, so both are simplified.
    let rel = if p.has_root() {
        dunce::simplified(p)
            .strip_prefix(dunce::simplified(root))
            .ok()?
    } else {
        p
    };
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn cargo_subcommand_exists(root: &Path, sub: &str) -> bool {
    Command::new("cargo")
        .args([sub, "--version"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The last `n` non-empty lines of `text`, sanitised.
pub fn tail(text: &str, n: usize) -> String {
    let text = sanitize(text);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("… {start} earlier lines omitted\n"));
    }
    out.push_str(&lines[start..].join("\n"));
    out
}

/// Removes terminal escape sequences (CSI `ESC [ … letter`, OSC `ESC ] … BEL|ESC \`, and
/// two-byte `ESC x`) and every other control character except newline and tab: tool
/// output is untrusted text on its way into an agent's context.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Human-readable report: one line per step, failing tails underneath.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    if !report.removed_env.is_empty() {
        out.push_str(&format!(
            "note: ran without {} (trust them as env:NAME to keep them)\n",
            report.removed_env.join(", ")
        ));
    }
    for s in &report.steps {
        out.push_str(&format!(
            "{:<4} {:<9} {:>6.1}s  {}\n",
            s.status.label(),
            s.name,
            s.ms as f64 / 1000.0,
            s.cmd
        ));
        if s.status != Status::Pass && !s.tail.is_empty() {
            for line in s.tail.lines() {
                out.push_str("       ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    let failed = report.failures().count();
    out.push_str(&if failed == 0 {
        format!("verify ({}): all steps passed\n", report.mode)
    } else {
        format!("verify ({}): {failed} step(s) failed\n", report.mode)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_last_lines_and_says_what_it_dropped() {
        let text = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = tail(&text, 3);
        assert_eq!(t, "… 47 earlier lines omitted\nline 48\nline 49\nline 50");
        assert_eq!(tail("a\n\n b \n", 5), "a\n b ");
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        assert_eq!(sanitize("\u{1b}[31m-x\u{1b}[m"), "-x");
        assert_eq!(
            sanitize("a\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{1b}\\b"),
            "alinkb"
        );
        assert_eq!(sanitize("p\r\u{8}q\tr\n"), "pq\tr\n");
    }

    #[test]
    fn attributes_fmt_and_clippy_output() {
        let root = Path::new("/w");
        let out = "Diff in /w/src/a.rs:12:\n-x\n+y\n\
                   src/b.rs:3:5: warning: unused variable\n\
                   src\\c.rs:1:1: error[E0308]: mismatched types\n\
                   error: could not compile `x`\n\
                   /elsewhere/d.rs:1:1: warning: outside\n";
        assert_eq!(
            attributed_files(root, out),
            ["src/a.rs", "src/b.rs", "src/c.rs"]
        );
    }

    /// rustfmt names files by their verbatim path on Windows, cargo by the plain one; either
    /// may be the workspace root's form.
    #[cfg(windows)]
    #[test]
    fn attributes_verbatim_and_plain_windows_paths() {
        let out = "Diff in \\\\?\\C:\\w\\src\\a.rs:1:\n-x\n+y\n\
                   C:\\w\\src\\b.rs:1:1: error[E0308]: mismatched types\n";
        for root in [r"C:\w", r"\\?\C:\w"] {
            assert_eq!(
                attributed_files(Path::new(root), out),
                ["src/a.rs", "src/b.rs"],
                "{root}"
            );
        }
    }
}
