//! Configuration and environment that can make the gate pass without the code changing
//! (decision D-15). Reproduced in reviews: `--cap-lints allow` plus `runner = "true"` in a
//! git-ignored `.cargo/config.toml` turned a failing `clippy -D warnings` and a failing
//! test into exit code 0; so did an `[alias]` shadowing `cargo clippy`, a fake linker, a
//! nested `rustfmt.toml` and a toolchain `path`.
//!
//! The files are parsed as TOML and judged against an **allowlist**: cargo keys that cannot
//! change what fmt, clippy or the tests report pass; known weakening settings get a named
//! kind; anything else is reported as `config:<table>`. A repository that needs one of them
//! trusts its kind in `[package.metadata.fsmap.verify] trust = [...]`. Variables that do the
//! same from the environment are removed before every step.
//!
//! A guardrail against an agent's shortcuts, not a sandbox: `PATH`, the installed toolchain
//! and the user's own settings outside these files are trusted.

use std::path::{Path, PathBuf};

use crate::git;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Finding {
    /// What the setting does, which is also the word that trusts it: `runner`, `linker`,
    /// `rustc-wrapper`, `cap-lints`, `allow-flag`, `rustflag`, `env`, `alias`,
    /// `config:<table>`, `fmt-disabled`, `fmt-ignore`, `untracked-clippy-config`,
    /// `toolchain-path`, `toolchain-custom`, `unreadable`.
    pub kind: String,
    /// The file, relative to the root when inside it.
    pub file: String,
    /// The setting as `dotted.key = value`, cut at 160 characters.
    pub text: String,
}

/// Environment variables removed before running a gate step unless trusted as
/// `env:NAME`: they change how code is compiled, linted or tested, or let a test pass
/// by rewriting its expectations.
const WEAKENING_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTDOC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "CLIPPY_CONF_DIR",
    "CLIPPY_ARGS",
    "INSTA_FORCE_PASS",
    "INSTA_UPDATE",
    "UPDATE_EXPECT",
    "TRYBUILD",
    "TRYCMD",
    "SNAPSHOTS",
    "RUSTFMT",
];
/// Prefixes of cargo's environment configuration: build settings, target runners,
/// linkers and flags, aliases, profiles and unstable options.
const WEAKENING_ENV_PREFIXES: &[&str] = &[
    "CARGO_BUILD_",
    "CARGO_TARGET_",
    "CARGO_ALIAS_",
    "CARGO_PROFILE_",
    "CARGO_UNSTABLE_",
    "CARGO_ENCODED_",
    "__CARGO_",
];

