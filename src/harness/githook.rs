//! The git pre-push hook and the push audit (decision D-16). The pre-bash hook reads the
//! agent's shell commands to find pushes, which no reading of shell gets right every time;
//! git knows exactly what it is about to push. A small shim in the repository's hooks
//! directory hands each push made from Claude Code to `fsmap hook git-pre-push`, and the
//! audit at the end of a turn reports pushes that went around the shim.
//!
//! A push is verified when each commit it publishes holds a tree the gate passed on (with
//! the same configuration, environment and toolchain), or is HEAD of a work tree whose
//! fingerprint passed with nothing uncommitted. In a Claude Code cloud session the hook only
//! reports: the platform pushes the branch when it releases the session, and a refused push
//! there would lose the session's commits with the container (user decision R1).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::state;
use super::verify::Config;
use crate::git;

/// Whether the hook may refuse a push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Refuse a push the gate has not verified.
    Enforce,
    /// Let it through with a warning, record it, and report it at the end of the turn.
    Report,
}

impl Mode {
    /// `Report` in a Claude Code cloud session, where every process Claude Code starts
    /// (the release push included) inherits `CLAUDE_CODE_REMOTE=true`.
    pub fn current() -> Mode {
        if std::env::var("CLAUDE_CODE_REMOTE").is_ok_and(|v| v == "true") {
            Mode::Report
        } else {
            Mode::Enforce
        }
    }
}

/// A git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Oid(String);

impl Oid {
    pub fn parse(s: &str) -> Option<Oid> {
        (matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Oid(s.to_string()))
    }

    /// The all-zero id git uses for "no object" (a deleted or new ref).
    pub fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        &self.0[..10]
    }
}

/// One line of the pre-push hook's standard input. `local_ref` is whatever the user typed
/// (`HEAD~1`, `(delete)`): shown, never interpreted.
pub struct RefUpdate {
    pub local_ref: String,
    pub local: Oid,
    pub remote_ref: String,
    pub remote: Oid,
}

/// Parse the hook's standard input. Empty input is a push that sends nothing (everything
/// up to date); a line that is not four fields with two object ids is an error.
pub fn parse_updates(stdin: &str) -> Result<Vec<RefUpdate>> {
    stdin
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [local_ref, local, remote_ref, remote] = fields[..] else {
                bail!("unexpected pre-push input line: {line:?}");
            };
            let oid = |s: &str| Oid::parse(s).with_context(|| format!("not an object id: {s:?}"));
            Ok(RefUpdate {
                local_ref: local_ref.to_string(),
                local: oid(local)?,
                remote_ref: remote_ref.to_string(),
                remote: oid(remote)?,
            })
        })
        .collect()
}

const MARKER: &str = "# fsmap-harness pre-push v1";

/// Where the shim stands in a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shim {
    /// Ours, byte for byte, and executable.
    Ours,
    /// Ours by its marker, but changed or not executable: rewritten at session start.
    Changed,
    Missing,
    /// Someone else's `pre-push`: left alone.
    Foreign,
    /// `core.hooksPath` points elsewhere, so git would not run ours.
    HooksPath(PathBuf),
    /// `cargo-husky` installs its hooks only where none exists: ours would displace them.
    CargoHusky,
    /// `git-hook = false` in the verify table.
    OptedOut,
    /// Not a git repository, or git could not say where its hooks are.
    NoRepository,
}

impl Shim {
    /// Why the push check is off, for the user; `None` when it is on or deliberately off.
    pub fn off_reason(&self) -> Option<String> {
        match self {
            Shim::Ours | Shim::OptedOut => None,
            Shim::Changed => Some("the pre-push hook was changed".into()),
            Shim::Missing => Some("the pre-push hook is missing".into()),
            Shim::Foreign => Some("another tool's pre-push hook is installed".into()),
            Shim::HooksPath(p) => Some(format!("core.hooksPath points to {}", p.display())),
            Shim::CargoHusky => Some("cargo-husky manages this repository's hooks".into()),
            Shim::NoRepository => Some("git could not locate the hooks directory".into()),
        }
    }
}

