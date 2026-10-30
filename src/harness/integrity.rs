//! Configuration and environment that can make the gate pass without the code changing
//! (decision D-15). Reproduced in reviews: `--cap-lints allow` plus `runner = "true"` in a
//! git-ignored `.cargo/config.toml` turned a failing `clippy -D warnings` and a failing
//! test into exit code 0; so did an `[alias]` shadowing `cargo clippy`, a fake linker,
//! `--force-warn`, `[env] LD_PRELOAD`, a nested `rustfmt.toml` and a toolchain override.
//!
//! What is judged follows the threat model in `docs/agent-harness.md`: configuration that is
//! **hidden** from the repository's history makes the local gate disagree with CI, so it is
//! judged here. Files git tracks are in the diff and CI reads them too: they are not judged
//! (the stop hook reports changes to them). Hidden files are parsed as TOML and judged
//! against an **allowlist**: settings that cannot change what fmt, clippy or the tests report
//! pass, known shortcuts get a named kind, anything else is `config:<table>`. Inside the
//! repository nothing hidden can be trusted; outside it (the developer's own environment) a
//! repository may trust a kind in `[package.metadata.fsmap.verify] trust = [...]`.
//! Environment variables are judged by name and value, and the weakening ones are removed
//! before every step.
//!
//! A guardrail against an agent's shortcuts, not a sandbox: `PATH` and the installed
//! toolchain are trusted.

use std::path::{Path, PathBuf};

use crate::git;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Finding {
    /// What the setting does, which is also the word that trusts it: `runner`, `linker`,
    /// `rustc-wrapper`, `cap-lints`, `allow-flag`, `rustflag`, `env`, `alias`, `source`,
    /// `config:<table>`, `fmt-disabled`, `fmt-ignore`, `untracked-rustfmt-config`,
    /// `untracked-clippy-config`, `rustfmt-outside`, `toolchain-path`, `toolchain-custom`,
    /// `unreadable`.
    pub kind: String,
    /// The file, relative to the root when inside it.
    pub file: String,
    /// The setting as `dotted.key = value`, cut at 160 characters.
    pub text: String,
    /// Whether `trust` can accept it: not for a file inside the repository that git does
    /// not track, which only committing (or removing) resolves.
    pub trustable: bool,
}

/// Variables that always change what the steps report: the tools themselves, snapshot
/// "accept everything" switches, property-test case counts, and loader or compiler-driver
/// injection. Removing one restores the default CI runs with.
const ALWAYS_WEAKENING_ENV: &[&str] = &[
    "RUSTC",
    "RUSTDOC",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "RUSTFMT",
    "CLIPPY_CONF_DIR",
    "CLIPPY_ARGS",
    "INSTA_FORCE_PASS",
    "INSTA_UPDATE",
    "UPDATE_EXPECT",
    "UPDATE_GOLDENFILES",
    "EXPECTORATE",
    "TRYBUILD",
    "TRYCMD",
    "SNAPSHOTS",
    "PROPTEST_CASES",
    "QUICKCHECK_TESTS",
    "LD_PRELOAD",
    "LD_AUDIT",
    "COMPILER_PATH",
    "GCC_EXEC_PREFIX",
];

/// Whether setting `name` to `value` can change what the gate's steps report for the
/// repository at `root`. Judged by value where that is safe: `RUSTFLAGS="--cfg
/// tokio_unstable"`, `RUSTC_WRAPPER=sccache` and `CARGO_BUILD_JOBS` stay, `-A warnings` or
/// a runner goes (third review, finding 8).
pub fn env_weakens(name: &str, value: &str, root: &Path) -> bool {
    let flags = |sep: Option<char>| {
        let tokens: Vec<String> = match sep {
            Some(c) => value.split(c).map(String::from).collect(),
            None => value.split_whitespace().map(String::from).collect(),
        };
        !judge_flags(&tokens, Some(root)).is_empty()
    };
    match name {
        "RUSTFLAGS" | "RUSTDOCFLAGS" | "CARGO_BUILD_RUSTFLAGS" | "CARGO_BUILD_RUSTDOCFLAGS" => {
            flags(None)
        }
        "CARGO_ENCODED_RUSTFLAGS" | "CARGO_ENCODED_RUSTDOCFLAGS" => flags(Some('\u{1f}')),
        "RUSTC_WRAPPER"
        | "RUSTC_WORKSPACE_WRAPPER"
        | "CARGO_BUILD_RUSTC_WRAPPER"
        | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER" => {
            !value.is_empty() && !known_name(value, KNOWN_WRAPPERS, Some(root))
        }
        "CARGO_BUILD_JOBS"
        | "CARGO_BUILD_INCREMENTAL"
        | "CARGO_BUILD_TARGET"
        | "CARGO_BUILD_PIPELINING"
        | "CARGO_BUILD_DEP_INFO_BASEDIR"
        | "CARGO_BUILD_TARGET_DIR"
        | "CARGO_TARGET_DIR" => false, // the build directory is set by the gate
        n if ALWAYS_WEAKENING_ENV.contains(&n) => true,
        n if n.starts_with("CARGO_TARGET_") => {
            if n.ends_with("_RUSTFLAGS") || n.ends_with("_RUSTDOCFLAGS") {
                flags(None)
            } else if n.ends_with("_LINKER") {
                !known_name(value, KNOWN_LINKERS, Some(root))
            } else {
                true // runners and the rest
            }
        }
        n if n.starts_with("CARGO_PROFILE_") => {
            if n.ends_with("_DEBUG_ASSERTIONS") || n.ends_with("_OVERFLOW_CHECKS") {
                value != "true"
            } else if n.ends_with("_OPT_LEVEL") {
                value != "0"
            } else {
                false
            }
        }
        n => [
            "CARGO_BUILD_",
            "CARGO_ALIAS_",
            "CARGO_UNSTABLE_",
            "__CARGO_",
            "DYLD_",
        ]
        .iter()
        .any(|p| n.starts_with(p)),
    }
}

