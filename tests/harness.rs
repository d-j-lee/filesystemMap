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

/// Commit only what is staged (`common::commit` adds everything).
fn commit_staged(d: &Path, msg: &str, time: i64) {
    git(
        d,
        &[
            "-c",
            "user.name=Agent",
            "-c",
            "user.email=dev@example.com",
            "commit",
            "-qm",
            msg,
        ],
        time,
    );
}

fn push(d: &Path) -> Value {
    hook(
        d,
        "pre-bash",
        &json!({"session_id": "s1", "hook_event_name": "PreToolUse", "tool_name": "Bash",
                "tool_input": {"command": "git push origin main"}}),
    )
}

/// `fsmap verify <d> <args>`: exit code and stdout followed by stderr.
fn verify(d: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fsmap"))
        .arg("verify")
        .arg(d)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_string()
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
        &format!(
            "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n\
             [target.{}]\nrunner = \"true\"\n\
             [alias]\nclippy = \"check\"\n",
            host_triple()
        ),
    );
    write(d, ".git/info/exclude", "/.cargo\n");
    write(d, "src/lib.rs", FAILING_TEST);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.contains("integrity"), "{reason}");
    for kind in ["cap-lints", "runner", "alias"] {
        assert!(reason.contains(kind), "{kind}: {reason}");
    }
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
    commit(d, "Agent", "broken", T0 + 60);
    let v = bash("git push origin main");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{v}");
    write(d, "src/lib.rs", GOOD);
    commit(d, "Agent", "fixed", T0 + 120);
    assert!(bash("git push origin main").is_null());
}

/// Second review M7: the gate checks the working tree, a push sends HEAD. With build
/// inputs left uncommitted (here a module file the commit needs), the two differ.
#[test]
fn pushing_with_uncommitted_build_inputs_is_denied() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", &format!("pub mod helper;\n{GOOD}"));
    git(d, &["add", "src/lib.rs"], T0 + 60);
    commit_staged(d, "uses helper", T0 + 60);
    write(d, "src/helper.rs", "pub const ONE: u32 = 1;\n");
    write(d, "notes.md", "scratch\n");
    // No target reads these (third review, finding 3).
    write(d, "scratch/try.rs", "fn main() {}\n");
    write(d, "docs/book.toml", "[book]\ntitle = \"x\"\n");
    let v = push(d);
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{v}");
    let why = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        why.contains("src/helper.rs")
            && !why.contains("notes.md")
            && !why.contains("scratch/try.rs")
            && !why.contains("book.toml"),
        "{why}"
    );
    git(d, &["add", "src/helper.rs"], T0 + 120);
    commit_staged(d, "helper", T0 + 120);
    let v = push(d);
    assert!(
        v.is_null(),
        "only build inputs count, the rest may stay: {v}"
    );
    // Deleting a remote branch publishes nothing local.
    write(d, "src/helper.rs", "pub const ONE: u32 = 2;\n");
    let v = hook(
        d,
        "pre-bash",
        &json!({"session_id": "s1", "tool_input": {"command": "git push origin --delete old"}}),
    );
    assert!(v.is_null(), "{v}");
}

/// Third review, finding 7: a file already changed when the session started, and not
/// touched since, is not the session's; its failure is reported as such.
#[test]
fn a_change_pending_before_the_session_is_not_the_sessions() {
    let repo = fixture();
    let d = repo.path();
    write(d, "src/lib.rs", &format!("mod other;\n{GOOD}"));
    write(d, "src/other.rs", "pub fn   ugly( ) {}\n");
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        &format!("mod other;\n{}", GOOD.replace("a + b", "b + a")),
    );
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("src/other.rs (not changed in this session)"),
        "{reason}"
    );
    let v = stop(d, true);
    assert!(!blocked(&v), "released after one block: {v}");
    // Touching it makes it the session's.
    hook(d, "prompt", &json!({"session_id": "s1"}));
    write(d, "src/other.rs", "pub fn   ugly( ) { }\n");
    let v = stop(d, false);
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("src/other.rs (changed in this session)"),
        "{reason}"
    );
}

