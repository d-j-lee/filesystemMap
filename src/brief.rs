//! `fsmap brief`: what to know before (or right after) touching a file — files that
//! usually change with it, importers, tests, generated status, owners. `--diff` turns it
//! into a "you probably forgot B" check; `--hook` speaks Claude Code's hook protocol.
//!
//! The facts are cached per repository and HEAD so repeated calls (hooks) are instant.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::analysis::{Analysis, AuthorMode};
use crate::git;
use crate::model::{Flags, NodeId};
use crate::util::{count, duration};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct FileFacts {
    pub commits: u32,
    pub last_commit: i64,
    pub owners: Vec<String>,
    pub generated: bool,
    pub test: bool,
    pub hotspot: bool,
    pub fan_in: u32,
    pub code: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Pair {
    pub other: String,
    pub shared: u32,
    /// P(other changes | this changes).
    pub conf: f32,
    pub linked: Option<bool>,
}

/// Everything a brief needs, keyed by repo-relative path.
#[derive(Serialize, Deserialize, Default)]
pub struct Index {
    pub version: String,
    pub head: String,
    pub newest_commit: i64,
    pub files: BTreeMap<String, FileFacts>,
    pub partners: BTreeMap<String, Vec<Pair>>,
    pub importers: BTreeMap<String, Vec<String>>,
    pub imports: BTreeMap<String, Vec<String>>,
}

impl Index {
    pub fn build(a: &Analysis) -> Index {
        let t = &a.tree;
        let path = |id: NodeId| t.rel_path(id);
        let hot: HashSet<NodeId> = a
            .hotspots
            .iter()
            .filter(|h| h.flagged(t))
            .map(|h| h.node)
            .collect();
        let names = |ids: &[u32]| -> Vec<String> {
            if a.options.authors == AuthorMode::None {
                return Vec::new();
            }
            ids.iter()
                .filter_map(|&i| a.authors.names.get(i as usize).cloned())
                .collect()
        };
        let mut idx = Index {
            version: env!("CARGO_PKG_VERSION").into(),
            head: a
                .repos
                .first()
                .and_then(|r| r.head.clone())
                .unwrap_or_default(),
            newest_commit: a
                .repos
                .iter()
                .filter_map(|r| r.history.as_ref().map(|h| h.last))
                .max()
                .unwrap_or(0),
            ..Default::default()
        };
        for id in 0..t.len() as NodeId {
            let n = t.get(id);
            if !n.is_file() || !n.is_analysed() {
                continue;
            }
            idx.files.insert(
                path(id),
                FileFacts {
                    commits: n.commits,
                    last_commit: n.last_commit,
                    owners: a
                        .history
                        .file_authors
                        .get(&id)
                        .map(|v| names(v))
                        .unwrap_or_default(),
                    generated: n.flags.has(Flags::GENERATED),
                    test: n.flags.has(Flags::TEST),
                    hotspot: hot.contains(&id),
                    fan_in: n.fan_in,
                    code: n.code,
                },
            );
        }
        for p in &a.history.coupling {
            let (pa, pb) = (path(p.a), path(p.b));
            idx.partners.entry(pa.clone()).or_default().push(Pair {
                other: pb.clone(),
                shared: p.shared,
                conf: p.conf_ab,
                linked: p.linked,
            });
            idx.partners.entry(pb).or_default().push(Pair {
                other: pa,
                shared: p.shared,
                conf: p.conf_ba,
                linked: p.linked,
            });
        }
        for v in idx.partners.values_mut() {
            v.sort_by(|x, y| y.conf.total_cmp(&x.conf).then(x.other.cmp(&y.other)));
        }
        if let Some(g) = &a.graph {
            for e in &g.edges {
                if !t.get(e.to).is_file() {
                    continue;
                }
                idx.importers
                    .entry(path(e.to))
                    .or_default()
                    .push(path(e.from));
                idx.imports
                    .entry(path(e.from))
                    .or_default()
                    .push(path(e.to));
            }
        }
        idx
    }
}

fn cache_path(root: &Path) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let key = xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes());
    Some(base.join("fsmap").join(format!("brief-{key:016x}.json")))
}

pub fn current_head(root: &Path) -> Option<String> {
    git::run({
        let mut c = git::git(root);
        c.args(["rev-parse", "--short=12", "HEAD"]);
        c
    })
    .ok()
    .map(|o| String::from_utf8_lossy(&o).trim().to_string())
}

/// Load the cached index for `root` if it matches HEAD and this fsmap version.
pub fn load_cached(root: &Path) -> Option<Index> {
    let head = current_head(root)?;
    let text = std::fs::read_to_string(cache_path(root)?).ok()?;
    let idx: Index = serde_json::from_str(&text).ok()?;
    (idx.head == head && idx.version == env!("CARGO_PKG_VERSION")).then_some(idx)
}