/// Names of weakening variables set in `vars`, minus trusted ones. `CARGO_TARGET_DIR` is
/// not among them: the gate sets it itself.
pub fn weakening_env<'a>(vars: impl Iterator<Item = &'a str>, trust: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vars
        .filter(|k| {
            WEAKENING_ENV.contains(k) || WEAKENING_ENV_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .filter(|k| *k != "CARGO_TARGET_DIR")
        .filter(|k| !trust.iter().any(|t| t.strip_prefix("env:") == Some(*k)))
        .map(String::from)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Cargo,
    Rustfmt,
    Clippy,
    Toolchain,
}

#[derive(Debug)]
struct ConfigFile {
    path: PathBuf,
    kind: Kind,
    /// Inside the repository but not tracked by git.
    untracked: bool,
}

const ANCESTOR_FILES: &[(&str, Kind)] = &[
    (".cargo/config.toml", Kind::Cargo),
    (".cargo/config", Kind::Cargo),
    ("rustfmt.toml", Kind::Rustfmt),
    (".rustfmt.toml", Kind::Rustfmt),
    ("clippy.toml", Kind::Clippy),
    (".clippy.toml", Kind::Clippy),
    ("rust-toolchain.toml", Kind::Toolchain),
    ("rust-toolchain", Kind::Toolchain),
];

/// Every existing configuration file that cargo, rustfmt, clippy or rustup can read for
/// `root`: the root's ancestors, `CARGO_HOME`, rustfmt's user configuration, and every
/// `rustfmt.toml`/`clippy.toml` inside the tree (rustfmt looks upward from each source
/// file), git-ignored ones included. Hashed into the stop gate's fingerprint.
pub fn config_files(root: &Path) -> Vec<PathBuf> {
    configs(root).into_iter().map(|c| c.path).collect()
}

/// Whether an in-tree rustfmt or clippy file at `dir` can apply to the workspace
/// members: it sits at or above a member directory, or inside one without another package
/// in between (a vendored crate's own `clippy.toml` cannot; final review M10).
fn applies_to_members(dir: &Path, members: &[PathBuf]) -> bool {
    members.iter().any(|m| {
        m.starts_with(dir)
            || (dir.starts_with(m)
                && !dir
                    .ancestors()
                    .take_while(|d| *d != m.as_path())
                    .any(|d| d.join("Cargo.toml").is_file()))
    })
}

fn configs(root: &Path) -> Vec<ConfigFile> {
    let mut out: Vec<ConfigFile> = Vec::new();
    let mut add = |path: PathBuf, kind: Kind, untracked: bool| {
        if path.is_file() {
            out.push(ConfigFile {
                path,
                kind,
                untracked,
            });
        }
    };
    for dir in root.ancestors() {
        for (rel, kind) in ANCESTOR_FILES {
            // Above the root no git tracks a clippy file for this repository.
            add(dir.join(rel), *kind, dir != root && *kind == Kind::Clippy);
        }
    }
    if let Some(home) = cargo_home() {
        add(home.join("config.toml"), Kind::Cargo, false);
        add(home.join("config"), Kind::Cargo, false);
    }
    for dir in user_config_dirs() {
        add(dir.join("rustfmt/rustfmt.toml"), Kind::Rustfmt, false);
        add(dir.join("rustfmt/.rustfmt.toml"), Kind::Rustfmt, false);
    }
    for (rel, tracked) in tree_files(root) {
        let kind = if rel.ends_with("clippy.toml") {
            Kind::Clippy
        } else {
            Kind::Rustfmt
        };
        add(root.join(&rel), kind, !tracked);
    }
    // One entry per file; the in-tree listing knows whether git tracks it.
    let mut seen: Vec<(PathBuf, usize)> = Vec::new();
    let mut unique: Vec<ConfigFile> = Vec::new();
    for c in out {
        let key = std::fs::canonicalize(&c.path).unwrap_or_else(|_| c.path.clone());
        if let Some((_, i)) = seen.iter().find(|(k, _)| *k == key) {
            unique[*i].untracked |= c.untracked;
        } else {
            seen.push((key, unique.len()));
            unique.push(c);
        }
    }
    unique
}

/// `rustfmt.toml`, `.rustfmt.toml`, `clippy.toml` and `.clippy.toml` inside the work tree,
/// with whether git tracks them.
fn tree_files(root: &Path) -> Vec<(String, bool)> {
    let patterns = [
        ":(glob)**/rustfmt.toml",
        ":(glob)**/.rustfmt.toml",
        ":(glob)**/clippy.toml",
        ":(glob)**/.clippy.toml",
        ":(exclude)target/",
    ];
    let list = |others: bool| -> Vec<String> {
        git::run({
            let mut c = git::git(root);
            c.args(["ls-files", "-z"]);
            if others {
                // Untracked and ignored files alike: a hidden config is the point.
                c.arg("--others");
            }
            c.arg("--").args(patterns);
            c
        })
        .map(|o| {
            o.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
    };
    let mut out: Vec<(String, bool)> = list(false).into_iter().map(|p| (p, true)).collect();
    out.extend(list(true).into_iter().map(|p| (p, false)));
    out
}

fn rustup_home() -> Option<PathBuf> {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".rustup")))
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cargo")))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where rustfmt looks for a user configuration (`dirs::config_dir()` on each platform).
fn user_config_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(x));
    }
    if let Some(h) = home() {
        out.push(h.join(".config"));
        out.push(h.join("Library/Application Support"));
    }
    if let Some(a) = std::env::var_os("APPDATA") {
        out.push(PathBuf::from(a));
    }
    out
}