/// A suppression already pending when the session started is reported apart from the
/// session's own (the threat model promises tracked weakening is reported on every pass,
/// also across sessions; fourth review, M3); one the session adds is its own.
#[test]
fn suppressions_pending_before_the_session_are_reported_apart() {
    let repo = fixture();
    let d = repo.path();
    write(d, "src/lib.rs", &format!("mod extra;\n{GOOD}"));
    write(d, "src/extra.rs", "#[allow(dead_code)]\nfn unused() {}\n");
    start(d, "startup");
    write(
        d,
        "src/lib.rs",
        &format!("mod extra;\n{}", GOOD.replace("a + b", "b + a")),
    );
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap_or_default();
    let (own, pending) = msg
        .split_once("already changed when this session started")
        .unwrap_or((msg, ""));
    assert!(
        !own.contains("src/extra.rs") && pending.contains("allow (src/extra.rs)"),
        "{msg}"
    );
    hook(d, "prompt", &json!({"session_id": "s1"}));
    write(
        d,
        "src/extra.rs",
        "#[allow(dead_code)]\nfn unused() {}\n#[allow(dead_code)]\nfn other() {}\n",
    );
    let v = stop(d, false);
    let msg = v["systemMessage"].as_str().unwrap_or_default();
    let own = msg.split("already changed").next().unwrap_or_default();
    assert!(own.contains("allow (src/extra.rs"), "{msg}");
}

/// Fourth review, M2: narrowing the push check to the directories targets read must not
/// let tracked sources through: a rename, a deletion, or a file git was told to ignore
/// changes of (`--skip-worktree`, M4) all change what HEAD builds.
#[test]
fn tracked_source_changes_always_count_for_a_push() {
    let repo = fixture();
    let d = repo.path();
    write(d, "tests/extra.rs", "#[test]\nfn extra() {}\n");
    write(d, "scratch/try.rs", "fn main() {}\n");
    commit(d, "A", "more files", T0 + 10);
    start(d, "startup");
    let reason = |v: &Value| {
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    git(d, &["mv", "tests/extra.rs", "extra_old.rs"], T0 + 20);
    let v = push(d);
    assert!(reason(&v).contains("tests/extra.rs"), "rename: {v}");
    git(d, &["reset", "-q", "--hard"], T0 + 20);
    std::fs::remove_file(d.join("tests/extra.rs")).unwrap();
    let v = push(d);
    assert!(reason(&v).contains("tests/extra.rs"), "deletion: {v}");
    git(d, &["checkout", "-q", "--", "tests/extra.rs"], T0 + 20);
    write(d, "scratch/try.rs", "fn main() { let _ = 1; }\n");
    let v = push(d);
    assert!(
        reason(&v).contains("scratch/try.rs"),
        "tracked, outside targets: {v}"
    );
    git(d, &["checkout", "-q", "--", "scratch/try.rs"], T0 + 20);
    // A passing edit, so that only the uncommitted-input check can deny it.
    write(d, "src/lib.rs", &GOOD.replace("a + b", "b + a"));
    git(
        d,
        &["update-index", "--skip-worktree", "src/lib.rs"],
        T0 + 20,
    );
    let v = push(d);
    let why = reason(&v);
    assert!(
        why.contains("not committed") && why.contains("src/lib.rs"),
        "skip-worktree: {v}"
    );
}

/// A config reached through `include` is a build input like `.cargo/config.toml`: an
/// uncommitted edit to it denies a push (sixth review).
#[test]
fn an_uncommitted_included_config_blocks_a_push() {
    let repo = fixture();
    let d = repo.path();
    write(d, ".cargo/config.toml", "include = [\"shared.toml\"]\n");
    write(d, ".cargo/shared.toml", "[build]\njobs = 2\n");
    commit(d, "A", "shared config", T0 + 10);
    start(d, "startup");
    write(d, ".cargo/shared.toml", "[build]\njobs = 3\n");
    let v = push(d);
    let why = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        why.contains("not committed") && why.contains(".cargo/shared.toml"),
        "{v}"
    );
}

