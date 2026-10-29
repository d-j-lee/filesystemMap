//! End-to-end tests of the agent harness (decision D-15): the real binary's hooks against a
//! small Cargo project in a git repository. Each test pins one way the gate could fail
//! open; most come from the design review and from the two parallel harnesses' findings.

#![expect(
    clippy::unwrap_used,
    reason = "allow-unwrap-in-tests does not cover helper functions in integration tests"
)]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{commit, git, init, write};
use serde_json::{Value, json};

const T0: i64 = 1_700_000_000;

const CARGO_TOML: &str = "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                          [lints.clippy]\nunwrap_used = \"warn\"\n";
const GOOD: &str = "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    \
                    #[test]\n    fn adds() {\n        assert_eq!(super::add(2, 2), 4);\n    }\n}\n";
const FAILING_TEST: &str = "pub fn add(a: u32, b: u32) -> u32 {\n    a + b + 1\n}\n\n#[cfg(test)]\nmod tests {\n    \
                            #[test]\n    fn adds() {\n        assert_eq!(super::add(2, 2), 4);\n    }\n}\n";
const TYPE_ERROR: &str = "pub fn add(a: u32, b: u32) -> u32 {\n    \"oops\"\n}\n";

/// The hooks' cache and state directories live inside the fixture (see `hook_env`); they
/// are ignored so they cannot change the fingerprint and make a test pass for the wrong
/// reason (the mutation check caught exactly that).
const IGNORE: &str = "/target\n/.cache-test\n/.state-test\n";

/// A committed, passing Cargo project. `Cargo.lock` exists before any hook runs: a hook's
/// `cargo metadata` would otherwise create it and change the fingerprint by itself.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(d);
    write(d, "Cargo.toml", CARGO_TOML);
    write(d, ".gitignore", IGNORE);
    write(d, "src/lib.rs", GOOD);
    lockfile(d);
    commit(d, "A", "init", T0);
    dir
}

fn lockfile(d: &Path) {
    let ok = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(d)
        .status()
        .unwrap()
        .success();
    assert!(ok, "cargo generate-lockfile");
}

/// Run `fsmap hook <event>` as Claude Code would; returns its JSON (Null for no output).
fn hook_env(d: &Path, event: &str, input: &Value, env: &[(&str, &str)]) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fsmap"));
    cmd.args(["hook", event])
        .current_dir(d)
        .env("CLAUDE_PROJECT_DIR", d)
        .env("XDG_CACHE_HOME", d.join(".cache-test"))
        .env("XDG_STATE_HOME", d.join(".state-test"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hooks always exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text.trim()).unwrap()
    }
}

fn hook(d: &Path, event: &str, input: &Value) -> Value {
    hook_env(d, event, input, &[])
}

fn start(d: &Path, source: &str) -> Value {
    hook(
        d,
        "session-start",
        &json!({"session_id": "s1", "hook_event_name": "SessionStart", "source": source}),
    )
}

fn stop(d: &Path, active: bool) -> Value {
    hook(
        d,
        "stop",
        &json!({"session_id": "s1", "hook_event_name": "Stop", "stop_hook_active": active}),
    )
}

fn blocked(v: &Value) -> bool {
    v["decision"] == "block"
}

#[test]
fn post_edit_returns_compile_errors() {
    let repo = fixture();
    let d = repo.path();
    write(d, "src/lib.rs", TYPE_ERROR);
    let file = d.join("src/lib.rs");
    let v = hook(
        d,
        "post-edit",
        &json!({"session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": "Edit",
                "tool_input": {"file_path": file}}),
    );
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.contains("error[E0308]"), "{reason}");
    assert!(reason.contains("src/lib.rs:2:"), "{reason}");
    assert_eq!(reason.matches("E0308").count(), 1, "deduplicated: {reason}");
    // Not Rust: no check, no opinion.
    write(d, "notes.md", "hi\n");
    let v = hook(
        d,
        "post-edit",
        &json!({"session_id": "s1", "tool_input": {"file_path": d.join("notes.md")}}),
    );
    assert!(v.is_null(), "{v}");
}

#[test]
fn stop_gate_blocks_then_releases_after_three_strikes() {
    let repo = fixture();
    let d = repo.path();
    assert!(
        start(d, "startup")["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("[harness]")
    );
    assert!(stop(d, false).is_null(), "unchanged tree: nothing to check");

    write(d, "src/lib.rs", FAILING_TEST);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("adds"), "{v}");
    assert!(blocked(&stop(d, true)), "second strike");
    let v = stop(d, true);
    assert!(!blocked(&v), "third strike releases: {v}");
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Stop trying")
    );
    assert!(v["systemMessage"].as_str().unwrap().contains("report"));
    assert!(stop(d, true).is_null(), "released: the report turn may end");

    // The user writes again: the gate is back.
    hook(
        d,
        "prompt",
        &json!({"session_id": "s1", "prompt": "try again"}),
    );
    assert!(blocked(&stop(d, false)));
    write(d, "src/lib.rs", GOOD);
    let v = stop(d, false);
    assert!(!blocked(&v), "fixed: {v}");
}

/// The other harness's hole 1: committing during the turn cleaned the tree and the gate
/// saw nothing to check.
#[test]
fn committing_during_the_turn_does_not_skip_the_gate() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    commit(d, "Agent", "wip", T0 + 60);
    assert!(blocked(&stop(d, false)));
}

/// The other harness's last hole: an empty repository's first commit bypassed the gate.
#[test]
fn first_commit_in_an_empty_repository_is_gated() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(d);
    write(d, "Cargo.toml", CARGO_TOML);
    write(d, ".gitignore", IGNORE);
    write(d, "src/lib.rs", GOOD);
    lockfile(d);
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    commit(d, "Agent", "first", T0);
    assert!(blocked(&stop(d, false)));
}

