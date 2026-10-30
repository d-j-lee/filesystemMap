//! What the hooks remember between calls, and the work-tree fingerprint the stop gate keys
//! on (decision D-15).
//!
//! State lives in `<git-dir>/fsmap-harness/` (per worktree, outside the work tree,
//! untouched by `cargo clean`), or under `$XDG_STATE_HOME/fsmap/harness/` when there is no
//! usable git directory. Every hook treats missing or unreadable state as "nothing known".

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::integrity;
use crate::git;

/// Files larger than this are fingerprinted by size and mtime instead of content.
const HASH_LIMIT: u64 = 4 << 20;
/// Session records not seen for this long are dropped.
const SESSION_TTL_SECS: i64 = 14 * 86_400;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Fingerprint when the session started. Written once, on `startup`; never
    /// overwritten by resume, compact or clear (review finding B3).
    pub start_fp: Option<String>,
    /// HEAD when the session was first seen, for telling which files it changed.
    pub start_head: Option<String>,
    /// Files already changed or untracked when the session was first seen, relative to
    /// the work tree top, with a hash of their contents: unchanged since, they are not the
    /// session's (third review, finding 7).
    pub start_dirty: BTreeMap<String, String>,
    /// Consecutive blocking gate failures since the user last wrote.
    pub failures: u32,
    /// The "stop and report" message was given for the current failure streak.
    pub released: bool,
    /// Unix seconds of the last hook call for this session.
    pub last_seen: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Fingerprint of the last tree the gate passed on.
    pub last_pass_fp: Option<String>,
    /// The last blocking failure: re-blocking on an unchanged tree needs no re-run.
    pub last_fail_fp: Option<String>,
    pub last_fail_reason: Option<String>,
    /// Every failure of that run was in files the session did not change.
    pub last_fail_untouched_only: bool,
    pub sessions: BTreeMap<String, Session>,
}

impl State {
    pub fn session(&mut self, id: &str) -> &mut Session {
        self.sessions.entry(id.to_string()).or_default()
    }

    /// Other sessions seen in the last hour (a second agent in the same worktree).
    pub fn live_others(&self, id: &str, now: i64) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(k, s)| k.as_str() != id && now - s.last_seen < 3600)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Where the state for the work tree at `root` lives.
pub fn dir(root: &Path) -> PathBuf {
    let gitdir = git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--absolute-git-dir"]);
        c
    })
    .ok()
    .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()));
    if let Some(g) = gitdir {
        let d = g.join("fsmap-harness");
        if std::fs::create_dir_all(&d).is_ok() {
            return d;
        }
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    let key = xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes());
    base.join("fsmap/harness").join(format!("{key:016x}"))
}

pub fn load(root: &Path) -> State {
    load_in(&dir(root))
}