/// Names of the weakening variables in `vars`, minus those trusted as `env:NAME`.
pub fn weakening_env<'a>(
    vars: impl Iterator<Item = (&'a str, &'a str)>,
    trust: &[String],
    root: &Path,
) -> Vec<String> {
    let mut out: Vec<String> = vars
        .filter(|(k, v)| env_weakens(k, v, root))
        .filter(|(k, _)| !trust.iter().any(|t| t.strip_prefix("env:") == Some(*k)))
        .map(|(k, _)| k.to_string())
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

/// Where a configuration file lives, which decides how it is judged. Ordered from the
/// least to the most strictly judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    /// Tracked in this repository: in the diff, and CI reads it too. Not judged.
    Tracked,
    /// Inside a project that is not in a git repository: there is no history to hide
    /// from, so it is judged like `Outside`.
    Unversioned,
    /// Outside the repository (ancestors, `CARGO_HOME`, the user's rustfmt configuration):
    /// the developer's environment. Judged; a repository can trust kinds here.
    Outside,
    /// Inside the repository but untracked, ignored, or hidden from `git status`: what CI
    /// never sees. Judged; never trusted.
    Hidden,
}

#[derive(Debug)]
struct ConfigFile {
    path: PathBuf,
    kind: Kind,
    place: Place,
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
/// `rustfmt.toml`/`clippy.toml` inside the tree, git-ignored ones included. Hashed into
/// the stop gate's fingerprint.
pub fn config_files(root: &Path) -> Vec<PathBuf> {
    configs(root).into_iter().map(|c| c.path).collect()
}

/// Whether an in-tree rustfmt or clippy file at `dir` can apply to the workspace
/// members: it sits at or above a member directory, or inside one without another package
/// in between (a vendored crate's own `clippy.toml` cannot; second review M10).
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
    let top = toplevel(root);
    let listed = top.as_deref().map(listed_files).unwrap_or_default();
    // Compared canonically: git prints the toplevel with links resolved (`/private/var` on
    // macOS), while `root` is spelled as given.
    let (croot, ctop) = (canon(root), top.as_deref().map(canon));
    // Where one location is (its directory resolved, the file itself not).
    let place_at = |path: &Path| -> Place {
        let Some(top) = ctop.as_deref() else {
            return if path.starts_with(&croot) {
                Place::Unversioned
            } else {
                Place::Outside
            };
        };
        match path.strip_prefix(top) {
            Err(_) => Place::Outside,
            Ok(rel) => {
                let rel = rel.to_string_lossy().replace('\\', "/");
                match listed.iter().find(|(p, _)| *p == rel) {
                    Some((_, true)) => Place::Tracked,
                    Some((_, false)) => Place::Hidden,
                    // Not a config file name (an included file): ask git about it alone.
                    None => place_of_file(Some(top), path),
                }
            }
        }
    };
    // A link is placed by the stricter of where it is and where it points: git tracks only
    // a link's text, so a tracked link to an ignored file hides its content (fifth review).
    let place_of = |path: &Path| -> Place {
        let here = place_at(&canon_file(path));
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            here.max(place_at(&canon(path)))
        } else {
            here
        }
    };
    let mut candidates: Vec<(PathBuf, Kind)> = Vec::new();
    for dir in root.ancestors() {
        for (rel, kind) in ANCESTOR_FILES {
            candidates.push((dir.join(rel), *kind));
        }
    }
    if let Some(home) = cargo_home() {
        candidates.push((home.join("config.toml"), Kind::Cargo));
        candidates.push((home.join("config"), Kind::Cargo));
    }
    for dir in user_rustfmt_dirs() {
        candidates.push((dir.join("rustfmt.toml"), Kind::Rustfmt));
        candidates.push((dir.join(".rustfmt.toml"), Kind::Rustfmt));
    }
    if let Some(top) = ctop.as_deref() {
        for (rel, _) in &listed {
            let kind = match rel.rsplit('/').next().unwrap_or(rel) {
                "clippy.toml" | ".clippy.toml" => Kind::Clippy,
                "rustfmt.toml" | ".rustfmt.toml" => Kind::Rustfmt,
                _ => continue, // cargo reads its configuration from the root upwards only
            };
            // Spelled under `root` where possible, like the members it is compared with.
            let path = top.join(rel);
            let path = match path.strip_prefix(&croot) {
                Ok(r) => root.join(r),
                Err(_) => path,
            };
            candidates.push((path, kind));
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for (path, kind) in candidates {
        if !path.is_file() {
            continue;
        }
        let key = canon_file(&path);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let place = place_of(&path);
        out.push(ConfigFile { path, kind, place });
    }
    // Files a cargo configuration includes, each placed on its own: a tracked config can
    // include an ignored one (fourth review, M1). Appended entries are followed in turn.
    let mut i = 0;
    while i < out.len() {
        if out[i].kind == Kind::Cargo {
            for path in includes(&out[i].path) {
                let key = canon_file(&path);
                if !path.is_file() || seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let place = place_of(&path);
                out.push(ConfigFile {
                    path,
                    kind: Kind::Cargo,
                    place,
                });
            }
        }
        i += 1;
    }
    out
}

/// The files a cargo configuration file includes (`include = "a.toml"`, a list of paths,
/// or of `{ path, optional }` tables), relative to its directory.
fn includes(config: &Path) -> Vec<PathBuf> {
    let Some(dir) = config.parent() else {
        return Vec::new();
    };
    let table = std::fs::read_to_string(config)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok());
    let Some(v) = table.as_ref().and_then(|t| t.get("include")) else {
        return Vec::new();
    };
    let entries: Vec<&toml::Value> = match v {
        toml::Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    entries
        .into_iter()
        .filter_map(|e| {
            e.as_str()
                .or_else(|| e.get("path").and_then(toml::Value::as_str))
        })
        .map(|p| dir.join(p))
        .collect()
}

/// A file's path with its directory resolved but not the file itself: a link inside the
/// repository is placed where the link is, not where it points (fourth review, m1).
fn canon_file(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(d), Some(n)) => canon(d).join(n),
        _ => canon(p),
    }
}

