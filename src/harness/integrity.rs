//! Configuration and environment that can make every gate step pass without the code
//! changing (decision D-15, review finding B1). Reproduced: four lines of
//! `.cargo/config.toml` (`--cap-lints allow` plus `runner = "true"`) turned a failing
//! `clippy -D warnings` and a failing test into exit code 0.
//!
//! The gate reports such settings as findings and fails on them unless the repository
//! trusts that kind in `[package.metadata.fsmap.verify] trust = [...]`. Variables that do
//! the same from the environment are removed before every step.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Finding {
    /// `runner`, `rustc-wrapper`, `cap-lints`, `allow-flag`, `env`, `fmt-disabled`,
    /// `fmt-ignore`, `toolchain-path`.
    pub kind: &'static str,
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// Environment variables removed before running a gate step unless trusted as
/// `env:NAME`: they change how code is compiled, linted or tested, or let a test pass
/// by rewriting its expectations.
const WEAKENING_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "RUSTC",
    "CARGO_BUILD_RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CLIPPY_CONF_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "INSTA_FORCE_PASS",
    "INSTA_UPDATE",
    "UPDATE_EXPECT",
    "TRYBUILD",
];

/// Names of weakening variables set in `vars` (`CARGO_TARGET_<triple>_RUNNER` and
/// `_RUSTFLAGS` included), minus trusted ones.
pub fn weakening_env<'a>(vars: impl Iterator<Item = &'a str>, trust: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vars
        .filter(|k| {
            WEAKENING_ENV.contains(k)
                || (k.starts_with("CARGO_TARGET_")
                    && (k.ends_with("_RUNNER") || k.ends_with("_RUSTFLAGS")))
        })
        .filter(|k| !trust.iter().any(|t| t.strip_prefix("env:") == Some(*k)))
        .map(String::from)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every configuration file that cargo, rustfmt, clippy or rustup reads for `root`.
/// Existing files only. These are hashed into the stop gate's fingerprint whether or not
/// git tracks or ignores them.
pub fn config_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in root.ancestors() {
        for rel in [
            ".cargo/config.toml",
            ".cargo/config",
            "rustfmt.toml",
            ".rustfmt.toml",
            "clippy.toml",
            ".clippy.toml",
            "rust-toolchain.toml",
            "rust-toolchain",
        ] {
            out.push(dir.join(rel));
        }
    }
    if let Some(home) = cargo_home() {
        out.push(home.join("config.toml"));
        out.push(home.join("config"));
    }
    if let Some(cfg) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        out.push(cfg.join("rustfmt/rustfmt.toml"));
        out.push(cfg.join("rustfmt/.rustfmt.toml"));
    }
    out.retain(|p| p.is_file());
    out.dedup();
    out
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
}

/// Settings in the configuration files that weaken the gate, minus trusted kinds.
pub fn scan(root: &Path, trust: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    for path in config_files(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let shown = path
            .strip_prefix(root)
            .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
        let kind_of: fn(&str, &str) -> Option<&'static str> = match name {
            "config.toml" | "config" => cargo_config_kind,
            "rustfmt.toml" | ".rustfmt.toml" => rustfmt_kind,
            "rust-toolchain.toml" | "rust-toolchain" => toolchain_kind,
            _ => continue,
        };
        let mut section = String::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(s) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = s.trim().trim_matches('[').trim_matches(']').to_string();
                if section == "env" && !trust.iter().any(|t| t == "env") {
                    out.push(Finding {
                        kind: "env",
                        file: shown.clone(),
                        line: i + 1,
                        text: line.to_string(),
                    });
                }
                continue;
            }
            if let Some(kind) = kind_of(&section, line)
                && !trust.iter().any(|t| t == kind)
            {
                out.push(Finding {
                    kind,
                    file: shown.clone(),
                    line: i + 1,
                    text: line.chars().take(160).collect(),
                });
            }
        }
    }
    out
}

fn key(line: &str) -> &str {
    line.split('=')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"')
}

fn cargo_config_kind(_section: &str, line: &str) -> Option<&'static str> {
    let k = key(line);
    if k == "runner" || k.ends_with(".runner") {
        Some("runner")
    } else if matches!(k, "rustc-wrapper" | "rustc-workspace-wrapper" | "rustc")
        || k.ends_with(".rustc-wrapper")
    {
        Some("rustc-wrapper")
    } else if line.contains("cap-lints") {
        Some("cap-lints")
    } else if ["\"-A", "'-A", "--allow", "-Awarnings"]
        .iter()
        .any(|t| line.contains(t))
    {
        Some("allow-flag")
    } else {
        None
    }
}

fn rustfmt_kind(_section: &str, line: &str) -> Option<&'static str> {
    match key(line) {
        "disable_all_formatting" if line.contains("true") => Some("fmt-disabled"),
        "ignore" => Some("fmt-ignore"),
        _ => None,
    }
}

fn toolchain_kind(section: &str, line: &str) -> Option<&'static str> {
    // `path = "/abs/dir"` makes rustup run binaries from that directory.
    (key(line) == "path" && (section.is_empty() || section == "toolchain"))
        .then_some("toolchain-path")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
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
        let kinds: Vec<&str> = scan(r, &[]).iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                "cap-lints",
                "runner",
                "env",
                "fmt-disabled",
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

    #[test]
    fn benign_config_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            ".cargo/config.toml",
            "[build]\nrustflags = [\"-C\", \"target-cpu=native\"]\n[net]\nretry = 3\n",
        );
        write(r, "rustfmt.toml", "max_width = 100\n");
        write(
            r,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.97.0\"\n",
        );
        assert!(scan(r, &[]).is_empty(), "{:?}", scan(r, &[]));
    }

    #[test]
    fn weakening_env_names() {
        let vars = [
            "PATH",
            "RUSTFLAGS",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
        ];
        assert_eq!(
            weakening_env(vars.iter().copied(), &["env:RUSTC_WRAPPER".into()]),
            ["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER", "RUSTFLAGS"]
        );
    }
}