/// Settings in the configuration files that weaken the gate, minus trusted kinds.
/// `members` are the workspace members' directories (rustfmt and clippy files that cannot
/// apply to them are not judged).
pub fn scan(root: &Path, members: &[PathBuf], trust: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    let rustup = rustup_home();
    for c in configs(root) {
        if matches!(c.kind, Kind::Rustfmt | Kind::Clippy)
            && c.path.starts_with(root)
            && !c
                .path
                .parent()
                .is_some_and(|d| applies_to_members(d, members))
        {
            continue;
        }
        let shown = c.path.strip_prefix(root).map_or_else(
            |_| c.path.display().to_string(),
            |p| p.display().to_string().replace('\\', "/"),
        );
        let mut push = |kind: &str, text: String| {
            out.push(Finding {
                kind: kind.to_string(),
                file: shown.clone(),
                text: text.chars().take(160).collect(),
            });
        };
        if c.kind == Kind::Clippy {
            if c.untracked {
                push(
                    "untracked-clippy-config",
                    "a clippy configuration git does not track (lint thresholds and allowances)"
                        .into(),
                );
            }
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&c.path) else {
            continue;
        };
        match (c.kind, text.parse::<toml::Table>()) {
            (Kind::Cargo, Ok(t)) => cargo_findings(&t, root, &mut push),
            (Kind::Rustfmt, Ok(t)) => rustfmt_findings(&t, &mut push),
            (Kind::Toolchain, Ok(t)) => toolchain_findings(&t, rustup.as_deref(), &mut push),
            // The legacy `rust-toolchain` file may hold just a channel name.
            (Kind::Toolchain, Err(_)) if !text.trim().contains(['=', '[']) => {
                channel_finding(text.trim(), rustup.as_deref(), &mut push);
            }
            (_, Err(e)) => push("unreadable", format!("not valid TOML: {}", e.message())),
            (Kind::Clippy, Ok(_)) => {}
        }
    }
    out.retain(|f| !trust.contains(&f.kind));
    out
}

/// The toolchain settings in `dir` and its ancestors, which rustup obeys as soon as
/// cargo runs there: checked before the gate runs cargo at all.
pub fn toolchain_scan(dir: &Path, trust: &[String]) -> Vec<Finding> {
    toolchain_scan_with(dir, trust, rustup_home().as_deref())
}

fn toolchain_scan_with(dir: &Path, trust: &[String], rustup: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    for d in dir.ancestors() {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let path = d.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut push = |kind: &str, text: String| {
                out.push(Finding {
                    kind: kind.to_string(),
                    file: path.display().to_string(),
                    text,
                });
            };
            match text.parse::<toml::Table>() {
                Ok(t) => toolchain_findings(&t, rustup, &mut push),
                Err(_) if !text.trim().contains(['=', '[']) => {
                    channel_finding(text.trim(), rustup, &mut push);
                }
                Err(e) => push("unreadable", format!("not valid TOML: {}", e.message())),
            }
        }
    }
    out.retain(|f| !trust.contains(&f.kind));
    out
}

type Push<'a> = dyn FnMut(&str, String) + 'a;

fn show(path: &str, v: &toml::Value) -> String {
    format!("{path} = {v}")
}

fn entries(v: &toml::Value) -> Vec<(&String, &toml::Value)> {
    v.as_table().map(|t| t.iter().collect()).unwrap_or_default()
}