/// An edit git was told not to show (`--skip-worktree`) still changes the fingerprint, so
/// the gate runs again (fourth review, M4).
#[test]
fn a_skip_worktree_edit_reruns_the_gate() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    git(
        d,
        &["update-index", "--skip-worktree", "src/lib.rs"],
        T0 + 40,
    );
    write(d, "src/lib.rs", &GOOD.replace("a + b", "b + a"));
    assert!(!blocked(&stop(d, false)));
    // Listed already: only its contents tell this edit from the one that passed.
    write(d, "src/lib.rs", FAILING_TEST);
    assert!(blocked(&stop(d, false)), "the hidden edit must be checked");
}

/// Fifth review: skip-worktree entries that did not change, or are absent from disk (a
/// sparse checkout), are not uncommitted work; denying every push for them would leave
/// nothing to commit.
#[test]
fn unchanged_or_sparse_skip_worktree_files_do_not_block_a_push() {
    let repo = fixture();
    let d = repo.path();
    write(d, "tools/gen/Cargo.toml", "[package]\nname = \"gen\"\n");
    write(d, "tools/gen/src/main.rs", "fn main() {}\n");
    commit(d, "A", "tool", T0 + 10);
    start(d, "startup");
    git(
        d,
        &["update-index", "--skip-worktree", "src/lib.rs"],
        T0 + 20,
    );
    git(
        d,
        &[
            "update-index",
            "--skip-worktree",
            "tools/gen/Cargo.toml",
            "tools/gen/src/main.rs",
        ],
        T0 + 20,
    );
    std::fs::remove_dir_all(d.join("tools")).unwrap();
    let v = push(d);
    assert!(v.is_null(), "{v}");
}

/// A workspace below the git top: `git status` prints paths from the top, so dirty files
/// must be read from there, or re-editing one would not rerun the gate (fifth review;
/// before, they were joined onto the workspace and never read).
#[test]
fn re_editing_a_dirty_file_in_a_nested_workspace_reruns_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    init(top);
    let d = &top.join("crate");
    write(d, "Cargo.toml", CARGO_TOML);
    write(d, ".gitignore", IGNORE);
    write(d, "src/lib.rs", GOOD);
    lockfile(d);
    commit(top, "A", "init", T0);
    start(d, "startup");
    write(d, "src/lib.rs", &GOOD.replace("a + b", "b + a"));
    assert!(!blocked(&stop(d, false)));
    write(d, "src/lib.rs", FAILING_TEST);
    assert!(blocked(&stop(d, false)), "the second edit must be checked");
}

/// The threat model: configuration git does not track fails the gate whatever it says
/// (third review, finding 1: `[env]` reaches rustc, clippy and the tests); once tracked it
/// is in the diff, is not judged, and is reported when the gate passes.
#[test]
fn hidden_config_fails_and_tracked_config_is_reported() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", &GOOD.replace("a + b", "b + a"));
    // `INSTA_UPDATE` is one the gate would judge in any file it judges.
    write(
        d,
        ".cargo/config.toml",
        "[env]\nFX_MODE = \"fast\"\nINSTA_UPDATE = \"no\"\n",
    );
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("## integrity") && reason.contains("env.FX_MODE"),
        "{reason}"
    );
    git(d, &["add", ".cargo/config.toml"], T0 + 60);
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap_or_default();
    assert!(
        msg.contains("build configuration changed") && msg.contains(".cargo/config.toml"),
        "{v}"
    );
    // A tracked file the config includes, under another name, is reported too.
    hook(d, "prompt", &json!({"session_id": "s1"}));
    write(d, ".cargo/config.toml", "include = [\"shared.toml\"]\n");
    write(d, ".cargo/shared.toml", "[env]\nFX_MODE = \"fast\"\n");
    git(d, &["add", ".cargo"], T0 + 90);
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap_or_default();
    assert!(
        msg.contains(".cargo/shared.toml") && !msg.contains("lint or format"),
        "a cargo file under another name is build configuration: {v}"
    );
}