fn load_in(d: &Path) -> State {
    std::fs::read(d.join("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Apply `f` to the freshest state and save it, under a short lock. Hooks run the gate on
/// a snapshot, which can take minutes, and record the outcome through this, so a record
/// another hook wrote meanwhile survives (final review M5). Returns false when the state
/// could not be written; callers say so instead of failing silently.
pub fn update(root: &Path, f: impl FnOnce(&mut State)) -> bool {
    update_in(&dir(root), f)
}

fn update_in(d: &Path, f: impl FnOnce(&mut State)) -> bool {
    if std::fs::create_dir_all(d).is_err() {
        return false;
    }
    let Some(_lock) = Lock::acquire(&d.join("state.lock")) else {
        return false;
    };
    let mut st = load_in(d);
    f(&mut st);
    save_in(d, &mut st)
}

/// An exclusive operating-system lock on `state.lock` (`flock`, `LockFileEx`), held for one
/// read-modify-write. The system releases it when its holder exits, so a crashed hook
/// leaves nothing behind and no clock decides that a lock is stale. Where the file system
/// cannot lock, the update is not written and the caller says so: writing unlocked could
/// overwrite another hook's record.
struct Lock(std::fs::File);

impl Lock {
    fn acquire(path: &Path) -> Option<Lock> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        file.lock().ok()?;
        Some(Lock(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closing the file releases the lock as well.
        let _ = self.0.unlock();
    }
}

/// Atomic replace (write a temp file, rename).
fn save_in(d: &Path, state: &mut State) -> bool {
    let now = now();
    state
        .sessions
        .retain(|_, s| now - s.last_seen < SESSION_TTL_SECS);
    let Ok(json) = serde_json::to_vec_pretty(state) else {
        return false;
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = d.join(format!("state.json.{}.{n}.tmp", std::process::id()));
    std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, d.join("state.json")).is_ok()
}

/// One line per hook outcome, for `fsmap hook stats` (D-15: measure before claiming the
/// gate helps).
pub fn log(root: &Path, entry: &serde_json::Value) {
    let d = dir(root);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join("log.jsonl"))
    {
        let _ = writeln!(f, "{entry}");
    }
}

pub fn read_log(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir(root).join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn head(root: &Path) -> Option<String> {
    git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--verify", "-q", "HEAD"]);
        c
    })
    .ok()
    .map(|o| String::from_utf8_lossy(&o).trim().to_string())
}

/// A hash of everything a commit or an edit can change that the gate depends on:
/// - HEAD and the status of every path, untracked included (submodules separately);
/// - the contents of every dirty file (re-editing a modified file changes no status line);
/// - git-ignored files cargo would still compile (`tests/*.rs`, `examples/`, …) and the
///   configuration files and environment that change what the gate checks (B1, M4);
/// - the toolchain (`rustc -vV`) and this fsmap version.
pub fn fingerprint(root: &Path, trust: &[String]) -> Option<String> {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    hash_tree(&mut h, root, 0)?;
    for path in ignored_build_inputs(root) {
        hash_file(&mut h, &root.join(path));
    }
    // Through links: a tracked link's target is what cargo reads (fifth review).
    for path in integrity::config_files(root) {
        hash_file(&mut h, &dunce::canonicalize(&path).unwrap_or(path));
    }
    let vars = super::verify::env_vars();
    for name in integrity::weakening_env(
        vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        trust,
        root,
    ) {
        if let Some((_, v)) = vars.iter().find(|(k, _)| *k == name) {
            h.update(format!("{name}={v}\n").as_bytes());
        }
    }
    if let Ok(o) = super::verify::clean_command(root, "rustc")
        .arg("-vV")
        .output()
    {
        h.update(&o.stdout);
    }
    h.update(env!("CARGO_PKG_VERSION").as_bytes());
    Some(format!("{:016x}", h.digest()))
}

fn hash_tree(h: &mut xxhash_rust::xxh3::Xxh3, root: &Path, depth: usize) -> Option<()> {
    h.update(head(root).unwrap_or_else(|| "unborn".into()).as_bytes());
    // Work-tree comparison with filters neutralised; submodules are not entered here
    // (their own filter drivers would run, D-15 review M5) but fingerprinted below with
    // their own neutralised config.
    let status = git::run({
        let mut c = git::git_worktree(root);
        c.args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
        ]);
        c
    })
    .ok()?;
    h.update(&status);
    // `git status` prints paths from the work tree top, also for a workspace below it
    // (fifth review: joined onto the workspace, a nested one's edits were never read).
    let top = toplevel(root).unwrap_or_else(|| root.to_path_buf());
    for path in dirty_paths(&status) {
        hash_file(h, &top.join(&path));
    }
    for path in hidden_index_entries(root) {
        h.update(path.as_bytes());
        hash_file(h, &top.join(&path));
    }
    if depth < 2 {
        for sub in submodules(root) {
            let p = root.join(&sub);
            if p.join(".git").exists() {
                h.update(sub.as_bytes());
                // A submodule we cannot read is still part of the hash by name.
                let _ = hash_tree(h, &p, depth + 1);
            }
        }
    }
    Some(())
}