/// Cargo tables that cannot change what the gate's steps report.
const CARGO_SAFE_TABLES: &[&str] = &[
    "cargo-new",
    "http",
    "net",
    "registries",
    "registry",
    "term",
    "install",
    "future-incompat-report",
    "credential-alias",
    "cache",
    "gc",
    "resolver",
    "doc",
];
/// `[build]` keys that cannot change what the gate's steps report (`target-dir` is
/// overridden by the gate).
const BUILD_SAFE_KEYS: &[&str] = &[
    "jobs",
    "incremental",
    "target",
    "target-dir",
    "dep-info-basedir",
    "pipelining",
];
/// Linkers a repository may choose without being trusted, by file name, when not
/// shipped inside the repository itself.
const KNOWN_LINKERS: &[&str] = &[
    "cc", "c++", "gcc", "g++", "clang", "clang++", "ld", "ld.lld", "lld", "rust-lld", "mold",
    "ld.mold", "link.exe", "lld-link",
];
/// Compiler caches allowed as `rustc-wrapper` when named, not shipped in the repository.
const KNOWN_WRAPPERS: &[&str] = &["sccache", "cachepot"];

/// A tool named by file name only (found on `PATH`, which is trusted), or by a path
/// outside the repository, whose file name is in `known`.
fn known_tool(v: &toml::Value, root: &Path, known: &[&str]) -> bool {
    let Some(s) = v.as_str() else {
        return false;
    };
    let path = Path::new(s);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let name = name.strip_suffix(".exe").unwrap_or(name);
    // Cross compilers carry a target prefix (`aarch64-linux-gnu-gcc`).
    let known_name = known.iter().any(|k| {
        *k == name
            || (matches!(*k, "gcc" | "g++" | "clang" | "clang++" | "cc")
                && name.ends_with(&format!("-{k}")))
    });
    known_name
        && (path.components().count() == 1 || (path.is_absolute() && !path.starts_with(root)))
}

/// Subcommands the gate runs that an `[alias]` could replace (cargo ignores aliases of
/// its built-in commands, but `fmt`, `clippy` and `deny` are external).
const GATE_SUBCOMMANDS: &[&str] = &["fmt", "clippy", "deny", "test", "check", "build"];

fn cargo_findings(t: &toml::Table, root: &Path, push: &mut Push<'_>) {
    for (key, v) in t {
        match key.as_str() {
            "build" => {
                for (k, v) in entries(v) {
                    let path = format!("build.{k}");
                    match k.as_str() {
                        "rustflags" | "rustdocflags" => flag_findings(&path, v, push),
                        "rustc-wrapper" | "rustc-workspace-wrapper"
                            if known_tool(v, root, KNOWN_WRAPPERS) => {}
                        "rustc" | "rustc-wrapper" | "rustc-workspace-wrapper" | "rustdoc" => {
                            push("rustc-wrapper", show(&path, v));
                        }
                        k if BUILD_SAFE_KEYS.contains(&k) => {}
                        _ => push("config:build", show(&path, v)),
                    }
                }
            }
            "target" => {
                for (triple, tv) in entries(v) {
                    for (k, v) in entries(tv) {
                        let path = format!("target.{triple}.{k}");
                        match k.as_str() {
                            "runner" => push("runner", show(&path, v)),
                            "linker" if known_tool(v, root, KNOWN_LINKERS) => {}
                            "linker" | "ar" => push("linker", show(&path, v)),
                            "rustflags" | "rustdocflags" => flag_findings(&path, v, push),
                            // `[target.<triple>.<lib>]` build-script overrides.
                            _ => push("config:target", show(&path, v)),
                        }
                    }
                }
            }
            "alias" => {
                for (name, v) in entries(v) {
                    if GATE_SUBCOMMANDS.contains(&name.as_str()) {
                        push("alias", show(&format!("alias.{name}"), v));
                    }
                }
            }
            // Ordinary variables (`SQLX_OFFLINE`) are fine; the ones the gate removes from
            // its own environment are not, set here either.
            "env" => {
                for (name, v) in entries(v) {
                    if !weakening_env(std::iter::once(name.as_str()), &[]).is_empty() {
                        push("env", show(&format!("env.{name}"), v));
                    }
                }
            }
            // Source replacement (`cargo vendor`) can swap in modified dependencies.
            "source" => push("source", show("source", v)),
            "profile" => profile_findings("profile", v, push),
            k if CARGO_SAFE_TABLES.contains(&k) => {}
            k => push(&format!("config:{k}"), show(k, v)),
        }
    }
}