pub fn store_cached(root: &Path, idx: &Index) {
    if let Some(p) = cache_path(root) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(idx) {
            let _ = std::fs::write(p, json);
        }
    }
}

/// CODEOWNERS: last matching rule wins (GitHub semantics, approximated with globs).
pub struct CodeOwners {
    rules: Vec<(GlobMatcher, Vec<String>)>,
}

impl CodeOwners {
    pub fn load(root: &Path) -> Option<CodeOwners> {
        let text = [
            ".github/CODEOWNERS",
            "CODEOWNERS",
            "docs/CODEOWNERS",
            ".gitlab/CODEOWNERS",
        ]
        .iter()
        .find_map(|p| std::fs::read_to_string(root.join(p)).ok())?;
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pat) = parts.next() else { continue };
            let owners: Vec<String> = parts.map(str::to_string).collect();
            let mut glob = pat.trim_start_matches('/').to_string();
            if !pat.starts_with('/') && !pat.trim_end_matches('/').contains('/') {
                glob = format!("**/{glob}");
            }
            if glob.ends_with('/') {
                glob.push_str("**");
            } else if !glob.contains('*') && !glob.contains('.') {
                glob.push_str("{,/**}");
            }
            if let Ok(g) = Glob::new(&glob) {
                rules.push((g.compile_matcher(), owners));
            }
        }
        Some(CodeOwners { rules })
    }

    pub fn owners(&self, path: &str) -> Option<&[String]> {
        self.rules
            .iter()
            .rev()
            .find(|(m, _)| m.is_match(path))
            .map(|(_, o)| o.as_slice())
    }
}

fn likely_tests<'a>(idx: &'a Index, path: &str) -> Vec<&'a str> {
    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut out: Vec<&str> = Vec::new();
    for (p, f) in &idx.files {
        if !f.test || p == path {
            continue;
        }
        let other = Path::new(p)
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let named = other.contains(&stem) && stem.len() >= 3;
        let imports = idx
            .imports
            .get(p)
            .is_some_and(|v| v.iter().any(|x| x == path));
        if named || imports {
            out.push(p);
        }
    }
    out.sort();
    out.truncate(5);
    out
}