/// Third review, finding 6: rustfmt reads the user's own configuration when the project
/// has none; CI does not have it.
#[test]
fn rustfmt_rules_from_the_home_directory_fail_the_gate() {
    let repo = fixture();
    let d = repo.path();
    let home = tempfile::tempdir().unwrap();
    write(home.path(), ".rustfmt.toml", "max_width = 40\n");
    let real_home = std::env::var("HOME").unwrap_or_default();
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{real_home}/.cargo"));
    let rustup_home =
        std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{real_home}/.rustup"));
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_fsmap"))
            .arg("verify")
            .arg(d)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("CARGO_HOME", &cargo_home)
            .env("RUSTUP_HOME", &rustup_home)
            .output()
            .unwrap()
    };
    let out = run();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("rustfmt-outside"), "{text}");
    write(d, "rustfmt.toml", "edition = \"2021\"\n");
    commit(d, "A", "own rustfmt rules", T0 + 60);
    let out = run();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("rustfmt-outside"), "{text}");
}

/// Final review M1: the gate passes only when every step passes. A failure only in files
/// this session did not change (here an unformatted file from before the session) blocks
/// once, so Claude sees it, then lets the turn end with a report; the user is told the gate
/// did NOT pass, and a push stays denied.
#[test]
fn failures_in_untouched_files_block_once_then_ask_for_a_report() {
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
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("src/other.rs (not changed in this session)"),
        "{reason}"
    );
    let v = stop(d, true);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(msg.contains("gate NOT passed"), "{msg}");
    commit(d, "Agent", "change", T0 + 60);
    let v = push(d);
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{v}");
    assert!(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("## fmt"),
        "{v}"
    );
}

/// Final review M1(a): a change can break a file it never touched. Removing the only
/// caller of a helper makes it dead code in the untouched file; that is not a pass.
#[test]
fn breaking_an_untouched_file_is_not_a_pass() {
    let repo = fixture();
    let d = repo.path();
    write(
        d,
        "src/other.rs",
        "pub(crate) fn one() -> u32 {\n    1\n}\n",
    );
    write(
        d,
        "src/lib.rs",
        &format!(
            "mod other;\n{}",
            GOOD.replace("a + b", "a + b + other::one() - 1")
        ),
    );
    commit(d, "A", "helper", T0 + 10);
    start(d, "startup");
    write(d, "src/lib.rs", &format!("mod other;\n{GOOD}"));
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(
        v["reason"].as_str().unwrap().contains("src/other.rs"),
        "{v}"
    );
}

const UNWRAP: &str = "pub fn add(a: u32, b: u32) -> u32 {\n    Some(a + b).unwrap()\n}\n";

/// Second review M6: a session that started before the first commit owns what it commits;
/// its failures are not "someone else's" and keep all three strikes.
#[test]
fn a_session_started_before_the_first_commit_owns_its_commits() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(d);
    write(d, ".gitignore", IGNORE);
    write(d, "Cargo.toml", CARGO_TOML);
    write(d, "src/lib.rs", GOOD);
    lockfile(d);
    start(d, "startup");
    write(d, "src/lib.rs", UNWRAP);
    commit(d, "Agent", "first", T0);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(
        v["reason"]
            .as_str()
            .unwrap()
            .contains("src/lib.rs (changed in this session)"),
        "{v}"
    );
    assert!(
        blocked(&stop(d, true)),
        "its own failure: second strike blocks"
    );
}

/// The same when the session starts before there is any Cargo project: the agent runs
/// `cargo init` and commits.
#[test]
fn a_session_started_before_the_cargo_project_owns_its_commits() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(d);
    write(d, ".gitignore", IGNORE);
    start(d, "startup");
    write(d, "Cargo.toml", CARGO_TOML);
    write(d, "src/lib.rs", UNWRAP);
    lockfile(d);
    commit(d, "Agent", "first", T0);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(
        blocked(&stop(d, true)),
        "its own failure: second strike blocks"
    );
}