/// Profiles may tune optimisation and debug info, but not switch off the checks that
/// tests rely on.
fn profile_findings(path: &str, v: &toml::Value, push: &mut Push<'_>) {
    for (k, v) in entries(v) {
        let p = format!("{path}.{k}");
        match k.as_str() {
            "debug-assertions" | "overflow-checks" => push("config:profile", show(&p, v)),
            _ if v.is_table() => profile_findings(&p, v, push),
            _ => {}
        }
    }
}

/// `-C` options that cannot make failing code pass.
const SAFE_CODEGEN: &[&str] = &[
    "target-cpu",
    "target-feature",
    "debuginfo",
    "force-frame-pointers",
    "force-unwind-tables",
    "codegen-units",
    "incremental",
    "split-debuginfo",
    "strip",
    "symbol-mangling-version",
    "relocation-model",
    "code-model",
    "embed-bitcode",
    "lto",
    "panic",
    "prefer-dynamic",
    "metadata",
    "extra-filename",
    "instrument-coverage",
    "save-temps",
];
/// `-C` options that choose or drive the linker, which can replace the test binary.
const LINK_CODEGEN: &[&str] = &[
    "linker",
    "link-arg",
    "link-args",
    "linker-flavor",
    "link-self-contained",
    "linker-plugin-lto",
    "link-dead-code",
];

/// Judge a `rustflags` value (an array, or a string split at whitespace).
fn flag_findings(path: &str, v: &toml::Value, push: &mut Push<'_>) {
    let tokens: Vec<String> = match v {
        toml::Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), String::from))
            .collect(),
        toml::Value::String(s) => s.split_whitespace().map(String::from).collect(),
        other => vec![other.to_string()],
    };
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        // (option, its value, whether the value was the next token)
        let split = |short: &str, long: &str| -> Option<(String, bool)> {
            if t == short || t == long {
                Some((next.unwrap_or("").to_string(), true))
            } else if let Some(v) = t.strip_prefix(&format!("{long}=")) {
                Some((v.to_string(), false))
            } else if !short.is_empty() && t.len() > short.len() && t.starts_with(short) {
                Some((t[short.len()..].to_string(), false))
            } else {
                None
            }
        };
        let shown = format!(
            "{path}: {t}{}",
            next.map(|n| format!(" {n}")).unwrap_or_default()
        );
        let kind: Option<&str>;
        let used_next;
        if let Some((_, n)) = split("", "--cap-lints") {
            kind = Some("cap-lints");
            used_next = n;
        } else if let Some((_, n)) = split("-A", "--allow") {
            kind = Some("allow-flag");
            used_next = n;
        } else if let Some((_, n)) = split("", "--force-warn") {
            // A force-warned lint stays a warning under `-D warnings` (final review B1).
            kind = Some("allow-flag");
            used_next = n;
        } else if let Some((_, n)) = [("-W", "--warn"), ("-D", "--deny"), ("-F", "--forbid")]
            .into_iter()
            .find_map(|(s, l)| split(s, l))
        {
            kind = None;
            used_next = n;
        } else if let Some((opt, n)) = split("-C", "--codegen") {
            let (name, value) = opt.split_once('=').unwrap_or((opt.as_str(), ""));
            let on = matches!(value, "on" | "yes" | "y" | "true" | "");
            kind = match name {
                // Above 0, debug assertions and overflow checks default to off (final
                // review M1): failing `debug_assert!`s and overflows then pass.
                "opt-level" if value == "0" => None,
                "debug-assertions" | "overflow-checks" if on => None,
                "link-arg" if is_known_fuse_ld(value) => None,
                n if SAFE_CODEGEN.contains(&n) => None,
                n if LINK_CODEGEN.contains(&n) => Some("linker"),
                _ => Some("rustflag"),
            };
            used_next = n;
        } else if let Some((_, n)) =
            split("-l", "--link-lib").or_else(|| split("-L", "--library-path"))
        {
            kind = Some("linker");
            used_next = n;
        } else if let Some((_, n)) = split("", "--cfg") {
            // `--cfg tokio_unstable` and the like; code gated on a cfg is in the diff.
            kind = None;
            used_next = n;
        } else {
            kind = Some("rustflag");
            used_next = false;
        }
        if let Some(k) = kind {
            push(k, shown);
        }
        i += if used_next { 2 } else { 1 };
    }
}