/// Re-editing an already modified file changes no `git status` line.
#[test]
fn re_editing_a_dirty_file_reruns_the_gate() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        &GOOD.replace("a + b", "b + a"), // dirty, still passing
    );
    assert!(!blocked(&stop(d, false)));
    write(d, "src/lib.rs", FAILING_TEST);
    assert!(
        blocked(&stop(d, false)),
        "same status line, different content"
    );
}

/// Review finding B1, reproduced first: these settings turned a failing clippy and a
/// failing test into exit 0 — even hidden from git through .git/info/exclude.
#[test]
fn cargo_config_that_weakens_the_gate_fails_it() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(
        d,
        ".cargo/config.toml",
        "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n\
         [target.x86_64-unknown-linux-gnu]\nrunner = \"true\"\n",
    );
    write(d, ".git/info/exclude", "/.cargo\n");
    write(d, "src/lib.rs", FAILING_TEST);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.contains("integrity"), "{reason}");
    assert!(
        reason.contains("cap-lints") && reason.contains("runner"),
        "{reason}"
    );
}

/// The same bypass through the environment: the variables are removed for the steps.
#[test]
fn weakening_environment_variables_are_ignored() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        &GOOD.replace("a + b", "Some(a + b).unwrap()"), // clippy::unwrap_used
    );
    let v = hook_env(
        d,
        "stop",
        &json!({"session_id": "s1", "stop_hook_active": false}),
        &[
            ("RUSTFLAGS", "--cap-lints allow"),
            ("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER", "true"),
        ],
    );
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("unwrap"), "{v}");
}

/// Review finding B3: SessionStart fires on compact/resume too; it must not move the
/// session's baseline onto a broken tree.
#[test]
fn compaction_does_not_reset_the_baseline() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    start(d, "compact");
    assert!(blocked(&stop(d, false)));
}

/// Review finding M2: only background agents (which edit the tree) postpone the gate.
#[test]
fn background_shells_do_not_switch_the_gate_off() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    let with = |t: &str| {
        hook(
            d,
            "stop",
            &json!({"session_id": "s1", "stop_hook_active": false,
                    "background_tasks": [{"id": "x", "type": t, "status": "running"}]}),
        )
    };
    let v = with("subagent");
    assert!(!blocked(&v) && v["systemMessage"].as_str().unwrap().contains("background"));
    assert!(
        blocked(&with("shell")),
        "a dev server must not disable the gate"
    );
}

/// The gate runs before work is published, so a broken commit is not pushed.
#[test]
fn push_is_denied_while_the_gate_fails() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    let bash = |cmd: &str| {
        hook(
            d,
            "pre-bash",
            &json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": cmd}}),
        )
    };
    assert!(bash("git status").is_null());
    let v = bash("git commit -am x && git push origin main");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{v}");
    write(d, "src/lib.rs", GOOD);
    assert!(bash("git push origin main").is_null());
}

/// Review finding M7: a failure only in files this session never touched (here an
/// unformatted file from before the session) is reported, not blocked on.
#[test]
fn pre_existing_failures_in_untouched_files_do_not_block() {
    let repo = fixture();
    let d = repo.path();
    write(d, "src/lib.rs", &format!("mod other;\n{GOOD}"));
    write(d, "src/other.rs", "pub fn   ugly( ) {}\n");
    commit(d, "A", "unformatted", T0 + 10);
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        &format!("mod other;\n{}", GOOD.replace("a + b", "b + a")),
    );
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(msg.contains("src/other.rs (fmt)"), "{msg}");
}

/// Suppressions and removed assertions are shown to the user even when the gate passes.
#[test]
fn passing_changes_that_weaken_checks_are_reported() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\n#[allow(dead_code)]\nfn unused() {}\n\n\
         #[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore]\n    fn adds() {}\n}\n",
    );
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(msg.contains("1 allow"), "{msg}");
    assert!(msg.contains("1 ignored test"), "{msg}");
    assert!(msg.contains("removed assertion"), "{msg}");
}

/// A second session in the same worktree is announced.
#[test]
fn concurrent_sessions_are_announced() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    let v = hook(
        d,
        "session-start",
        &json!({"session_id": "s2", "source": "startup"}),
    );
    assert!(
        v["systemMessage"]
            .as_str()
            .unwrap()
            .contains("another Claude session")
    );
}

/// The shipped settings announce a missing fsmap instead of running without a gate.
#[cfg(unix)]
#[test]
fn template_session_start_is_loud_without_fsmap() {
    let settings: Value = serde_json::from_str(include_str!("../harness/settings.json")).unwrap();
    let cmd = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let out = Command::new("sh")
        .args(["-c", cmd])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["systemMessage"].as_str().unwrap().contains("OFF"));
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("cargo clippy")
    );
}

/// This repository uses the shipped templates verbatim (one source of truth).
#[test]
fn repository_copies_match_the_templates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (copy, template) in [
        (".claude/settings.json", "harness/settings.json"),
        (
            ".claude/agents/rust-reviewer.md",
            "harness/agents/rust-reviewer.md",
        ),
        ("AGENTS.md", "harness/AGENTS.md"),
    ] {
        let a = std::fs::read_to_string(root.join(copy)).unwrap();
        let b = std::fs::read_to_string(root.join(template)).unwrap();
        assert_eq!(a, b, "{copy} differs from {template}");
    }
}

#[test]
fn stats_summarise_the_log() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    stop(d, false);
    let out = Command::new(env!("CARGO_BIN_EXE_fsmap"))
        .args(["hook", "stats"])
        .env("CLAUDE_PROJECT_DIR", d)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("gate block"), "{text}");
    assert!(text.contains("session-start"), "{text}");
    let _ = git; // shared helper module
}
