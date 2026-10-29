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
            add(dir.join(rel), *kind, false);
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
pub fn scan(root: &Path, trust: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in configs(root) {
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
                    "a clippy configuration git does not track".into(),
                );
            }
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&c.path) else {
            continue;
        };
        match (c.kind, text.parse::<toml::Table>()) {
            (Kind::Cargo, Ok(t)) => cargo_findings(&t, &mut push),
            (Kind::Rustfmt, Ok(t)) => rustfmt_findings(&t, &mut push),
            (Kind::Toolchain, Ok(t)) => toolchain_findings(&t, &mut push),
            // The legacy `rust-toolchain` file may hold just a channel name.
            (Kind::Toolchain, Err(_)) if !text.trim().contains(['=', '[']) => {
                channel_finding(text.trim(), &mut push);
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
                Ok(t) => toolchain_findings(&t, &mut push),
                Err(_) if !text.trim().contains(['=', '[']) => {
                    channel_finding(text.trim(), &mut push);
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
    "source",
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
/// Subcommands the gate runs that an `[alias]` could replace (cargo ignores aliases of
/// its built-in commands, but `fmt`, `clippy` and `deny` are external).
const GATE_SUBCOMMANDS: &[&str] = &["fmt", "clippy", "deny", "test", "check", "build"];

fn cargo_findings(t: &toml::Table, push: &mut Push<'_>) {
    for (key, v) in t {
        match key.as_str() {
            "build" => {
                for (k, v) in entries(v) {
                    let path = format!("build.{k}");
                    match k.as_str() {
                        "rustflags" | "rustdocflags" => flag_findings(&path, v, push),
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
            "env" => push("env", show("env", v)),
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
    "opt-level",
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
        } else if let Some((_, n)) = [("-W", "--warn"), ("-D", "--deny"), ("-F", "--forbid")]
            .into_iter()
            .find_map(|(s, l)| split(s, l))
            .or_else(|| split("", "--force-warn"))
        {
            kind = None;
            used_next = n;
        } else if let Some((opt, n)) = split("-C", "--codegen") {
            let name = opt.split('=').next().unwrap_or("");
            kind = if SAFE_CODEGEN.contains(&name) {
                None
            } else if LINK_CODEGEN.contains(&name) {
                Some("linker")
            } else {
                Some("rustflag")
            };
            used_next = n;
        } else if let Some((_, n)) =
            split("-l", "--link-lib").or_else(|| split("-L", "--library-path"))
        {
            kind = Some("linker");
            used_next = n;
        } else if let Some((_, n)) = split("", "--cfg") {
            kind = Some("rustflag");
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

fn toolchain_findings(t: &toml::Table, push: &mut Push<'_>) {
    let Some(tc) = t.get("toolchain").and_then(toml::Value::as_table) else {
        return;
    };
    if let Some(p) = tc.get("path") {
        // rustup runs `cargo`, `rustc` and `clippy-driver` from that directory.
        push("toolchain-path", show("toolchain.path", p));
    }
    if let Some(c) = tc.get("channel").and_then(toml::Value::as_str) {
        channel_finding(c, push);
    }
}

/// A channel that is not an official one names a toolchain linked with
/// `rustup toolchain link`: any directory of binaries.
fn channel_finding(channel: &str, push: &mut Push<'_>) {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(stable|beta|nightly|\d+\.\d+(\.\d+)?)(-\d{4}-\d{2}-\d{2})?(-[a-z0-9_]+(-[a-z0-9_.]+)*)?$",
        )
        .expect("a valid pattern")
    });
    if !RE.is_match(channel) {
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

    fn kinds(root: &Path) -> Vec<String> {
        let mut k: Vec<String> = scan(root, &[]).into_iter().map(|f| f.kind).collect();
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
        let trusted = scan(r, &["runner".into(), "env".into()]);
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
             [profile.dev]\nopt-level = 1\n[source.crates-io]\nreplace-with = \"v\"\n",
        );
        write(r, "rustfmt.toml", "max_width = 100\nignore = []\n");
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.97.0\"\n",
        );
        assert!(scan(r, &[]).is_empty(), "{:?}", scan(r, &[]));
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly-2026-01-01\"\n",
        );
        assert!(scan(r, &[]).is_empty(), "{:?}", scan(r, &[]));
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
        ];
        assert_eq!(
            weakening_env(vars.iter().copied(), &["env:RUSTC_WRAPPER".into()]),
            [
                "CARGO_ALIAS_CLIPPY",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                "RUSTFLAGS",
                "RUSTUP_TOOLCHAIN"
            ]
        );
    }
}