fn canon(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Where a single file is relative to the project's work tree at `top` (canonical):
/// tracked there, inside it but not tracked, or outside it. Asked of `top` itself, not of
/// the repository nearest the file: a file in an ignored nested repository is hidden from
/// this one (sixth review). `path` has its directory resolved (`canon_file`).
fn place_of_file(top: Option<&Path>, path: &Path) -> Place {
    let Some(rel) = top.and_then(|t| path.strip_prefix(t).ok()) else {
        return Place::Outside;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    match git::run({
        let mut c = git::git(top.unwrap_or(path));
        c.args(["ls-files", "-v", "-z", "--"])
            .arg(format!(":(literal){rel}"));
        c
    }) {
        Ok(o) if o.first().is_some_and(|t| index_tag_tracked(*t)) => Place::Tracked,
        _ => Place::Hidden,
    }
}

fn toplevel(root: &Path) -> Option<PathBuf> {
    git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--show-toplevel"]);
        c
    })
    .ok()
    .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
}

/// Whether a `git ls-files -v` tag means git sees the file's changes: not for
/// skip-worktree (`S`) or assume-unchanged (lower case), which hide them (fourth review,
/// M4).
fn index_tag_tracked(tag: u8) -> bool {
    tag.is_ascii_uppercase() && tag != b'S'
}

/// Configuration files inside the work tree at `top`, relative to it, with whether git
/// tracks them: tracked, untracked and ignored alike (a hidden config is the point).
fn listed_files(top: &Path) -> Vec<(String, bool)> {
    let patterns = [
        ":(glob)**/rustfmt.toml",
        ":(glob)**/.rustfmt.toml",
        ":(glob)**/clippy.toml",
        ":(glob)**/.clippy.toml",
        ":(glob)**/.cargo/config",
        ":(glob)**/.cargo/config.toml",
        ":(glob)**/rust-toolchain",
        ":(glob)**/rust-toolchain.toml",
        ":(exclude)target/",
    ];
    // `-v` prefixes each entry with a tag and a space.
    let list = |others: bool| -> Vec<(String, bool)> {
        git::run({
            let mut c = git::git(top);
            c.args(["ls-files", "-v", "-z"]);
            if others {
                c.arg("--others");
            }
            c.arg("--").args(patterns);
            c
        })
        .map(|o| {
            o.split(|&b| b == 0)
                .filter(|s| s.len() > 2)
                .map(|s| {
                    let tracked = !others && index_tag_tracked(s[0]);
                    (String::from_utf8_lossy(&s[2..]).into_owned(), tracked)
                })
                .collect()
        })
        .unwrap_or_default()
    };
    let mut out = list(false);
    out.extend(list(true));
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

/// Where rustfmt looks when no configuration is found above a file: the home directory
/// and the user configuration directory (`dirs::config_dir()/rustfmt` on each platform).
fn user_rustfmt_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home() {
        out.push(h.clone());
        out.push(h.join(".config/rustfmt"));
        out.push(h.join("Library/Application Support/rustfmt"));
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(x).join("rustfmt"));
    }
    if let Some(a) = std::env::var_os("APPDATA") {
        out.push(PathBuf::from(a).join("rustfmt"));
    }
    out
}

/// The build targets the configuration files and the environment name (`build.target` as
/// a string or a list, `CARGO_BUILD_TARGET`): their `[target.<triple>]` tables apply too
/// (fourth review, M5).
fn build_targets(files: &[ConfigFile]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in files.iter().filter(|c| c.kind == Kind::Cargo) {
        let table = std::fs::read_to_string(&c.path)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok());
        match table.as_ref().and_then(|t| t.get("build")?.get("target")) {
            Some(toml::Value::String(t)) => out.push(t.clone()),
            Some(toml::Value::Array(a)) => {
                out.extend(a.iter().filter_map(|t| t.as_str().map(String::from)));
            }
            _ => {}
        }
    }
    if let Some(v) = std::env::var_os("CARGO_BUILD_TARGET") {
        out.extend(
            v.to_string_lossy()
                .split([',', ' '])
                .filter(|t| !t.is_empty())
                .map(String::from),
        );
    }
    out
}

/// The host triple, for deciding which `[target.<triple>]` tables apply.
fn host_triple(dir: &Path) -> Option<String> {
    let out = super::verify::clean_command(dir, "rustc")
        .arg("-vV")
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(String::from)
}

/// Settings that weaken the gate in hidden configuration (see the module documentation),
/// minus kinds trusted for files outside the repository. `members` are the workspace
/// members' directories.
pub fn scan(root: &Path, members: &[PathBuf], trust: &[String]) -> Vec<Finding> {
    let files = configs(root);
    let members: Vec<PathBuf> = members.iter().map(|m| canon(m)).collect();
    let rustup = rustup_home();
    let host = host_triple(root);
    // rustfmt stops at the first configuration above a file: the project's own one at or
    // above the root means configurations outside the project never apply.
    let own_rustfmt = files.iter().any(|c| {
        c.kind == Kind::Rustfmt
            && matches!(c.place, Place::Tracked | Place::Unversioned)
            && c.path.parent().is_some_and(|d| root.starts_with(d))
    });
    let targets = build_targets(&files);
    let mut out = Vec::new();
    for c in files {
        if c.place == Place::Tracked {
            continue;
        }
        if matches!(c.kind, Kind::Rustfmt | Kind::Clippy)
            && c.place == Place::Hidden
            && !c
                .path
                .parent()
                .is_some_and(|d| applies_to_members(&canon(d), &members))
        {
            continue;
        }
        let shown = c.path.strip_prefix(root).map_or_else(
            |_| c.path.display().to_string(),
            |p| p.display().to_string().replace('\\', "/"),
        );
        let trustable = c.place != Place::Hidden;
        let mut push = |kind: &str, text: String| {
            if !(trustable && trust.iter().any(|t| t == kind)) {
                out.push(Finding {
                    kind: kind.to_string(),
                    file: shown.clone(),
                    text: text.chars().take(160).collect(),
                    trustable,
                });
            }
        };
        let Ok(text) = std::fs::read_to_string(&c.path) else {
            continue;
        };
        match c.kind {
            Kind::Clippy => push(
                "untracked-clippy-config",
                "lint thresholds and allowances git does not track".into(),
            ),
            Kind::Rustfmt => match c.place {
                Place::Hidden => push(
                    "untracked-rustfmt-config",
                    "formatting rules git does not track".into(),
                ),
                Place::Outside if !own_rustfmt => push(
                    "rustfmt-outside",
                    "formatting rules from outside the repository apply to it (CI lacks them)"
                        .into(),
                ),
                Place::Outside | Place::Tracked => {}
                Place::Unversioned => match text.parse::<toml::Table>() {
                    Ok(t) => rustfmt_findings(&t, &mut push),
                    Err(e) => push("unreadable", format!("not valid TOML: {}", e.message())),
                },
            },
            Kind::Cargo => match text.parse::<toml::Table>() {
                Ok(t) => cargo_findings(
                    &t,
                    root,
                    c.place == Place::Hidden,
                    host.as_deref(),
                    &targets,
                    &mut push,
                ),
                Err(e) => push("unreadable", format!("not valid TOML: {}", e.message())),
            },
            Kind::Toolchain if c.place == Place::Hidden => push(
                "untracked-toolchain-config",
                format!("a toolchain pin git does not track: {}", text.trim()),
            ),
            Kind::Toolchain => match text.parse::<toml::Table>() {
                Ok(t) => toolchain_findings(&t, rustup.as_deref(), &mut push),
                // The legacy `rust-toolchain` file may hold just a channel name.
                Err(_) if !text.trim().contains(['=', '[']) => {
                    channel_finding(text.trim(), rustup.as_deref(), &mut push);
                }
                Err(e) => push("unreadable", format!("not valid TOML: {}", e.message())),
            },
        }
    }
    out
}