/// The baseline commit is recorded the first time a session is seen, whatever the source:
/// a session first seen at a compaction still owns what it commits afterwards.
#[test]
fn a_session_first_seen_at_compaction_owns_its_later_commits() {
    let repo = fixture();
    let d = repo.path();
    start(d, "compact");
    write(d, "src/lib.rs", UNWRAP);
    commit(d, "Agent", "unwrap", T0 + 60);
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(
        blocked(&stop(d, true)),
        "its own failure: second strike blocks"
    );
}

/// Second review B1: `--force-warn` keeps a lint a warning under `-D warnings`.
#[test]
fn force_warn_in_a_hidden_config_fails_the_gate() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", UNWRAP);
    write(d, ".git/info/exclude", "/.cargo\n");
    write(
        d,
        ".cargo/config.toml",
        "[build]\nrustflags = [\"--force-warn\", \"clippy::unwrap_used\"]\n",
    );
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("allow-flag"), "{v}");
}

/// `RUSTFMT` pointing at `true` made the fmt step pass; it is removed like the others.
#[test]
fn a_fake_rustfmt_from_the_environment_is_ignored() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", &GOOD.replace("a + b", "a   +   b"));
    let v = hook_env(
        d,
        "stop",
        &json!({"session_id": "s1", "stop_hook_active": false}),
        &[("RUSTFMT", "true")],
    );
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("## fmt"), "{v}");
}

/// When git cannot fingerprint the tree, the gate still runs (it used to report "did not
/// run" and let the stop through).
#[test]
fn the_gate_runs_when_git_cannot_read_the_repository() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    let config = std::fs::read_to_string(d.join(".git/config")).unwrap();
    write(d, ".git/config", &format!("{config}[core\n"));
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("adds"), "{v}");
}

/// The pass report covers new untracked files and manifest settings that switch checks
/// off (second review M4 and M9).
#[test]
fn passing_changes_report_untracked_suppressions_and_manifest_settings() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", &format!("mod extra;\n{GOOD}"));
    write(d, "src/extra.rs", "#[allow(dead_code)]\nfn unused() {}\n");
    write(
        d,
        "Cargo.toml",
        &format!(
            "{}\n[profile.dev]\ndebug-assertions = false\n",
            CARGO_TOML.replace("unwrap_used = \"warn\"", "unwrap_used = 'allow'")
        ),
    );
    let v = stop(d, false);
    assert!(!blocked(&v), "{v}");
    let msg = v["systemMessage"].as_str().unwrap();
    assert!(msg.contains("allow (src/extra.rs)"), "{msg}");
    assert!(msg.contains("lint allowed in a manifest"), "{msg}");
    assert!(msg.contains("checks switched off in a manifest"), "{msg}");
}

/// A post-edit check waits for one already running instead of skipping the edit.
#[test]
fn post_edit_waits_for_a_running_check() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    let lock = d.join(".git/fsmap-harness/check.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::fs::remove_file(lock).unwrap();
    });
    write(d, "src/lib.rs", TYPE_ERROR);
    let v = hook(
        d,
        "post-edit",
        &json!({"session_id": "s1", "tool_input": {"file_path": d.join("src/lib.rs")}}),
    );
    release.join().unwrap();
    assert!(blocked(&v), "{v}");
}

/// Final review M2: a session that starts on a broken commit changed nothing, so its turn
/// may end, but pushing that commit is still gated.
#[test]
fn pushing_a_broken_commit_from_a_fresh_session_is_denied() {
    let repo = fixture();
    let d = repo.path();
    write(d, "src/lib.rs", FAILING_TEST);
    commit(d, "A", "broken", T0 + 10);
    start(d, "startup");
    assert!(stop(d, false).is_null(), "the session changed nothing");
    assert_eq!(push(d)["hookSpecificOutput"]["permissionDecision"], "deny");
}

