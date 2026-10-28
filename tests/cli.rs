//! End-to-end tests: build a small git repository with a known history, run the real
//! binary against it, and check every output contract.

use std::path::Path;
use std::process::{Command, Output};

fn fsmap(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fsmap"))
        .args(args)
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cwd.join(".cache-test"))
        .output()
        .expect("run fsmap")
}

fn git(dir: &Path, args: &[&str], time: i64) {
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

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn commit(dir: &Path, author: &str, msg: &str, time: i64) {
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

/// A Python package with a module-level import cycle (a ↔ b), a type-only import that
/// must not count, two files that always change together without importing each other,
/// a committed build directory, and an ignored cache.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"], 0);
    let t0 = 1_700_000_000;
    write(d, "pyproject.toml", "[project]\nname = \"pkg\"\n");
    write(d, ".gitignore", "cache/\n");
    write(d, "pkg/__init__.py", "");
    write(
        d,
        "pkg/a.py",
        "from pkg import b\n\ndef fa():\n    return b.fb()\n",
    );
    write(
        d,
        "pkg/b.py",
        "import pkg.a\n\ndef fb():\n    if True:\n        return 1\n",
    );
    write(
        d,
        "pkg/c.py",
        "import typing\nif typing.TYPE_CHECKING:\n    from pkg import a\n",
    );
    write(d, "proto/schema.txt", "v1\n");
    write(d, "client/api.py", "VERSION = 1\n");
    // Committed bytecode cache: an unambiguous build artifact.
    write(d, "pkg/__pycache__/a.cpython-312.pyc", &"x".repeat(200_000));
    for i in 0..6 {
        write(d, &format!("pkg/__pycache__/m{i}.cpython-312.pyc"), "y");
    }
    // A tracked `build/` next to a manifest is build *scripts*, not output.
    write(d, "build/release.py", "import os\n");
    write(d, "cache/blob.bin", "z");
    commit(d, "Alice", "init", t0);
    for i in 0..8 {
        write(d, "proto/schema.txt", &format!("v{}\n", i + 2));
        write(d, "client/api.py", &format!("VERSION = {}\n", i + 2));
        commit(
            d,
            if i % 2 == 0 { "Alice" } else { "Bob" },
            "bump",
            t0 + (i + 1) * 86_400,
        );
        // Unrelated work in between, so co-change is informative (lift > 1).
        write(
            d,
            "pkg/b.py",
            &format!("import pkg.a\n\ndef fb():\n    if True:\n        return {i}\n"),
        );
        commit(d, "Carol", "tweak", t0 + (i + 1) * 86_400 + 3600);
        write(
            d,
            "pkg/a.py",
            &format!("from pkg import b\n\ndef fa():\n    return b.fb() + {i}\n"),
        );
        commit(d, "Carol", "tweak", t0 + (i + 1) * 86_400 + 7200);
    }
    dir
}

#[test]
fn json_snapshot_contract() {
    let repo = fixture();
    let out = fsmap(&["scan", ".", "-q"], repo.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], "fsmap/1");
    let nodes = v["nodes"].as_array().unwrap();
    let find = |name: &str| nodes.iter().position(|n| n["name"] == name).unwrap();
    // Git state and origins.
    assert_eq!(nodes[find("a.py")]["git"], "tracked");
    assert_eq!(nodes[find("blob.bin")]["git"], "ignored");
    assert_eq!(nodes[find("__pycache__")]["zone"], "artifact");
    assert!(
        nodes[find("build")]["zone"].is_null(),
        "tracked build/ is source"
    );
    // History: api.py changed in 9 commits by two authors.
    assert_eq!(nodes[find("api.py")]["commits"], 9);
    // The module-level cycle is found; the TYPE_CHECKING import does not make c.py part of it.
    let cycles = v["deps"]["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    let members: Vec<&str> = cycles[0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            nodes[m.as_u64().unwrap() as usize]["name"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(members, ["a.py", "b.py"]);
    // Change coupling without an import link.
    let pair = v["coupling"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            let names = [p["a"].as_u64().unwrap(), p["b"].as_u64().unwrap()]
                .map(|i| nodes[i as usize]["name"].as_str().unwrap().to_string());
            names.contains(&"api.py".to_string()) && names.contains(&"schema.txt".to_string())
        })
        .expect("api.py ⇄ schema.txt coupling");
    assert_eq!(pair["shared"], 9, "8 bumps + the initial commit");
    // Findings: committed build output is flagged.
    let rules: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"committed-artifact"), "{rules:?}");
    assert!(rules.contains(&"dep-cycle"), "{rules:?}");
}