/// The toolchain settings rustup obeys as soon as cargo runs in `dir`: toolchain files in
/// `dir` and its ancestors that git does not track, and directory overrides
/// (`rustup override set`) in rustup's own settings (third review, finding 2). Checked
/// before the gate runs cargo at all.
pub fn toolchain_scan(dir: &Path, trust: &[String]) -> Vec<Finding> {
    toolchain_scan_with(dir, trust, rustup_home().as_deref())
}

fn toolchain_scan_with(dir: &Path, trust: &[String], rustup: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    let top = toplevel(dir).map(|t| canon(&t));
    for d in dir.ancestors() {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let path = d.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let place = place_of_file(top.as_deref(), &canon_file(&path));
            if place == Place::Tracked {
                continue;
            }
            let mut push = |kind: &str, text: String| {
                out.push(Finding {
                    kind: kind.to_string(),
                    file: path.display().to_string(),
                    text,
                    trustable: place != Place::Hidden,
                });
            };
            if place == Place::Hidden {
                // Even an official channel changes which clippy and rustfmt run.
                push(
                    "untracked-toolchain-config",
                    format!("a toolchain pin git does not track: {}", text.trim()),
                );
                continue;
            }
            match text.parse::<toml::Table>() {
                Ok(t) => toolchain_findings(&t, rustup, &mut push),
                Err(_) if !text.trim().contains(['=', '[']) => {
                    channel_finding(text.trim(), rustup, &mut push);
                }
                Err(e) => push("unreadable", format!("not valid TOML: {}", e.message())),
            }
        }
    }
    if let Some(home) = rustup {
        let settings = home.join("settings.toml");
        let overrides = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| t.get("overrides").and_then(toml::Value::as_table).cloned())
            .unwrap_or_default();
        let dir = canon(dir);
        for (path, toolchain) in &overrides {
            // Compared simplified: on Windows the key may be verbatim (`\\?\C:\…`) or plain.
            if let Some(name) = toolchain.as_str()
                && dir.starts_with(dunce::simplified(Path::new(path)))
            {
                let mut push = |kind: &str, text: String| {
                    out.push(Finding {
                        kind: kind.to_string(),
                        file: settings.display().to_string(),
                        text: format!("override for {path}: {text}"),
                        trustable: true,
                    });
                };
                channel_finding(name, rustup, &mut push);
            }
        }
    }
    out.retain(|f| !(f.trustable && trust.contains(&f.kind)));
    out
}

type Push<'a> = dyn FnMut(&str, String) + 'a;

fn show(path: &str, v: &toml::Value) -> String {
    format!("{path} = {v}")
}

fn entries(v: &toml::Value) -> Vec<(&String, &toml::Value)> {
    v.as_table().map(|t| t.iter().collect()).unwrap_or_default()
}

/// Cargo tables that cannot change what the gate's steps report (`source` is judged per
/// entry).
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
/// Linkers a configuration may choose by file name, when not shipped inside the
/// repository itself.
const KNOWN_LINKERS: &[&str] = &[
    "cc", "c++", "gcc", "g++", "clang", "clang++", "ld", "ld.lld", "lld", "rust-lld", "mold",
    "ld.mold", "link.exe", "lld-link",
];
/// Compiler caches allowed as `rustc-wrapper` when named, not shipped in the repository.
const KNOWN_WRAPPERS: &[&str] = &["sccache", "cachepot"];

/// A tool named by file name only (found on `PATH`, which is trusted), or by a path
/// outside the repository (`root`, when known), whose file name is in `known`.
fn known_name(s: &str, known: &[&str], root: Option<&Path>) -> bool {
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
        && (path.components().count() == 1
            || (path.is_absolute() && root.is_none_or(|r| !path.starts_with(r))))
}

fn known_tool(v: &toml::Value, root: &Path, known: &[&str]) -> bool {
    v.as_str().is_some_and(|s| known_name(s, known, Some(root)))
}

/// Subcommands the gate runs that an `[alias]` could replace (cargo ignores aliases of
/// its built-in commands, but `fmt`, `clippy` and `deny` are external).
const GATE_SUBCOMMANDS: &[&str] = &["fmt", "clippy", "deny", "test", "check", "build"];

