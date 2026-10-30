//! Helpers shared by the end-to-end tests.

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output};

pub fn fsmap(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsmap"))
        .args(args)
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cwd.join(".cache-test"))
        .output()
        .expect("run fsmap")
}

pub fn git(dir: &Path, args: &[&str], time: i64) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", format!("@{time} +0000"))
        .env("GIT_COMMITTER_DATE", format!("@{time} +0000"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("run git")
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub fn commit(dir: &Path, author: &str, msg: &str, time: i64) {
    git(dir, &["add", "-A"], time);
    git(
        dir,
        &[
            "-c",
            &format!("user.name={author}"),
            "-c",
            "user.email=dev@example.com",
            "commit",
            "-qm",
            msg,
        ],
        time,
    );
}

/// Parse a snapshot and return (json, path-of-node helper).
pub fn snapshot(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("snapshot JSON")
}

pub fn node_path(v: &serde_json::Value, mut i: usize) -> String {
    let nodes = v["nodes"].as_array().unwrap();
    let mut parts = Vec::new();
    while let Some(p) = nodes[i]["parent"].as_u64() {
        parts.push(nodes[i]["name"].as_str().unwrap().to_string());
        i = p as usize;
    }
    parts.reverse();
    parts.join("/")
}

pub fn node_by_path<'a>(v: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    let n = v["nodes"].as_array().unwrap().len();
    let i = (0..n)
        .find(|&i| node_path(v, i) == path)
        .unwrap_or_else(|| panic!("no node {path}"));
    &v["nodes"][i]
}

pub fn init(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"], 0);
}