/// Paths of submodules (gitlinks) in the index.
fn submodules(root: &Path) -> Vec<String> {
    git::run({
        let mut c = git::git(root);
        c.args(["ls-files", "-s", "-z"]);
        c
    })
    .map(|o| {
        o.split(|&b| b == 0)
            .filter_map(|e| {
                let e = String::from_utf8_lossy(e);
                let (meta, path) = e.split_once('\t')?;
                meta.starts_with("160000").then(|| path.to_string())
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Git-ignored files that cargo still compiles or reads (review finding M4).
fn ignored_build_inputs(root: &Path) -> Vec<String> {
    git::run({
        let mut c = git::git(root);
        c.args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            "*.rs",
            "Cargo.toml",
            "*/Cargo.toml",
            "Cargo.lock",
            ":!:target/",
        ]);
        c
    })
    .map(|o| {
        o.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    })
    .unwrap_or_default()
}

/// Paths named by `git status --porcelain=v2 -z` (renames give their new path).
/// The changed and untracked files of the work tree around `dir` with a hash of each one's
/// contents, relative to the work tree top; empty without git or when there are more than
/// `MAX_DIRTY` (then every change counts as the session's, the strict reading).
pub fn dirty_snapshot(dir: &Path) -> BTreeMap<String, String> {
    const MAX_DIRTY: usize = 2000;
    let Some(top) = toplevel(dir) else {
        return BTreeMap::new();
    };
    let status = git::run({
        let mut c = git::git_worktree(dir);
        c.args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
        ]);
        c
    })
    .unwrap_or_default();
    let paths = dirty_paths(&status);
    if paths.len() > MAX_DIRTY {
        return BTreeMap::new();
    }
    paths
        .into_iter()
        .map(|p| {
            let hash = content_hash(&top.join(&p));
            (p, hash)
        })
        .collect()
}

/// A hash of a file's contents, `-` when it cannot be read (deleted, a directory). Files
/// over 16 MiB are identified by size and modification time instead of being read.
pub fn content_hash(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return "-".into();
    };
    if meta.len() > 16 << 20 {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        return format!("{}:{mtime}", meta.len());
    }
    std::fs::read(path).map_or_else(
        |_| "-".into(),
        |b| format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&b)),
    )
}

/// The current paths in `git status --porcelain=v2 -z` output (a rename's new path).
pub fn dirty_paths(status: &[u8]) -> Vec<String> {
    status_entries(status)
        .into_iter()
        .filter(|e| !e.origin)
        .map(|e| e.path)
        .collect()
}

/// One path in `git status` output.
pub struct StatusEntry {
    pub path: String,
    pub untracked: bool,
    /// The original path of a rename or copy.
    pub origin: bool,
}

/// The paths in `git status --porcelain=v2 -z` output; a rename's original path is listed
/// too, as a tracked change (fourth review, M2).
pub fn status_entries(status: &[u8]) -> Vec<StatusEntry> {
    let mut out = Vec::new();
    let mut fields = status.split(|&b| b == 0);
    while let Some(entry) = fields.next() {
        let text = String::from_utf8_lossy(entry);
        let mut parts = text.splitn(2, ' ');
        let kind = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        // Field counts before the path: ordinary 7, rename/copy 8 (the original path is
        // the next NUL field), unmerged 9, untracked 0.
        let skip = match kind {
            "1" => 7,
            "2" => 8,
            "u" => 9,
            "?" => 0,
            _ => continue,
        };
        if let Some(path) = rest.splitn(skip + 1, ' ').nth(skip) {
            out.push(StatusEntry {
                path: path.to_string(),
                untracked: kind == "?",
                origin: false,
            });
        }
        if kind == "2"
            && let Some(origin) = fields.next()
        {
            out.push(StatusEntry {
                path: String::from_utf8_lossy(origin).into_owned(),
                untracked: false,
                origin: true,
            });
        }
    }
    out
}