/// Final review B2: when cargo cannot read the project, the gate cannot run, and that
/// must not look like a pass.
#[test]
fn a_project_cargo_cannot_read_blocks_instead_of_passing() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    write(d, "Cargo.toml", &format!("{CARGO_TOML}this is not toml\n"));
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("cannot run"), "{v}");
    assert_eq!(push(d)["hookSpecificOutput"]["permissionDecision"], "deny");
}

/// Final review B2: a malformed verify table used to drop every extra step silently in
/// the hooks. It is an error for the hooks and the CLI alike.
#[test]
fn a_malformed_verify_table_is_an_error() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(
        d,
        "Cargo.toml",
        &format!("{CARGO_TOML}\n[package.metadata.fsmap.verify]\nextra = [[\"false\"], []]\n"),
    );
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("extra"), "{v}");
    assert_eq!(verify(d, &[]).0, Some(2));
    write(
        d,
        "Cargo.toml",
        &format!("{CARGO_TOML}\n[package.metadata.fsmap.verify]\nextra = \"false\"\n"),
    );
    assert_eq!(verify(d, &[]).0, Some(2));
}

/// The fingerprint covers configuration git cannot see: a hidden cargo config added after
/// a pass makes the gate run again (and its integrity step fail). The setting here breaks
/// no other step, so the integrity failure is the only one, and it must keep all three
/// strikes: a weakening setting is never "someone else's failure".
#[test]
fn a_hidden_config_added_after_a_pass_reruns_the_gate() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", &GOOD.replace("a + b", "b + a"));
    assert!(!blocked(&stop(d, false)));
    write(d, ".git/info/exclude", "/.cargo\n");
    write(d, ".cargo/config.toml", "[env]\nINSTA_FORCE_PASS = \"1\"\n");
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("## integrity") && !reason.contains("## clippy"),
        "{reason}"
    );
    assert!(
        blocked(&stop(d, true)),
        "integrity failures are not released early"
    );
}

/// Final review: the end-to-end test of RUSTFLAGS did not prove that runners are removed.
/// Here a runner that exits 0 would make a failing test pass.
#[cfg(unix)]
#[test]
fn a_runner_from_the_environment_cannot_pass_failing_tests() {
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    write(d, "src/lib.rs", FAILING_TEST);
    let var = format!(
        "CARGO_TARGET_{}_RUNNER",
        host_triple().to_uppercase().replace(['-', '.'], "_")
    );
    let v = hook_env(
        d,
        "stop",
        &json!({"session_id": "s1", "stop_hook_active": false}),
        &[(var.as_str(), "true")],
    );
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("adds"), "{v}");
}

/// Final review B1: rustup obeys a toolchain `path` as soon as cargo runs, so the check
/// must come before the gate's first cargo call. The fake toolchain's cargo would leave a
/// marker (with rustup installed; without it the file is ignored either way).
#[cfg(unix)]
#[test]
fn a_toolchain_path_is_refused_before_any_cargo_runs() {
    use std::os::unix::fs::PermissionsExt;
    let repo = fixture();
    let d = repo.path();
    start(d, "startup");
    let tc = tempfile::tempdir().unwrap();
    let marker = tc.path().join("ran");
    for tool in ["cargo", "rustc"] {
        let bin = tc.path().join("bin").join(tool);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        d,
        "rust-toolchain.toml",
        &format!("[toolchain]\npath = \"{}\"\n", tc.path().display()),
    );
    write(d, ".git/info/exclude", "/rust-toolchain.toml\n");
    let v = stop(d, false);
    assert!(blocked(&v), "{v}");
    assert!(v["reason"].as_str().unwrap().contains("toolchain"), "{v}");
    assert!(!marker.exists(), "the repository's toolchain ran");
}

