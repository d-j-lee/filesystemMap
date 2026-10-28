//! What the hooks remember between calls, and the work-tree fingerprint the stop gate keys
//! on (decision D-15).
//!
//! State lives in `<git-dir>/fsmap-harness/` (per worktree, outside the work tree,
//! untouched by `cargo clean`), or under `$XDG_STATE_HOME/fsmap/harness/` when there is no
//! usable git directory. Every hook treats missing or unreadable state as "nothing known".

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::integrity;
use crate::git;

/// Files larger than this are fingerprinted by size and mtime instead of content.
const HASH_LIMIT: u64 = 4 << 20;
/// Session records not seen for this long are dropped.
const SESSION_TTL_SECS: i64 = 14 * 86_400;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// Fingerprint and HEAD when the session started. Written once, on `startup`; never
    /// overwritten by resume, compact or clear (review finding B3).
    pub start_fp: Option<String>,
    pub start_head: Option<String>,
    /// Consecutive blocking gate failures since the user last wrote.
    pub failures: u32,
    /// The "stop and report" message was given for the current failure streak.
    pub released: bool,
    /// Unix seconds of the last hook call for this session.
    pub last_seen: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Fingerprint of the last tree the gate passed on.
    pub last_pass_fp: Option<String>,
    /// The last blocking failure: re-blocking on an unchanged tree needs no re-run.
    pub last_fail_fp: Option<String>,
    pub last_fail_reason: Option<String>,
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
    std::fs::read(dir(root).join("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Atomic replace (write a temp file, rename). Returns false when the state could not be
/// written; callers say so instead of failing silently.
pub fn save(root: &Path, state: &mut State) -> bool {
    let now = now();
    state
        .sessions
        .retain(|_, s| now - s.last_seen < SESSION_TTL_SECS);
    let d = dir(root);
    if std::fs::create_dir_all(&d).is_err() {
        return false;
    }
    let Ok(json) = serde_json::to_vec_pretty(state) else {
        return false;
    };
    let tmp = d.join(format!("state.json.{}.tmp", std::process::id()));
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
    for path in integrity::config_files(root) {
        hash_file(&mut h, &path);
    }
    let vars: Vec<(String, String)> = std::env::vars().collect();
    for name in integrity::weakening_env(vars.iter().map(|(k, _)| k.as_str()), trust) {
        if let Some((_, v)) = vars.iter().find(|(k, _)| *k == name) {
            h.update(format!("{name}={v}\n").as_bytes());
        }
    }
    if let Ok(o) = Command::new("rustc").arg("-vV").current_dir(root).output() {
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
    for path in dirty_paths(&status) {
        hash_file(h, &root.join(&path));
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
pub fn dirty_paths(status: &[u8]) -> Vec<String> {
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
            out.push(path.to_string());
        }
        if kind == "2" {
            fields.next();
        }
    }
    out
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