/// The shim for a workspace at `rel` from the work tree top. It runs only for processes
/// Claude Code started (`CLAUDECODE=1`); a person's push is not its business. In a cloud
/// session it never refuses, whatever happens to fsmap (missing, too old, crashed): a
/// refused push there can lose the session's commits (R1).
fn shim_text(rel: &str) -> String {
    let rel = rel.replace('\'', r"'\''");
    format!(
        "#!/bin/sh\n\
         {MARKER}\n\
         # Checks pushes made from Claude Code with `fsmap hook git-pre-push`. Installed by\n\
         # `fsmap hook session-start`; to turn it off, set git-hook = false under\n\
         # [package.metadata.fsmap.verify] or [workspace.metadata.fsmap.verify].\n\
         [ \"$CLAUDECODE\" = 1 ] || exit 0\n\
         if fsmap=$(command -v fsmap); then\n\
         \x20 \"$fsmap\" hook git-pre-push --workspace '{rel}' \"$@\"\n\
         \x20 status=$?\n\
         \x20 [ $status = 0 ] && exit 0\n\
         \x20 [ $status = 1 ] || echo '[harness] fsmap could not run the push check (too old, or it failed).' >&2\n\
         else\n\
         \x20 echo '[harness] fsmap is not on PATH, so this push was not checked.' >&2\n\
         \x20 status=127\n\
         fi\n\
         [ \"$CLAUDE_CODE_REMOTE\" = true ] && exit 0\n\
         [ $status = 1 ] || echo '[harness] Ask the user to install or update fsmap, or to set git-hook = false.' >&2\n\
         exit 1\n"
    )
}

/// The hooks directory git uses, the common one the shim belongs in, and the workspace's
/// path from the work tree top.
fn locate(ws: &Path) -> Option<(PathBuf, PathBuf, String)> {
    let out = git::run({
        let mut c = git::git(ws);
        c.args([
            "rev-parse",
            "--show-prefix",
            "--path-format=absolute",
            "--git-path",
            "hooks",
            "--git-common-dir",
        ]);
        c
    })
    .ok()?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines();
    let prefix = lines.next()?.trim().trim_end_matches('/');
    let used = PathBuf::from(lines.next()?.trim());
    let common = PathBuf::from(lines.next()?.trim()).join("hooks");
    let rel = if prefix.is_empty() { "." } else { prefix };
    Some((used, common, rel.to_string()))
}

/// Paths compared with their directory resolved (the file itself may not exist yet).
fn same_place(a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| match (p.parent(), p.file_name()) {
        (Some(d), Some(n)) => {
            dunce::canonicalize(d).map_or_else(|_| p.to_path_buf(), |d| d.join(n))
        }
        _ => p.to_path_buf(),
    };
    resolve(a) == resolve(b)
}

/// Whether the workspace depends on cargo-husky: in its lock file, or declared in its
/// manifest (cargo may not have resolved it yet).
fn uses_cargo_husky(ws: &Path) -> bool {
    let read = |name: &str| std::fs::read_to_string(ws.join(name)).unwrap_or_default();
    read("Cargo.lock")
        .lines()
        .any(|l| l.trim() == "name = \"cargo-husky\"")
        || read("Cargo.toml").lines().any(|l| {
            l.trim_start()
                .strip_prefix("cargo-husky")
                .is_some_and(|rest| rest.trim_start().starts_with(['=', '.']))
        })
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    true
}

/// The shim's state, the path it belongs at and the text it should have.
fn inspect(ws: &Path, cfg: &Config) -> (Shim, Option<(PathBuf, String)>) {
    let Some((used, common, rel)) = locate(ws) else {
        return (Shim::NoRepository, None);
    };
    let path = common.join("pre-push");
    let text = shim_text(&rel);
    let existing = std::fs::read(&path).ok();
    let ours = existing
        .as_deref()
        .is_some_and(|b| String::from_utf8_lossy(b).lines().any(|l| l == MARKER));
    let state = if cfg.no_git_hook {
        Shim::OptedOut
    } else if !same_place(&used, &common) {
        Shim::HooksPath(used)
    } else if existing.is_some() && !ours {
        Shim::Foreign
    } else if uses_cargo_husky(ws) && !ours {
        Shim::CargoHusky
    } else if existing.is_none() {
        Shim::Missing
    } else if existing.as_deref() == Some(text.as_bytes())
        && std::fs::metadata(&path).is_ok_and(|m| executable(&m))
    {
        Shim::Ours
    } else {
        Shim::Changed
    };
    (state, Some((path, text)))
}