/// `fsmap verify` itself: steps, JSON report and exit codes (0 pass, 1 failure, 2 error).
#[test]
fn verify_cli_reports_steps_and_exit_codes() {
    let repo = fixture();
    let d = repo.path();
    let (code, out) = verify(d, &["--json"]);
    assert_eq!(code, Some(0), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["integrity", "fmt", "clippy", "test"]);
    write(d, "src/lib.rs", FAILING_TEST);
    let (code, out) = verify(d, &["--json"]);
    assert_eq!(code, Some(1));
    let v: Value = serde_json::from_str(&out).unwrap();
    let test = &v["steps"][3];
    assert_eq!(test["status"], "fail");
    assert!(test["tail"].as_str().unwrap().contains("adds"), "{test}");
    write(d, "src/lib.rs", GOOD);
    write(
        d,
        "Cargo.toml",
        &format!(
            "{CARGO_TOML}\n[package.metadata.fsmap.verify]\nextra = [[\"definitely-not-a-command-xyz\"]]\n"
        ),
    );
    let (code, out) = verify(d, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("could not start"), "{out}");
}

/// A step that overruns the budget is killed, and the report says so.
#[cfg(unix)]
#[test]
fn verify_cli_enforces_its_time_budget() {
    let repo = fixture();
    let d = repo.path();
    write(
        d,
        "Cargo.toml",
        &format!(
            "{CARGO_TOML}\n[package.metadata.fsmap.verify]\nextra = [[\"sh\", \"-c\", \"sleep 60\"]]\n"
        ),
    );
    let started = std::time::Instant::now();
    let (code, out) = verify(d, &["--timeout", "25"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(40),
        "{:?}",
        started.elapsed()
    );
    assert!(
        out.contains("killed after") || out.contains("time budget"),
        "{out}"
    );
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

/// The shipped settings with fsmap missing, or too old to have `fsmap hook` (an older
/// install answers with a usage error and exit 2, which Claude Code would treat as
/// "block": final review M3). Session start says the harness is OFF; every other hook
/// exits 0 and stays out of the way.
#[cfg(unix)]
#[test]
fn template_hooks_tolerate_a_missing_or_old_fsmap() {
    use std::os::unix::fs::PermissionsExt;
    let settings: Value = serde_json::from_str(include_str!("../harness/settings.json")).unwrap();
    let command = |event: &str| {
        settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let old = tempfile::tempdir().unwrap();
    let fake = old.path().join("fsmap");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho \"error: unrecognized subcommand 'hook'\" >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let crashing = tempfile::tempdir().unwrap();
    let fake = crashing.path().join("fsmap");
    std::fs::write(&fake, "#!/bin/sh\nkill -9 $$\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let crash_path = format!("{}:/usr/bin:/bin", crashing.path().display());
    for event in ["UserPromptSubmit", "PostToolUse", "PreToolUse", "Stop"] {
        let out = Command::new("sh")
            .args(["-c", &command(event)])
            .env("PATH", &crash_path)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{event}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            v["systemMessage"]
                .as_str()
                .unwrap()
                .contains("failed (exit 137)"),
            "{event}: {v}"
        );
    }
    for path in [
        "/usr/bin:/bin".to_string(),
        format!("{}:/usr/bin:/bin", old.path().display()),
    ] {
        let run = |cmd: &str| {
            let mut child = Command::new("sh")
                .args(["-c", cmd])
                .env("PATH", &path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            // A hook that exits without reading its input closes the pipe first.
            let _ = child
                .stdin
                .take()
                .unwrap()
                .write_all(br#"{"session_id":"s1","tool_input":{"command":"git push"}}"#);
            child.wait_with_output().unwrap()
        };
        let out = run(&command("SessionStart"));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            v["systemMessage"].as_str().unwrap().contains("OFF"),
            "{path}"
        );
        assert!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("cargo clippy")
        );
        for event in ["UserPromptSubmit", "PostToolUse", "PreToolUse", "Stop"] {
            let out = run(&command(event));
            assert!(out.status.success(), "{event} with PATH={path}");
            assert!(out.stdout.is_empty(), "{event} with PATH={path}");
        }
    }
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
}