/// Tracked files whose edits git was told not to show (`--skip-worktree`,
/// `--assume-unchanged`) and that differ from the index, relative to the work tree top:
/// `git status` cannot see those edits, so they are hashed and treated as uncommitted
/// (fourth review, M4). Unchanged ones, and ones absent from disk as in a sparse checkout,
/// are not work to commit (fifth review).
pub fn hidden_index_entries(dir: &Path) -> Vec<String> {
    let Some(top) = toplevel(dir) else {
        return Vec::new();
    };
    // `-v -s`: "<tag> <mode> <object> <stage>\t<path>".
    let listing = git::run({
        let mut c = git::git(&top);
        c.args(["ls-files", "-v", "-s", "-z"]);
        c
    })
    .unwrap_or_default();
    let candidates: Vec<(String, String)> = listing
        .split(|&b| b == 0)
        .filter(|e| e.len() > 2 && (e[0] == b'S' || e[0].is_ascii_lowercase()))
        .filter_map(|e| {
            let text = String::from_utf8_lossy(&e[2..]);
            let (meta, path) = text.split_once('\t')?;
            let object = meta.split(' ').nth(1)?.to_string();
            std::fs::symlink_metadata(top.join(path))
                .is_ok_and(|m| m.is_file())
                .then(|| (path.to_string(), object))
        })
        .collect();
    let mut changed = Vec::new();
    for chunk in candidates.chunks(500) {
        let Ok(out) = git::run({
            let mut c = git::git(&top);
            c.args(["hash-object", "--no-filters", "--"])
                .args(chunk.iter().map(|(p, _)| p));
            c
        }) else {
            // Cannot tell: count them all, the strict reading.
            changed.extend(chunk.iter().map(|(p, _)| p.clone()));
            continue;
        };
        let hashes = String::from_utf8_lossy(&out);
        for ((path, object), now) in chunk.iter().zip(hashes.lines()) {
            if now.trim() != object {
                changed.push(path.clone());
            }
        }
    }
    changed
}

fn toplevel(dir: &Path) -> Option<PathBuf> {
    git::run({
        let mut c = git::git(dir);
        c.args(["rev-parse", "--show-toplevel"]);
        c
    })
    .ok()
    .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
}

fn hash_file(h: &mut xxhash_rust::xxh3::Xxh3, path: &Path) {
    h.update(path.to_string_lossy().as_bytes());
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        h.update(b"\0missing");
        return;
    };
    if !meta.is_file() || meta.len() > HASH_LIMIT {
        h.update(&meta.len().to_le_bytes());
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        h.update(&mtime.to_le_bytes());
        return;
    }
    let mut buf = Vec::new();
    if std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .is_ok()
    {
        h.update(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Final review M5: hooks of two sessions updating at once must not drop each other's
    /// records.
    /// A tracked link's target is what cargo reads: editing it changes the fingerprint
    /// (fifth review).
    #[cfg(unix)]
    #[test]
    fn the_fingerprint_follows_config_links() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("shared.toml");
        std::fs::write(&target, "[build]\njobs = 1\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(r)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(r.join(".cargo")).unwrap();
        std::os::unix::fs::symlink(&target, r.join(".cargo/config.toml")).unwrap();
        git(&["add", "-A"]);
        let before = fingerprint(r, &[]);
        std::fs::write(&target, "[build]\njobs = 2\n").unwrap();
        assert!(before.is_some());
        assert_ne!(before, fingerprint(r, &[]));
    }

    #[test]
    fn concurrent_updates_keep_every_record() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for i in 0..15 {
                        // A write that fails is reported, not lost: say which it was.
                        assert!(update_in(&d, |st| {
                            st.session(&format!("s{t}-{i}")).last_seen = now();
                        }));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let st: State =
            serde_json::from_slice(&std::fs::read(d.join("state.json")).unwrap()).unwrap();
        assert_eq!(st.sessions.len(), 8 * 15);
    }

    /// A held lock is never taken over, however old its file looks. The lock file used
    /// before counted as stale 30 s after its mtime (a clock jump ages a fresh one) and
    /// was given up on after 5 s of waiting; either way two writers could overwrite each
    /// other's records.
    #[test]
    fn a_held_lock_is_never_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(d.join("state.lock"))
            .unwrap();
        held.lock().unwrap();
        held.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        let writer = {
            let d = d.clone();
            std::thread::spawn(move || update_in(&d, |st| st.last_pass_fp = Some("x".into())))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !d.join("state.json").exists(),
            "written while the lock was held"
        );
        held.unlock().unwrap();
        assert!(writer.join().unwrap());
        assert_eq!(load_in(&d).last_pass_fp.as_deref(), Some("x"));
    }

    #[test]
    fn porcelain_v2_paths() {
        let status = b"1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
? notes/x y.txt\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0";
        assert_eq!(
            dirty_paths(status),
            ["src/a b.rs", "new.rs", "notes/x y.txt", "conflict.rs"]
        );
    }
}