/// Judge a parsed cargo configuration. `hidden`: inside the repository but untracked, where
/// every `[env]` entry, every flag and every `[target.<triple>]` table counts (cargo hands
/// `[env]` to rustc, clippy and the test binaries; `--cfg` can compile a failing test out).
/// Elsewhere `[target.<triple>]` tables apply only to the host, the `targets` the
/// configuration builds for, and `cfg(...)` selectors.
fn cargo_findings(
    t: &toml::Table,
    root: &Path,
    hidden: bool,
    host: Option<&str>,
    targets: &[String],
    push: &mut Push<'_>,
) {
    for (key, v) in t {
        match key.as_str() {
            "build" => {
                for (k, v) in entries(v) {
                    let path = format!("build.{k}");
                    match k.as_str() {
                        "rustflags" | "rustdocflags" => {
                            flag_findings(&path, v, Some(root), hidden, push);
                        }
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
                for (selector, tv) in entries(v) {
                    let applies = hidden
                        || selector.starts_with("cfg(")
                        || Some(selector.as_str()) == host
                        || targets.contains(selector)
                        || host.is_none();
                    if !applies {
                        continue;
                    }
                    for (k, v) in entries(tv) {
                        let path = format!("target.{selector}.{k}");
                        match k.as_str() {
                            "runner" => push("runner", show(&path, v)),
                            "linker" if known_tool(v, root, KNOWN_LINKERS) => {}
                            "linker" | "ar" => push("linker", show(&path, v)),
                            "rustflags" | "rustdocflags" => {
                                flag_findings(&path, v, Some(root), hidden, push);
                            }
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
            "env" => {
                for (name, v) in entries(v) {
                    let value = v
                        .as_str()
                        .or_else(|| v.get("value").and_then(toml::Value::as_str))
                        .unwrap_or("");
                    if hidden || env_weakens(name, value, root) {
                        push("env", show(&format!("env.{name}"), v));
                    }
                }
            }
            // Replacing a source by a local directory can swap in modified dependencies; a
            // remote registry mirror cannot.
            "source" => {
                for (name, sv) in entries(v) {
                    for k in ["directory", "local-registry"] {
                        if let Some(dir) = sv.get(k) {
                            push("source", show(&format!("source.{name}.{k}"), dir));
                        }
                    }
                }
            }
            "profile" => profile_findings("profile", v, push),
            // Followed by `configs`, each included file judged by its own place.
            "include" => {}
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

/// Judge a `rustflags` value (an array, or a string split at whitespace): one finding per
/// kind, naming the flags (a flag's value is not reported as a flag of its own). In a
/// hidden file any flag counts.
fn flag_findings(
    path: &str,
    v: &toml::Value,
    root: Option<&Path>,
    hidden: bool,
    push: &mut Push<'_>,
) {
    let tokens: Vec<String> = match v {
        toml::Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), String::from))
            .collect(),
        toml::Value::String(s) => s.split_whitespace().map(String::from).collect(),
        other => vec![other.to_string()],
    };
    let mut by_kind: Vec<(&'static str, Vec<String>)> = Vec::new();
    for (kind, flag) in judge_flags(&tokens, root) {
        match by_kind.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, flags)) => flags.push(flag),
            None => by_kind.push((kind, vec![flag])),
        }
    }
    if hidden && by_kind.is_empty() && !tokens.is_empty() {
        by_kind.push(("rustflag", tokens));
    }
    for (kind, flags) in by_kind {
        push(kind, format!("{path}: {}", flags.join(" ")));
    }
}

/// The flags in `tokens` that can weaken the gate, with their kind. A flag's separate
/// value travels with it. `root`, when known, keeps linkers inside the repository out.
fn judge_flags(tokens: &[String], root: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        // (option value, whether the value was the next token)
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
        let kind: Option<&'static str>;
        let used_next;
        if let Some((_, n)) = split("", "--cap-lints") {
            kind = Some("cap-lints");
            used_next = n;
        } else if let Some((_, n)) = split("-A", "--allow") {
            kind = Some("allow-flag");
            used_next = n;
        } else if let Some((_, n)) = split("", "--force-warn") {
            // A force-warned lint stays a warning under `-D warnings` (second review B1).
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
                // Above 0, debug assertions and overflow checks default to off (second
                // review M1): failing `debug_assert!`s and overflows then pass.
                "opt-level" if value == "0" => None,
                "debug-assertions" | "overflow-checks" if on => None,
                "link-arg" if is_known_fuse_ld(value, root) => None,
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
        } else if t.starts_with('-') {
            kind = Some("rustflag");
            // An unknown option's value, if any, is reported with it, not as a flag.
            used_next = next.is_some_and(|n| !n.starts_with('-'));
        } else {
            kind = Some("rustflag");
            used_next = false;
        }
        if let Some(k) = kind {
            let shown = if used_next {
                format!("{t} {}", next.unwrap_or(""))
            } else {
                t.to_string()
            };
            out.push((k, shown));
        }
        i += if used_next { 2 } else { 1 };
    }
    out
}

/// `-fuse-ld=` with a known linker (mold, lld, gold, bfd), by name or by a path outside
/// the repository (mold's README gives an absolute path).
fn is_known_fuse_ld(arg: &str, root: Option<&Path>) -> bool {
    arg.strip_prefix("-fuse-ld=").is_some_and(|l| {
        matches!(l, "mold" | "lld" | "gold" | "bfd" | "ld.mold" | "ld.lld")
            || known_name(
                l,
                &["mold", "ld.mold", "lld", "ld.lld", "ld.gold", "ld.bfd"],
                root,
            ) && Path::new(l).is_absolute()
    })
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
/// too (second review M5).
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

    fn git(root: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// Scan with the root as the only workspace member.
    fn sc(root: &Path, trust: &[String]) -> Vec<Finding> {
        scan(root, &[root.to_path_buf()], trust)
    }

    fn kinds_trusting(root: &Path, trust: &[String]) -> Vec<String> {
        let mut k: Vec<String> = sc(root, trust).into_iter().map(|f| f.kind).collect();
        k.sort();
        k
    }

    fn kinds(root: &Path) -> Vec<String> {
        kinds_trusting(root, &[])
    }

    /// The host triple, so that `[target.<host>]` tables apply on every CI platform.
    fn host() -> String {
        host_triple(Path::new(".")).expect("rustc -vV names the host")
    }

    #[test]
    fn finds_gate_weakening_settings() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            &format!(
                "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n\
                 [target.{}]\nrunner = \"true\"\n\
                 [env]\nINSTA_FORCE_PASS = \"1\"\n# runner = \"commented\"\n",
                host()
            ),
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
        // Outside a git repository nothing is hidden: kinds can be trusted.
        let trusted = kinds_trusting(r, &["runner".into(), "env".into()]);
        assert_eq!(trusted, ["cap-lints", "fmt-disabled", "toolchain-path"]);
    }

    /// The bypasses the final review reproduced against the line-based parser.
    #[test]
    fn bypasses_of_the_line_parser_are_found() {
        let h = host();
        let cases: &[(&str, String, &str)] = &[
            (
                ".cargo/config.toml",
                "[alias]\nclippy = \"test --no-run\"\n".into(),
                "alias",
            ),
            (
                ".cargo/config.toml",
                format!("[target.{h}]\nlinker = \".cargo/fake-ld.sh\"\n"),
                "linker",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"-Cmetadata=#\", \"--cap-lints\", \"allow\"]\n".into(),
                "cap-lints",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cap-\\u006cints\", \"allow\"]\n".into(),
                "cap-lints",
            ),
            (
                ".cargo/config.toml",
                format!("target.{h}.runner = \"true\"\n"),
                "runner",
            ),
            (
                ".cargo/config.toml",
                "[target.'cfg(all())']\nrunner = \"true\"\n".into(),
                "runner",
            ),
            (
                ".cargo/config.toml",
                "build = { rustflags = \"-A warnings\" }\n".into(),
                "allow-flag",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"-C\", \"link-arg=-Wl,-e,exit\"]\n".into(),
                "linker",
            ),
            (
                ".cargo/config.toml",
                "[patch.crates-io]\nfoo = { path = \"x\" }\n".into(),
                "config:patch",
            ),
            (
                "rust-toolchain.toml",
                "toolchain.path = \"/abs/tc\"\n".into(),
                "toolchain-path",
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"evil\"\n".into(),
                "toolchain-custom",
            ),
            (
                "rust-toolchain",
                "my-linked-toolchain\n".into(),
                "toolchain-custom",
            ),
        ];
        for (file, text, kind) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), file, text);
            assert_eq!(kinds(dir.path()), [*kind], "{file}: {text}");
        }
    }

    /// Inside a repository, configuration git does not track is what CI never sees: it
    /// fails whatever it says and whatever the repository trusts; once tracked it is in
    /// the diff and is not judged (third review, findings 4 and 5).
    #[test]
    fn hidden_config_is_judged_and_never_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(
            r,
            ".cargo/config.toml",
            &format!(
                "[target.{}]\nrunner = \"true\"\n[env]\nLD_PRELOAD = \"x.so\"\nSQLX_OFFLINE = \"true\"\n",
                host()
            ),
        );
        let all = ["runner".to_string(), "env".into()];
        assert_eq!(kinds_trusting(r, &all), ["env", "env", "runner"]);
        git(r, &["add", ".cargo/config.toml"]);
        assert_eq!(kinds_trusting(r, &[]), Vec::<String>::new());
    }

    /// rustfmt reads configuration from each source file's directory upward, so a nested
    /// file counts, ignored or not; clippy files likewise.
    #[test]
    fn nested_rustfmt_and_untracked_clippy_config_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, ".gitignore", "src/rustfmt.toml\n");
        write(r, "src/rustfmt.toml", "max_width = 200\n");
        write(
            r,
            "crates/a/clippy.toml",
            "too-many-arguments-threshold = 99\n",
        );
        assert_eq!(
            kinds(r),
            ["untracked-clippy-config", "untracked-rustfmt-config"]
        );
        git(r, &["add", "crates/a/clippy.toml"]);
        git(r, &["add", "-f", "src/rustfmt.toml"]);
        assert!(kinds(r).is_empty(), "{:?}", kinds(r));
    }

    /// A rustfmt file outside the repository applies only when the repository has none
    /// of its own (third review, finding 6).
    #[test]
    fn rustfmt_rules_from_outside_the_repository() {
        let outer = tempfile::tempdir().unwrap();
        let r = outer.path().join("repo");
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q"]);
        write(outer.path(), "rustfmt.toml", "max_width = 200\n");
        assert_eq!(kinds(&r), ["rustfmt-outside"]);
        assert!(kinds_trusting(&r, &["rustfmt-outside".into()]).is_empty());
        write(&r, "rustfmt.toml", "edition = \"2024\"\n");
        git(&r, &["add", "rustfmt.toml"]);
        assert!(kinds(&r).is_empty(), "{:?}", kinds(&r));
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
        let h = host();
        let cases: &[(String, &str)] = &[
            (
                "[build]\nrustflags = [\"--force-warn\", \"clippy::unwrap_used\"]\n".into(),
                "allow-flag",
            ),
            (
                "[build]\nrustflags = [\"-C\", \"opt-level=1\"]\n".into(),
                "rustflag",
            ),
            (
                "[build]\nrustflags = [\"-Cdebug-assertions=off\"]\n".into(),
                "rustflag",
            ),
            (
                "[source.crates-io]\nreplace-with = \"v\"\n[source.v]\ndirectory = \"v\"\n".into(),
                "source",
            ),
            ("[env]\nINSTA_FORCE_PASS = \"1\"\n".into(), "env"),
            ("[env]\nLD_PRELOAD = \"/tmp/x.so\"\n".into(), "env"),
            (
                format!("[target.{h}]\nlinker = \".cargo/fake-ld.sh\"\n"),
                "linker",
            ),
            (format!("[target.{h}]\nlinker = \"evil-ld\"\n"), "linker"),
            (
                "[build]\nrustc-wrapper = \"./wrap.sh\"\n".into(),
                "rustc-wrapper",
            ),
        ];
        for (text, kind) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".cargo/config.toml", text);
            assert_eq!(kinds(dir.path()), [*kind], "{text}");
        }
    }

    /// Common legitimate settings stay quiet: mold, sccache, tokio_unstable, ordinary
    /// `[env]` variables, explicit debug assertions, vendoring through a registry mirror,
    /// and settings for targets other than the host (third review, finding 4).
    #[test]
    fn common_legitimate_settings_pass() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            &format!(
                "[build]\nrustc-wrapper = \"sccache\"\n\
                 rustflags = [\"--cfg\", \"tokio_unstable\", \"-C\", \"opt-level=0\", \"-Cdebug-assertions=on\"]\n\
                 rustdocflags = [\"--cfg\", \"docsrs\"]\n\
                 [target.{}]\nlinker = \"clang\"\n\
                 rustflags = [\"-C\", \"link-arg=-fuse-ld=mold\", \"-C\", 'link-arg=-fuse-ld={}']\n\
                 [target.wasm32-unknown-unknown]\nrunner = \"wasm-bindgen-test-runner\"\n\
                 [target.thumbv7em-none-eabihf]\nrustflags = [\"-C\", \"link-arg=-Tlink.x\"]\n\
                 [source.crates-io]\nreplace-with = \"corp\"\n\
                 [source.corp]\nregistry = \"sparse+https://mirror.example.com/index/\"\n\
                 [env]\nSQLX_OFFLINE = \"true\"\n",
                host(),
                // An absolute path outside the repository on every platform; a TOML
                // literal string, since Windows paths hold backslashes.
                std::env::temp_dir().join("bin").join("mold").display()
            ),
        );
        assert!(sc(r, &[]).is_empty(), "{:?}", sc(r, &[]));
    }

    /// Flags are reported once per kind, a flag's value with it; a linker inside the
    /// repository is never a known one.
    #[test]
    fn flags_are_reported_with_their_values() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            &format!(
                "[build]\nrustdocflags = [\"-Z\", \"unstable-options\", \"--extern-html-root-url\", \"x=y\"]\n\
                 rustflags = [\"-A\", \"dead_code\", \"-Aunused\"]\n\
                 [target.{}]\nrustflags = [\"-C\", 'link-arg=-fuse-ld={}']\n",
                host(),
                r.join("tools/mold").display()
            ),
        );
        let f = sc(r, &[]);
        let mut k: Vec<&str> = f.iter().map(|f| f.kind.as_str()).collect();
        k.sort_unstable();
        assert_eq!(k, ["allow-flag", "linker", "rustflag"], "{f:?}");
        let text = |kind: &str| &f.iter().find(|f| f.kind == kind).unwrap().text;
        assert!(text("rustflag").contains("-Z unstable-options"), "{f:?}");
        assert!(
            text("rustflag").contains("--extern-html-root-url x=y"),
            "{f:?}"
        );
        assert!(
            text("allow-flag").contains("-A dead_code -Aunused"),
            "{f:?}"
        );
    }

    /// A clippy file above the repository is untracked by definition (final review M3); a
    /// vendored crate's own clippy file inside the tree cannot apply to the members (M10).
    #[test]
    fn clippy_files_outside_and_in_vendored_crates() {
        let outer = tempfile::tempdir().unwrap();
        let r = outer.path().join("repo");
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q"]);
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

    /// `rustup override set` lives in rustup's settings, not in the tree (third review,
    /// finding 2).
    #[test]
    fn rustup_directory_overrides_are_checked() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("crate");
        std::fs::create_dir_all(&sub).unwrap();
        let key = canon(dir.path()).display().to_string();
        let settings = |toolchain: &str| {
            write(
                home.path(),
                "settings.toml",
                &format!("version = \"12\"\n[overrides]\n'{key}' = \"{toolchain}\"\n"),
            );
        };
        settings("my-linked-toolchain");
        let f = toolchain_scan_with(&sub, &[], Some(home.path()));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].kind, "toolchain-custom");
        assert!(f[0].file.ends_with("settings.toml"), "{f:?}");
        settings("1.97.0");
        assert!(toolchain_scan_with(&sub, &[], Some(home.path())).is_empty());
        // Windows: a key in either spelling of the same directory applies.
        if cfg!(windows) {
            let key = format!(r"\\?\{key}");
            write(
                home.path(),
                "settings.toml",
                &format!("version = \"12\"\n[overrides]\n'{key}' = \"my-linked-toolchain\"\n"),
            );
            assert_eq!(toolchain_scan_with(&sub, &[], Some(home.path())).len(), 1);
        }
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
        // Inside a repository but untracked, trust does not apply; tracked, it is in the
        // diff and CI uses it too.
        git(dir.path(), &["init", "-q"]);
        let f = toolchain_scan(&sub, &["toolchain-path".into()]);
        assert!(f.len() == 1 && !f[0].trustable, "{f:?}");
        git(dir.path(), &["add", "rust-toolchain.toml"]);
        assert!(toolchain_scan(&sub, &[]).is_empty());
    }

    /// A tracked config can `include` an ignored one; the included file is judged by its
    /// own place and is part of the fingerprint (fourth review, M1).
    #[test]
    fn included_configs_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, ".gitignore", "/.cargo/local.toml\n");
        write(
            r,
            ".cargo/config.toml",
            "include = [{ path = \"local.toml\", optional = true }]\n",
        );
        git(r, &["add", ".gitignore", ".cargo/config.toml"]);
        write(
            r,
            ".cargo/local.toml",
            &format!("[target.{}]\nrunner = \"true\"\n", host()),
        );
        assert_eq!(kinds_trusting(r, &["runner".into()]), ["runner"]);
        assert!(config_files(r).iter().any(|p| p.ends_with("local.toml")));
        // A tracked included file is placed as tracked, whatever its name (fifth review).
        write(r, ".cargo/config.toml", "include = [\"shared.toml\"]\n");
        write(r, ".cargo/shared.toml", "[env]\nMY_SETTING = \"1\"\n");
        git(r, &["add", ".cargo/config.toml", ".cargo/shared.toml"]);
        assert!(kinds(r).is_empty(), "{:?}", kinds(r));
    }

    /// A file inside an ignored nested repository is hidden from this one, whatever the
    /// nested one tracks (sixth review).
    #[test]
    fn an_include_in_an_ignored_nested_repository_is_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, ".gitignore", "/.local/\n");
        write(
            r,
            ".cargo/config.toml",
            "include = [\"../.local/w.toml\"]\n",
        );
        git(r, &["add", ".gitignore", ".cargo/config.toml"]);
        let nested = r.join(".local");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        write(
            &nested,
            "w.toml",
            &format!("[target.{}]\nrunner = \"true\"\n", host()),
        );
        git(&nested, &["add", "w.toml"]);
        assert_eq!(kinds(r), ["runner"]);
    }

    /// A tracked symlink to an ignored file: git tracks only the link text, so the content
    /// is hidden (fifth review). The stricter of the link's and the target's place wins.
    #[cfg(unix)]
    #[test]
    fn a_tracked_link_to_an_ignored_config_is_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, ".gitignore", "/local-cargo.toml\n");
        write(
            r,
            "local-cargo.toml",
            &format!("[target.{}]\nrunner = \"true\"\n", host()),
        );
        std::fs::create_dir_all(r.join(".cargo")).unwrap();
        std::os::unix::fs::symlink("../local-cargo.toml", r.join(".cargo/config.toml")).unwrap();
        git(r, &["add", ".gitignore", ".cargo/config.toml"]);
        assert_eq!(kinds(r), ["runner"]);
    }

    /// `git update-index --skip-worktree` hides a tracked file's changes from git; it is
    /// judged as hidden (fourth review, M4).
    #[test]
    fn skip_worktree_configs_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, ".cargo/config.toml", "[build]\njobs = 2\n");
        git(r, &["add", ".cargo/config.toml"]);
        write(
            r,
            ".cargo/config.toml",
            &format!("[target.{}]\nrunner = \"true\"\n", host()),
        );
        assert!(kinds(r).is_empty(), "tracked, not judged");
        git(
            r,
            &["update-index", "--skip-worktree", ".cargo/config.toml"],
        );
        assert_eq!(kinds(r), ["runner"]);
    }

    /// The build target can be an array or come from another file; a hidden file has every
    /// target table judged (fourth review, M5). A hidden file's flags all count, `--cfg`
    /// included, since `#[cfg(not(x))]` can compile a failing test out (M7).
    #[test]
    fn target_tables_and_flags_in_hidden_and_outside_files() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            "[build]\ntarget = [\"thumbv7em-none-eabihf\"]\n\
             [target.thumbv7em-none-eabihf]\nrunner = \"true\"\n",
        );
        assert_eq!(kinds(r), ["runner"], "array build target, no git");
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(
            r,
            ".cargo/config.toml",
            "[target.wasm32-unknown-unknown]\nrunner = \"true\"\n\
             [build]\nrustflags = [\"--cfg\", \"miri\"]\n",
        );
        assert_eq!(kinds(r), ["runner", "rustflag"]);
    }

    /// A symlink inside the repository is placed where the link is, not where it points
    /// (fourth review, m1); a hidden toolchain file fails whatever channel it pins (m2).
    #[cfg(unix)]
    #[test]
    fn symlinks_and_hidden_toolchain_files_are_hidden() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "fmt.toml", "tab_spaces = 8\n");
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "-q"]);
        write(r, "rustfmt.toml", "edition = \"2021\"\n");
        git(r, &["add", "rustfmt.toml"]);
        std::fs::create_dir_all(r.join("src")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("fmt.toml"), r.join("src/rustfmt.toml"))
            .unwrap();
        assert_eq!(kinds(r), ["untracked-rustfmt-config"]);
        std::fs::remove_file(r.join("src/rustfmt.toml")).unwrap();
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.80.0\"\n",
        );
        assert_eq!(kinds(r), ["untracked-toolchain-config"]);
        let f = toolchain_scan(r, &["untracked-toolchain-config".into()]);
        assert!(f.len() == 1 && !f[0].trustable, "{f:?}");
    }

    /// Variables are judged by value where that is safe (third review, finding 8).
    #[test]
    fn weakening_env_by_name_and_value() {
        let root = Path::new("/repo");
        let vars = [
            ("PATH", "/usr/bin"),
            ("RUSTFLAGS", "--cfg tokio_unstable -C target-cpu=native"),
            ("RUSTDOCFLAGS", "-A warnings"),
            ("CARGO_ENCODED_RUSTFLAGS", "--cap-lints\u{1f}allow"),
            ("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER", "true"),
            ("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER", "clang"),
            ("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", "/repo/ld"),
            ("CARGO_ALIAS_CLIPPY", "check"),
            ("CARGO_BUILD_JOBS", "8"),
            ("CARGO_PROFILE_DEV_DEBUG", "0"),
            ("CARGO_PROFILE_TEST_OVERFLOW_CHECKS", "false"),
            ("CARGO_PROFILE_DEV_OPT_LEVEL", "1"),
            ("RUSTUP_TOOLCHAIN", "stable"),
            ("RUSTC_WRAPPER", "sccache"),
            ("RUSTC_WORKSPACE_WRAPPER", "/repo/wrap.sh"),
            ("CARGO_TARGET_DIR", "/elsewhere"),
            ("CARGO_HOME", "/home/x/.cargo"),
            ("RUSTFMT", "/bin/true"),
            ("LD_PRELOAD", "x.so"),
            ("UPDATE_GOLDENFILES", "1"),
        ];
        assert_eq!(
            weakening_env(vars.iter().copied(), &["env:RUSTFMT".into()], root),
            [
                "CARGO_ALIAS_CLIPPY",
                "CARGO_ENCODED_RUSTFLAGS",
                "CARGO_PROFILE_DEV_OPT_LEVEL",
                "CARGO_PROFILE_TEST_OVERFLOW_CHECKS",
                "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                "LD_PRELOAD",
                "RUSTC_WORKSPACE_WRAPPER",
                "RUSTDOCFLAGS",
                "RUSTUP_TOOLCHAIN",
                "UPDATE_GOLDENFILES",
            ]
        );
    }
}
