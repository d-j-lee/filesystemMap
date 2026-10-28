//! What the hooks remember between calls, and the work-tree fingerprint the stop gate keys on.
//!
//! State lives in `<git-dir>/fsmap-harness/` (per worktree, outside the work tree, untouched by
//! `cargo clean`). Every hook treats a missing or unreadable state as "nothing known yet".

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git;

/// Files larger than this are fingerprinted by size and mtime instead of content.
const HASH_LIMIT: u64 = 4 << 20;

/// Build inputs that are often git-ignored but change what the gate checks.
const IGNORED_INPUTS: &[&str] = &[
    ".cargo/config.toml",
    ".cargo/config",
    "rust-toolchain.toml",
    "rust-toolchain",
];

#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    /// Fingerprint when the session started (or was resumed/compacted).
    pub start_fp: Option<String>,
    /// Consecutive failed gate runs.
    pub failures: u32,
    /// The release message was already sent for the current failure streak.
    pub released: bool,
    /// Fingerprint for which the forgotten-partner note was already given.
    pub noted_fp: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Fingerprint of the last tree the gate passed on.
    pub last_pass_fp: Option<String>,
    pub sessions: BTreeMap<String, Session>,
}

/// `<git-dir>/fsmap-harness` for the work tree at `root`, if `root` is in one.
pub fn dir(root: &Path) -> Option<PathBuf> {
    let out = git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--absolute-git-dir"]);
        c
    })
    .ok()?;
    let gitdir = PathBuf::from(String::from_utf8_lossy(&out).trim());
    Some(gitdir.join("fsmap-harness"))
}

pub fn load(root: &Path) -> State {
    dir(root)
        .and_then(|d| std::fs::read(d.join("state.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Best effort: a read-only git dir means the gate simply runs more often.
pub fn save(root: &Path, state: &State) {
    let Some(d) = dir(root) else { return };
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        let tmp = d.join("state.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, d.join("state.json"));
        }
    }
}

/// A hash of everything a commit-or-edit can change: HEAD, the status of every path
/// (untracked included), the contents of every dirty file, and ignored build inputs.
/// Re-editing an already-modified file changes no status line, so contents are hashed.
pub fn fingerprint(root: &Path) -> Option<String> {
    let head = git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--verify", "-q", "HEAD"]);
        c
    })
    .map(|o| String::from_utf8_lossy(&o).trim().to_string())
    .unwrap_or_else(|_| "unborn".into());
    // Work-tree comparison: the repository's filter drivers are neutralised.
    let status = git::run({
        let mut c = git::git_worktree(root);
        c.args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ]);
        c
    })
    .ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(head.as_bytes());
    h.update(&status);
    for path in dirty_paths(&status) {
        hash_file(&mut h, &root.join(&path));
    }
    for rel in IGNORED_INPUTS {
        hash_file(&mut h, &root.join(rel));
    }
    Some(format!("{:016x}", h.digest()))
}

/// Paths named by `git status --porcelain=v2 -z` (renames give their new path).
fn dirty_paths(status: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = status.split(|&b| b == 0);
    while let Some(entry) = fields.next() {
        let text = String::from_utf8_lossy(entry);
        let mut parts = text.splitn(2, ' ');
        let kind = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        // Field counts before the path: ordinary 8, rename/copy 9 (+ original path as
        // the next NUL field), unmerged 10, untracked 0.
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