#[test]
fn insights_gate_and_baseline_ratchet() {
    let repo = fixture();
    let d = repo.path();
    let gate = fsmap(&["insights", ".", "-q", "--fail-on", "medium"], d);
    assert_eq!(
        gate.status.code(),
        Some(1),
        "committed __pycache__/ is at least medium severity"
    );
    let base = fsmap(
        &[
            "insights",
            ".",
            "-q",
            "--write-baseline",
            "base.json",
            "--format",
            "json",
        ],
        d,
    );
    assert!(base.status.success());
    // Same tree + baseline ⇒ nothing new ⇒ pass.
    let again = fsmap(
        &[
            "insights",
            ".",
            "-q",
            "--fail-on",
            "medium",
            "--baseline",
            "base.json",
        ],
        d,
    );
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    // SARIF is valid JSON with results.
    let sarif = fsmap(&["insights", ".", "-q", "--format", "sarif"], d);
    let v: serde_json::Value = serde_json::from_slice(&sarif.stdout).unwrap();
    assert_eq!(v["version"], "2.1.0");
    assert!(!v["runs"][0]["results"].as_array().unwrap().is_empty());
}

#[test]
fn brief_diff_reports_forgotten_partner() {
    let repo = fixture();
    let d = repo.path();
    write(d, "client/api.py", "VERSION = 99\n");
    let out = fsmap(&["brief", "--diff"], d);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Possibly forgotten"), "{text}");
    assert!(text.contains("proto/schema.txt"), "{text}");

    // Hook mode: JSON for Claude Code, silent for unremarkable files.
    let mut child = Command::new(env!("CARGO_BIN_EXE_fsmap"))
        .args(["brief", "--hook"])
        .current_dir(d)
        .env("XDG_CACHE_HOME", d.join(".cache-test"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let abs = std::fs::canonicalize(d.join("client/api.py")).unwrap();
    let event = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": { "file_path": abs.to_string_lossy() }
    });
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("hook JSON");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("schema.txt")
    );
}

#[test]
fn context_is_deterministic_and_bounded() {
    let repo = fixture();
    let d = repo.path();
    let a = fsmap(&["context", ".", "-q", "--budget", "1500"], d);
    let b = fsmap(&["context", ".", "-q", "--budget", "1500"], d);
    assert!(a.status.success());
    assert_eq!(a.stdout, b.stdout, "same tree ⇒ byte-identical context");
    let text = String::from_utf8_lossy(&a.stdout);
    assert!(text.starts_with("# Repository map:"));
    assert!(text.contains("## Layout"));
    assert!(
        text.len() / 4 < 2500,
        "roughly within budget: {} tokens",
        text.len() / 4
    );
}

#[test]
fn html_report_is_self_contained() {
    let repo = fixture();
    let d = repo.path();
    let out = fsmap(&["report", ".", "-q", "-o", "r.html"], d);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let html = std::fs::read_to_string(d.join("r.html")).unwrap();
    assert!(html.contains("id=\"fsmap-data\""));
    assert!(!html.contains("{{DATA}}"));
    assert!(!html.contains("src=\"http"), "no external scripts");
    assert!(html.contains("Content-Security-Policy"));
}

#[test]
fn snapshot_diff_and_agent_ignore() {
    let repo = fixture();
    let d = repo.path();
    assert!(
        fsmap(&["scan", ".", "-q", "-o", "old.json"], d)
            .status
            .success()
    );
    write(d, "data/big.bin", &"q".repeat(2 << 20));
    assert!(
        fsmap(&["scan", ".", "-q", "-o", "new.json"], d)
            .status
            .success()
    );
    let out = fsmap(&["diff", "old.json", "new.json"], d);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("data/big.bin"), "{text}");

    let ign = fsmap(&["agent-ignore", ".", "-q"], d);
    let v: serde_json::Value = serde_json::from_slice(&ign.stdout).unwrap();
    let deny: Vec<&str> = v["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert!(deny.iter().any(|r| r.contains("__pycache__")), "{deny:?}");
}

#[test]
fn sizes_only_needs_no_git() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a/b/c.txt", "hello");
    let out = fsmap(&["scan", ".", "-q", "--sizes-only"], dir.path());
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["totals"]["files"], 1);
    assert_eq!(v["totals"]["code"], 0, "contents not read");
}