/// `-fuse-ld=` with a known linker (mold, lld, gold, bfd).
fn is_known_fuse_ld(arg: &str) -> bool {
    arg.strip_prefix("-fuse-ld=")
        .is_some_and(|l| matches!(l, "mold" | "lld" | "gold" | "bfd" | "ld.mold" | "ld.lld"))
}

fn rustfmt_findings(t: &toml::Table, push: &mut Push<'_>) {
    if t.get("disable_all_formatting")
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        push("fmt-disabled", "disable_all_formatting = true".into());
    }
    if let Some(v) = t.get("ignore")
        && v.as_array().is_none_or(|a| !a.is_empty())
    {
        push("fmt-ignore", show("ignore", v));
    }
    if t.get("skip_children").and_then(toml::Value::as_bool) == Some(true) {
        push("fmt-ignore", "skip_children = true".into());
    }
}

fn toolchain_findings(t: &toml::Table, rustup: Option<&Path>, push: &mut Push<'_>) {
    let Some(tc) = t.get("toolchain").and_then(toml::Value::as_table) else {
        return;
    };
    if let Some(p) = tc.get("path") {
        // rustup runs `cargo`, `rustc` and `clippy-driver` from that directory.
        push("toolchain-path", show("toolchain.path", p));
    }
    if let Some(c) = tc.get("channel").and_then(toml::Value::as_str) {
        channel_finding(c, rustup, push);
    }
}