pub fn shim_state(ws: &Path, cfg: &Config) -> Shim {
    inspect(ws, cfg).0
}

/// Install or restore the shim where nothing else claims the hook; with `git-hook = false`,
/// remove ours. Returns the state afterwards.
pub fn install(ws: &Path, cfg: &Config) -> Result<Shim> {
    let (state, place) = inspect(ws, cfg);
    let Some((path, text)) = place else {
        return Ok(state);
    };
    match state {
        Shim::Missing | Shim::Changed => {
            let dir = path.parent().context("hooks directory")?;
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let tmp = dir.join(format!("pre-push.fsmap.{}.tmp", std::process::id()));
            std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                    .context("making the pre-push hook executable")?;
            }
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("installing {}", path.display()))?;
            Ok(Shim::Ours)
        }
        Shim::OptedOut => {
            let ours = std::fs::read_to_string(&path).is_ok_and(|t| t.lines().any(|l| l == MARKER));
            if ours {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
            Ok(Shim::OptedOut)
        }
        other => Ok(other),
    }
}

/// `<tree> <context digest>` for the work tree as `git add -A` would commit it, computed on
/// a copy of the index (the real one is not touched), with filter drivers neutralised as
/// for every work-tree comparison (D-13). `None` where a tree does not say what the gate
/// saw: submodules (their work trees are not in it) and git-ignored files cargo compiles.
pub fn verified_key(ws: &Path, trust: &[String]) -> Option<String> {
    if !state::submodules(ws).is_empty() || !state::ignored_build_inputs(ws).is_empty() {
        return None;
    }
    let index = git::run({
        let mut c = git::git(ws);
        c.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        c
    })
    .ok()?;
    let index = PathBuf::from(String::from_utf8_lossy(&index).trim());
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let copy = Scratch(state::dir(ws).join(format!("index.{}.{n}.tmp", std::process::id())));
    if index.is_file() {
        std::fs::copy(&index, &copy.0).ok()?;
        // With the original's mtime: git trusts an entry's stat data only when the entry is
        // older than the index file ("racy git"), and a copy made now would make every entry
        // older, hiding a same-size change made in the tick of the last `git add`.
        let mtime = std::fs::metadata(&index).and_then(|m| m.modified()).ok()?;
        std::fs::File::options()
            .write(true)
            .open(&copy.0)
            .and_then(|f| f.set_modified(mtime))
            .ok()?;
    }
    let with_index = |args: &[&str]| {
        let mut c = git::git_worktree(ws);
        c.args(args).env("GIT_INDEX_FILE", &copy.0);
        git::run(c)
    };
    with_index(&["add", "-A", "--", ":/"]).ok()?;
    let tree = with_index(&["write-tree"]).ok()?;
    let tree = String::from_utf8_lossy(&tree).trim().to_string();
    Oid::parse(&tree)?;
    Some(format!("{tree} {}", state::context_digest(ws, trust)))
}

/// A temporary index, removed with its lock file when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// The tree a commit (or a tag of one) holds.
pub fn tree_of(ws: &Path, oid: &Oid) -> Option<String> {
    let out = git::run({
        let mut c = git::git(ws);
        c.args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{}^{{tree}}", oid.as_str()));
        c
    })
    .ok()?;
    Some(String::from_utf8_lossy(&out).trim().to_string())
}

/// The commit an object names, tags peeled; `None` for anything else.
pub fn commit_of(ws: &Path, oid: &Oid) -> Option<Oid> {
    let out = git::run({
        let mut c = git::git(ws);
        c.args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{}^{{commit}}", oid.as_str()));
        c
    })
    .ok()?;
    Oid::parse(String::from_utf8_lossy(&out).trim())
}

/// Whether pushing `oid` publishes no commit the remote's tracking refs lack (a tag on a
/// commit already pushed, `--follow-tags`): nothing new to verify.
pub fn publishes_nothing_new(ws: &Path, oid: &Oid, remote: &str) -> bool {
    git::run({
        let mut c = git::git(ws);
        c.args(["rev-list", "-n1", oid.as_str(), "--not"])
            .arg(format!("--remotes={remote}"));
        c
    })
    .is_ok_and(|out| out.iter().all(u8::is_ascii_whitespace))
}