/// Markdown brief for one repo-relative path.
pub fn brief_file(
    idx: &Index,
    owners: Option<&CodeOwners>,
    path: &str,
    exclude: &HashSet<&str>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "### `{path}`");
    let Some(f) = idx.files.get(path) else {
        let _ = writeln!(s, "- Not in the map (new, ignored, or outside the scan).");
        return s;
    };
    if f.generated {
        let _ = writeln!(
            s,
            "- **Generated file** — edit its source/generator instead."
        );
    }
    let partners: Vec<&Pair> = idx
        .partners
        .get(path)
        .map(|v| {
            v.iter()
                .filter(|p| p.conf >= 0.3 && !exclude.contains(p.other.as_str()))
                .take(5)
                .collect()
        })
        .unwrap_or_default();
    if !partners.is_empty() {
        let list = partners
            .iter()
            .map(|p| {
                format!(
                    "`{}` ({:.0}% of this file's commits{})",
                    p.other,
                    p.conf * 100.0,
                    if p.linked == Some(false) {
                        ", no import link"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let _ = writeln!(s, "- Usually changes with: {list}");
    }
    if let Some(imp) = idx.importers.get(path) {
        let shown: Vec<String> = imp.iter().take(6).map(|p| format!("`{p}`")).collect();
        let more = imp.len().saturating_sub(6);
        let _ = writeln!(
            s,
            "- Imported by {}: {}{}",
            imp.len(),
            shown.join(", "),
            if more > 0 {
                format!(" … +{more}")
            } else {
                String::new()
            }
        );
    }
    let tests = likely_tests(idx, path);
    if !tests.is_empty() {
        let _ = writeln!(
            s,
            "- Likely tests: {}",
            tests
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut facts = Vec::new();
    if f.commits > 0 {
        facts.push(format!("{} commits in window", f.commits));
    }
    if f.last_commit > 0 && idx.newest_commit > 0 {
        let gap = idx.newest_commit - f.last_commit;
        facts.push(if gap < 86_400 {
            "changed in the latest commits".into()
        } else {
            format!("last changed {} before the latest commit", duration(gap))
        });
    }
    if f.hotspot {
        facts.push("hotspot (top change × complexity)".into());
    }
    if f.code > 0 {
        facts.push(format!("{} code lines", count(u64::from(f.code))));
    }
    if !facts.is_empty() {
        let _ = writeln!(s, "- History: {}", facts.join(", "));
    }
    if !f.owners.is_empty() {
        let _ = writeln!(
            s,
            "- Authors (degree-of-authorship): {}",
            f.owners.join(", ")
        );
    }
    if let Some(o) = owners.and_then(|o| o.owners(path)) {
        let _ = writeln!(s, "- CODEOWNERS: {}", o.join(" "));
    }
    s
}

/// For a change set: co-change partners that are *not* part of it.
pub fn forgotten(
    idx: &Index,
    changed: &[String],
    min_conf: f32,
) -> Vec<(String, String, f32, u32)> {
    let set: HashSet<&str> = changed.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for c in changed {
        for p in idx.partners.get(c).into_iter().flatten() {
            if p.conf >= min_conf
                && p.shared >= 5
                && !set.contains(p.other.as_str())
                && idx.files.contains_key(&p.other)
            {
                out.push((c.clone(), p.other.clone(), p.conf, p.shared));
            }
        }
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.1.cmp(&b.1)));
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// Files changed in the work tree relative to `base` (default HEAD), plus untracked.
pub fn changed_files(root: &Path, base: &str) -> Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    let out = git::run({
        let mut c = git::git(root);
        c.args(["diff", "--name-only", "-z", "--no-relative", base]);
        c
    })
    .context("git diff failed")?;
    files.extend(
        out.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned()),
    );
    let untracked = git::run({
        let mut c = git::git(root);
        c.args([
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ]);
        c
    })?;
    files.extend(
        untracked
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned()),
    );
    files.sort();
    files.dedup();
    Ok(files)
}

/// Claude Code hook protocol: read the hook event from stdin, return the JSON to print
/// (or `None` when there is nothing worth saying).
pub fn hook_response(
    idx: &Index,
    owners: Option<&CodeOwners>,
    root: &Path,
    stdin: &str,
) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdin).ok()?;
    let event = v.get("hook_event_name")?.as_str()?.to_string();
    let file = v.get("tool_input")?.get("file_path")?.as_str()?;
    let abs = PathBuf::from(file);
    let rel = abs
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let f = idx.files.get(&rel)?;
    let has_partners = idx
        .partners
        .get(&rel)
        .is_some_and(|p| p.iter().any(|x| x.conf >= 0.3));
    if !(has_partners || f.generated || f.hotspot || f.fan_in >= 5) {
        return None;
    }
    let mut text = format!(
        "fsmap brief for the file just {}:\n",
        if event == "PreToolUse" {
            "about to be edited"
        } else {
            "edited"
        }
    );
    text.push_str(&brief_file(idx, owners, &rel, &HashSet::new()));
    if text.len() > 2000 {
        let mut cut = 2000;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": text,
            }
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> Index {
        let mut idx = Index::default();
        for p in [
            "src/a.rs",
            "src/b.rs",
            "tests/alpha_test.rs",
            "src/alpha.rs",
        ] {
            idx.files.insert(
                p.into(),
                FileFacts {
                    test: p.starts_with("tests"),
                    ..Default::default()
                },
            );
        }
        idx.partners.insert(
            "src/a.rs".into(),
            vec![Pair {
                other: "src/b.rs".into(),
                shared: 8,
                conf: 0.8,
                linked: Some(false),
            }],
        );
        idx
    }

    #[test]
    fn forgot_b() {
        let idx = idx();
        let f = forgotten(&idx, &["src/a.rs".into()], 0.5);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].1, "src/b.rs");
        assert!(forgotten(&idx, &["src/a.rs".into(), "src/b.rs".into()], 0.5).is_empty());
    }

    #[test]
    fn brief_mentions_partner_and_tests() {
        let idx = idx();
        let s = brief_file(&idx, None, "src/a.rs", &HashSet::new());
        assert!(s.contains("Usually changes with: `src/b.rs` (80%"));
        let s = brief_file(&idx, None, "src/alpha.rs", &HashSet::new());
        assert!(s.contains("Likely tests: `tests/alpha_test.rs`"));
    }

    #[test]
    fn hook_protocol() {
        let idx = idx();
        let root = Path::new("/repo");
        let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{"file_path":"/repo/src/a.rs"}}"#;
        let out = hook_response(&idx, None, root, input).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("src/b.rs")
        );
        let quiet =
            r#"{"hook_event_name":"PostToolUse","tool_input":{"file_path":"/repo/src/b.rs"}}"#;
        assert!(hook_response(&idx, None, root, quiet).is_none());
    }

    #[test]
    fn codeowners_last_match_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CODEOWNERS"),
            "* @all\n/src/ @core\n*.md @docs\n",
        )
        .unwrap();
        let co = CodeOwners::load(dir.path()).unwrap();
        assert_eq!(co.owners("src/a.rs").unwrap(), ["@core"]);
        assert_eq!(co.owners("src/README.md").unwrap(), ["@docs"]);
        assert_eq!(co.owners("other/x").unwrap(), ["@all"]);
    }
}