/// A channel that is not an official one names a toolchain linked with
/// `rustup toolchain link`: any directory of binaries. rustup also accepts official-looking
/// link names (`stable-evil`), so a channel whose toolchain directory is a link is refused
/// too (final review M5).
fn channel_finding(channel: &str, rustup: Option<&Path>, push: &mut Push<'_>) {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(stable|beta|nightly|\d+\.\d+(\.\d+)?)(-\d{4}-\d{2}-\d{2})?(-[a-z0-9_]+(-[a-z0-9_.]+)*)?$",
        )
        .expect("a valid pattern")
    });
    let linked = rustup.is_some_and(|home| {
        std::fs::symlink_metadata(home.join("toolchains").join(channel))
            .is_ok_and(|m| m.file_type().is_symlink())
    });
    if !RE.is_match(channel) || linked {
        push("toolchain-custom", format!("channel = {channel:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Scan with the root as the only workspace member.
    fn sc(root: &Path, trust: &[String]) -> Vec<Finding> {
        scan(root, &[root.to_path_buf()], trust)
    }

    fn kinds(root: &Path) -> Vec<String> {
        let mut k: Vec<String> = sc(root, &[]).into_iter().map(|f| f.kind).collect();
        k.sort();
        k
    }

    #[test]
    fn finds_gate_weakening_settings() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n\
             [target.x86_64-unknown-linux-gnu]\nrunner = \"true\"\n\
             [env]\nINSTA_FORCE_PASS = \"1\"\n# runner = \"commented\"\n",
        );
        write(r, ".rustfmt.toml", "disable_all_formatting = true\n");
        write(r, "rust-toolchain.toml", "[toolchain]\npath = \"/tmp/x\"\n");
        assert_eq!(
            kinds(r),
            [
                "cap-lints",
                "env",
                "fmt-disabled",
                "runner",
                "toolchain-path"
            ]
        );
        let trusted = sc(r, &["runner".into(), "env".into()]);
        assert!(
            trusted
                .iter()
                .all(|f| f.kind != "runner" && f.kind != "env")
        );
    }

    /// The bypasses the final review reproduced against the line-based parser.
    #[test]
    fn bypasses_of_the_line_parser_are_found() {
        let cases: &[(&str, &str, &str)] = &[
            (
                ".cargo/config.toml",
                "[alias]\nclippy = \"test --no-run\"\n",
                "alias",
            ),
            (
                ".cargo/config.toml",
                "[target.x86_64-unknown-linux-gnu]\nlinker = \".cargo/fake-ld.sh\"\n",
                "linker",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"-Cmetadata=#\", \"--cap-lints\", \"allow\"]\n",
                "cap-lints",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cap-\\u006cints\", \"allow\"]\n",
                "cap-lints",
            ),
            (
                ".cargo/config.toml",
                "target.x86_64-unknown-linux-gnu.runner = \"true\"\n",
                "runner",
            ),
            (
                ".cargo/config.toml",
                "build = { rustflags = \"-A warnings\" }\n",
                "allow-flag",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"-C\", \"link-arg=-Wl,-e,exit\"]\n",
                "linker",
            ),
            (
                ".cargo/config.toml",
                "[patch.crates-io]\nfoo = { path = \"x\" }\n",
                "config:patch",
            ),
            (
                "rust-toolchain.toml",
                "toolchain.path = \"/abs/tc\"\n",
                "toolchain-path",
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"evil\"\n",
                "toolchain-custom",
            ),
            (
                "rust-toolchain",
                "my-linked-toolchain\n",
                "toolchain-custom",
            ),
        ];
        for (file, text, kind) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), file, text);
            assert_eq!(kinds(dir.path()), [*kind], "{file}: {text}");
        }
    }

    /// rustfmt reads configuration from each source file's directory upward, so a nested
    /// file counts, tracked or not.
    #[test]
    fn nested_rustfmt_and_untracked_clippy_config_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(r)
            .status()
            .unwrap();
        write(r, "src/rustfmt.toml", "disable_all_formatting = true\n");
        write(
            r,
            "crates/a/clippy.toml",
            "too-many-arguments-threshold = 99\n",
        );
        assert_eq!(kinds(r), ["fmt-disabled", "untracked-clippy-config"]);
        std::process::Command::new("git")
            .args(["add", "crates/a/clippy.toml"])
            .current_dir(r)
            .status()
            .unwrap();
        assert_eq!(kinds(r), ["fmt-disabled"]);
    }

    #[test]
    fn benign_config_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            "[build]\njobs = 4\nrustflags = [\"-C\", \"target-cpu=native\", \"-Dwarnings\"]\n\
             [net]\nretry = 3\n[alias]\nxtask = \"run -p xtask --\"\n\
             [profile.dev]\nopt-level = 1\n",
        );
        write(r, "rustfmt.toml", "max_width = 100\nignore = []\n");
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.97.0\"\n",
        );
        assert!(sc(r, &[]).is_empty(), "{:?}", sc(r, &[]));
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly-2026-01-01\"\n",
        );
        assert!(sc(r, &[]).is_empty(), "{:?}", sc(r, &[]));
    }

    /// The second implementation review: settings the allowlist wrongly called safe.
    #[test]
    fn second_review_bypasses_are_found() {
        let cases: &[(&str, &str)] = &[
            (
                "[build]\nrustflags = [\"--force-warn\", \"clippy::unwrap_used\"]\n",
                "allow-flag",
            ),
            (
                "[build]\nrustflags = [\"-C\", \"opt-level=1\"]\n",
                "rustflag",
            ),
            (
                "[build]\nrustflags = [\"-Cdebug-assertions=off\"]\n",
                "rustflag",
            ),
            (
                "[source.crates-io]\nreplace-with = \"v\"\n[source.v]\ndirectory = \"v\"\n",
                "source",
            ),
            ("[env]\nINSTA_FORCE_PASS = \"1\"\n", "env"),
            (
                "[target.x86_64-unknown-linux-gnu]\nlinker = \".cargo/fake-ld.sh\"\n",
                "linker",
            ),
            (
                "[target.x86_64-unknown-linux-gnu]\nlinker = \"evil-ld\"\n",
                "linker",
            ),
            ("[build]\nrustc-wrapper = \"./wrap.sh\"\n", "rustc-wrapper"),
        ];
        for (text, kind) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".cargo/config.toml", text);
            assert_eq!(kinds(dir.path()), [*kind], "{text}");
        }
    }

    /// Common legitimate settings stay quiet: mold, sccache, tokio_unstable, ordinary
    /// `[env]` variables, explicit debug assertions.
    #[test]
    fn common_legitimate_settings_pass() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            "[build]\nrustc-wrapper = \"sccache\"\n\
             rustflags = [\"--cfg\", \"tokio_unstable\", \"-C\", \"opt-level=0\", \"-Cdebug-assertions=on\"]\n\
             [target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\n\
             rustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n\
             [env]\nSQLX_OFFLINE = \"true\"\n",
        );
        assert!(sc(r, &[]).is_empty(), "{:?}", sc(r, &[]));
        write(
            r,
            ".cargo/config.toml",
            "[source.crates-io]\nreplace-with = \"v\"\n",
        );
        assert!(sc(r, &["source".into()]).is_empty(), "trusted vendoring");
    }

    /// A clippy file above the repository is untracked by definition (final review M3); a
    /// vendored crate's own clippy file inside the tree cannot apply to the members (M10).
    #[test]
    fn clippy_files_outside_and_in_vendored_crates() {
        let outer = tempfile::tempdir().unwrap();
        let r = outer.path().join("repo");
        std::fs::create_dir_all(&r).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&r)
            .status()
            .unwrap();
        write(
            &r,
            "vendor/base64/Cargo.toml",
            "[package]\nname = \"base64\"\n",
        );
        write(&r, "vendor/base64/clippy.toml", "msrv = \"1.48\"\n");
        assert!(kinds(&r).is_empty(), "{:?}", kinds(&r));
        write(
            outer.path(),
            "clippy.toml",
            "too-many-arguments-threshold = 99\n",
        );
        assert_eq!(kinds(&r), ["untracked-clippy-config"]);
    }

    /// rustup accepts official-looking link names (`stable-evil`); the toolchain
    /// directory being a link gives it away.
    #[cfg(unix)]
    #[test]
    fn linked_toolchains_with_official_looking_names_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let tc = home.path().join("toolchains");
        std::fs::create_dir_all(tc.join("stable-x86_64-unknown-linux-gnu")).unwrap();
        std::os::unix::fs::symlink("/tmp", tc.join("stable-evil")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"stable-evil\"\n",
        );
        let f = toolchain_scan_with(dir.path(), &[], Some(home.path()));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].kind, "toolchain-custom");
        write(
            dir.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"stable-x86_64-unknown-linux-gnu\"\n",
        );
        assert!(toolchain_scan_with(dir.path(), &[], Some(home.path())).is_empty());
    }

    #[test]
    fn toolchain_scan_reads_ancestors_before_cargo_runs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "rust-toolchain.toml",
            "[toolchain]\npath = \"/x\"\n",
        );
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let f = toolchain_scan(&sub, &[]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "toolchain-path");
        assert!(toolchain_scan(&sub, &["toolchain-path".into()]).is_empty());
    }

    #[test]
    fn weakening_env_names() {
        let vars = [
            "PATH",
            "RUSTFLAGS",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "CARGO_ALIAS_CLIPPY",
            "RUSTUP_TOOLCHAIN",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
            "CARGO_HOME",
            "RUSTFMT",
        ];
        assert_eq!(
            weakening_env(vars.iter().copied(), &["env:RUSTC_WRAPPER".into()]),
            [
                "CARGO_ALIAS_CLIPPY",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                "RUSTFLAGS",
                "RUSTFMT",
                "RUSTUP_TOOLCHAIN"
            ]
        );
    }
}