/// Remote-tracking refs and the commits they point at (symbolic `HEAD` refs left out).
pub fn remote_refs(ws: &Path) -> BTreeMap<String, String> {
    git::run({
        let mut c = git::git(ws);
        c.args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/remotes",
        ]);
        c
    })
    .map(|out| {
        String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| l.split_once('\0'))
            .filter(|(r, _)| !r.ends_with("/HEAD"))
            .map(|(r, o)| (r.to_string(), o.to_string()))
            .collect()
    })
    .unwrap_or_default()
}

/// Commits that pushes from this repository wrote into the reflogs of the refs that moved
/// between `before` and `now`. Each reflog is read newest first down to the entry that set
/// the ref to its `before` commit: by order, not time, because an entry's time is the
/// committer date, which `GIT_COMMITTER_DATE` can set to anything. Fetches move refs too;
/// their entries say so and are skipped.
pub fn pushed_since(
    ws: &Path,
    before: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, oid) in now {
        let start = before.get(name);
        if start == Some(oid) {
            continue;
        }
        let Ok(log) = git::run({
            let mut c = git::git(ws);
            c.args(["log", "-g", "-n", "50", "--format=%H%x00%gs"])
                .arg(name)
                .arg("--");
            c
        }) else {
            continue;
        };
        for line in String::from_utf8_lossy(&log).lines() {
            let Some((commit, subject)) = line.split_once('\0') else {
                continue;
            };
            if start.is_some_and(|s| s == commit) {
                break;
            }
            if subject.starts_with("update by push") {
                out.push((name.clone(), commit.to_string()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hook_input() {
        let a = "a".repeat(40);
        let z = "0".repeat(40);
        let u = parse_updates(&format!(
            "refs/heads/x {a} refs/heads/x {z}\n\n(delete) {z} refs/heads/y {a}\n"
        ))
        .unwrap();
        assert_eq!(u.len(), 2);
        assert!(!u[0].local.is_zero() && u[0].remote.is_zero());
        assert!(u[1].local.is_zero());
        assert!(parse_updates("").unwrap().is_empty());
        assert!(parse_updates("refs/heads/x abc refs/heads/x def\n").is_err());
        assert!(parse_updates("only three fields\n").is_err());
        assert!(Oid::parse(&"b".repeat(64)).is_some());
        assert!(Oid::parse(&"B".repeat(40)).is_none());
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Git trusts an index entry's stat data unless the entry is as new as the index file
    /// ("racy git"). A copy of the index made now is newer than every entry, so a change of
    /// the same size in the same timestamp tick as the last `git add` went unseen and the
    /// verdict named the old tree (seen as an intermittent refusal of a verified push).
    #[test]
    fn the_index_copy_keeps_racy_entries_racy() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q"]);
        git(d, &["config", "core.trustctime", "false"]);
        git(d, &["config", "core.checkStat", "minimal"]);
        // Stat data from a while ago, so that a copy made now is newer than the entry.
        let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let set = |p: &Path| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        };
        std::fs::write(d.join("a.txt"), "aaaa\n").unwrap();
        set(&d.join("a.txt"));
        git(d, &["add", "a.txt"]);
        git(
            d,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "a",
            ],
        );
        // Same size and stat data as the index entry, and an index no newer than it.
        std::fs::write(d.join("a.txt"), "bbbb\n").unwrap();
        set(&d.join("a.txt"));
        set(&d.join(".git/index"));
        let key = verified_key(d, &[]).expect("a key");
        git(d, &["add", "-A"]);
        let expected = git(d, &["write-tree"]);
        assert!(key.starts_with(&expected), "{key} vs {expected}");
    }

    #[test]
    fn shim_quotes_the_workspace() {
        let t = shim_text("it's here");
        assert!(t.contains(r"--workspace 'it'\''s here'"), "{t}");
        assert!(t.lines().any(|l| l == MARKER));
        assert!(t.starts_with("#!/bin/sh\n"));
    }
}
