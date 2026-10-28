//! `fsmap verify`: the single gate shared by people, agent hooks and CI (decision D-15).
//!
//! The steps for a Cargo workspace are fixed (fmt, clippy, test; cargo-deny in full mode).
//! A repository adds its own checks in `Cargo.toml`:
//!
//! ```toml
//! [workspace.metadata.fsmap.verify]   # or [package.metadata.fsmap.verify]
//! extra = [["node", "--check", "viewer/app.js"], ["{fsmap}", "insights", ".", "--fail-on", "high", "-q"]]
//! ```
//!
//! `{fsmap}` is replaced by the running fsmap binary. Every step runs even after a failure,
//! so one round reports everything that is wrong.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// How much of a failing step's output is kept.
const TAIL_LINES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// fmt, clippy, tests and the repository's extra steps (the stop gate).
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

#[derive(Clone, Debug, Serialize)]
pub struct StepResult {
    pub name: String,
    pub cmd: String,
    pub status: Status,
    pub ms: u64,
    /// Last lines of output for a failure, or the reason for a skip.
    pub tail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub root: String,
    pub mode: &'static str,
    pub steps: Vec<StepResult>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.steps.iter().all(|s| s.status != Status::Fail)
    }

    pub fn failures(&self) -> impl Iterator<Item = &StepResult> {
        self.steps.iter().filter(|s| s.status == Status::Fail)
    }
}

/// The Cargo workspace root containing `dir`.
pub fn workspace_root(dir: &Path) -> Result<PathBuf> {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(dir)
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

/// The steps `verify` runs in `root` (a workspace root).
pub fn plan(root: &Path, mode: Mode, fsmap_exe: &Path) -> Result<Vec<Step>> {
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
                &["cargo", "clippy", "--all-targets"][..],
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
    for (i, argv) in extra_steps(root)?.into_iter().enumerate() {
        let argv: Vec<String> = argv
            .into_iter()
            .map(|a| {
                if a == "{fsmap}" {
                    fsmap_exe.to_string_lossy().into_owned()
                } else {
                    a
                }
            })
            .collect();
        steps.push(Step {
            name: format!("extra-{}", i + 1),
            argv,
        });
    }
    Ok(steps)
}

/// `extra` from `[workspace.metadata.fsmap.verify]` or the root package's metadata, read
/// through `cargo metadata` so no TOML parser is needed.
fn extra_steps(root: &Path) -> Result<Vec<Vec<String>>> {
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .context("failed to run cargo metadata")?;
    if !out.status.success() {
        // A broken manifest is reported by the clippy/test steps; nothing extra to add.
        return Ok(Vec::new());
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("cargo metadata output")?;
    let root_manifest = root.join("Cargo.toml");
    let package_meta = meta["packages"].as_array().and_then(|ps| {
        ps.iter().find(|p| {
            p["manifest_path"]
                .as_str()
                .is_some_and(|m| Path::new(m) == root_manifest)
        })
    });
    let table = [
        Some(&meta["metadata"]),
        package_meta.map(|p| &p["metadata"]),
    ]
    .into_iter()
    .flatten()
    .map(|m| &m["fsmap"]["verify"]["extra"])
    .find(|v| v.is_array());
    let Some(list) = table.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    list.iter()
        .map(|cmd| {
            let argv: Option<Vec<String>> = cmd.as_array().map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            });
            match argv {
                Some(v) if !v.is_empty() && v.len() == cmd.as_array().map_or(0, Vec::len) => Ok(v),
                _ => bail!("fsmap.verify.extra entries must be non-empty arrays of strings: {cmd}"),
            }
        })
        .collect()
}

/// Run every step in `root`; `progress` sees each result as it finishes.
pub fn run(
    root: &Path,
    mode: Mode,
    steps: &[Step],
    mut progress: impl FnMut(&StepResult),
) -> Report {
    let mut results = Vec::with_capacity(steps.len());
    for step in steps {
        let r = run_step(root, step);
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
    }
}

fn run_step(root: &Path, step: &Step) -> StepResult {
    let cmd = step.argv.join(" ");
    let started = Instant::now();
    let done = |status, tail: String| StepResult {
        name: step.name.clone(),
        cmd: cmd.clone(),
        status,
        ms: started.elapsed().as_millis() as u64,
        tail,
    };
    if step.name == "deny" && !cargo_subcommand_exists(root, "deny") {
        return done(
            Status::Skip,
            "cargo-deny is not installed (cargo install cargo-deny --locked)".into(),
        );
    }
    let Some((program, args)) = step.argv.split_first() else {
        return done(Status::Fail, "empty command".into());
    };
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .output();
    match out {
        Err(e) => done(Status::Fail, format!("could not start `{program}`: {e}")),
        Ok(o) if o.status.success() => done(Status::Pass, String::new()),
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            done(Status::Fail, tail(&text, TAIL_LINES))
        }
    }
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

/// The last `n` non-empty lines of `text`, without terminal colour codes.
pub fn tail(text: &str, n: usize) -> String {
    let text = strip_ansi(text);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("… {start} earlier lines omitted\n"));
    }
    out.push_str(&lines[start..].join("\n"));
    out
}

/// Removes CSI escape sequences (`ESC [ … letter`); tools differ in how they honour
/// "no colour" settings.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Human-readable report: one line per step, failing tails underneath.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    for s in &report.steps {
        let mark = match s.status {
            Status::Pass => "ok  ",
            Status::Fail => "FAIL",
            Status::Skip => "skip",
        };
        out.push_str(&format!(
            "{mark} {:<8} {:>6.1}s  {}\n",
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
        assert_eq!(tail("\u{1b}[31m-x\u{1b}[m\n", 5), "-x");
    }
}
