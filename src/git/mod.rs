//! Git integration by shelling out to the `git` CLI (decision D-03, hardened per D-08).
//!
//! Every invocation goes through [`git`], which pins config that could otherwise break
//! parsing or execute programs from an untrusted repository's config.

pub mod history;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;

use crate::model::{Category, Flags, GitState, Kind, NodeId, Tree, Zone};

/// Build a `git` command with the fixed safety prefix.
pub fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--no-pager")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "log.showSignature=false",
            "-c",
            "color.ui=false",
            "-c",
            "diff.relative=false",
        ])
        // Never take index.lock (agents run git in the same repos concurrently), never
        // prompt, never lazily fetch blobs in partial clones.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null());
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_NAMESPACE",
    ] {
        c.env_remove(var);
    }
    c
}

/// A `git` command for subcommands that compare the work tree with the index (`status`,
/// `diff` against the work tree). Those can run `filter.<driver>.clean` programs named in
/// an untrusted repository's config whenever a file's stat data changed, so every
/// configured filter driver is neutralised for this invocation.
pub fn git_worktree(dir: &Path) -> Command {
    let mut drivers: Vec<String> = run({
        let mut c = git(dir);
        c.args(["config", "--get-regexp", r"^filter\."]);
        c
    })
    .map(|out| {
        String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|key| key.strip_prefix("filter."))
            .filter_map(|rest| rest.rsplit_once('.').map(|(name, _)| name.to_string()))
            .collect()
    })
    .unwrap_or_default();
    drivers.sort();
    drivers.dedup();
    let mut c = git(dir);
    for d in drivers {
        for key in ["clean", "smudge", "process"] {
            c.arg("-c").arg(format!("filter.{d}.{key}="));
        }
        c.arg("-c").arg(format!("filter.{d}.required=false"));
    }
    c
}

/// Run and return stdout; a non-zero exit is an error carrying the first stderr line.
pub fn run(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.lines().next().unwrap_or("git failed").trim());
    }
    Ok(out.stdout)
}

/// Run with `input` written to stdin from a separate thread (avoids pipe deadlocks).
fn run_with_stdin(mut cmd: Command, input: Vec<u8>) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("stdin writer panicked"))??;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim_end().to_string()
}

pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hygiene {
    /// Modified / staged tracked files (untracked files are counted on the nodes).
    pub dirty: u32,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub stashes: u32,
    /// Local branches with no upstream, or ahead of it.
    pub unpushed_branches: Vec<String>,
    pub worktrees: u32,
    pub prunable_worktrees: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    /// Work-tree root in the scanned tree (0 if the scan root is inside the repo).
    pub node: NodeId,
    /// Display path of the work tree relative to the scan root (`.` for the root).
    pub path: String,
    /// When the scan root is a subdirectory of the repo: its path inside the repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub head: Option<String>,
    pub shallow: bool,
    pub partial: bool,
    pub tracked: u32,
    pub untracked: u32,
    pub ignored_bytes: u64,
    pub hygiene: Option<Hygiene>,
    pub history: Option<history::Summary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    pub toplevel: PathBuf,
}

impl Repo {
    /// Map a toplevel-relative git path to a scan-root-relative path.
    fn to_scan_path(&self, git_path: &str, repo_rel: &str) -> Option<String> {
        let p = match &self.prefix {
            // The scan root itself (e.g. an ignored directory reported as `out/`).
            Some(prefix) if git_path == prefix.as_str() => "",
            Some(prefix) => git_path.strip_prefix(prefix.as_str())?.strip_prefix('/')?,
            None => git_path,
        };
        if p.is_empty() {
            return Some(repo_rel.to_string());
        }
        Some(if repo_rel.is_empty() {
            p.to_string()
        } else {
            format!("{repo_rel}/{p}")
        })
    }
}

/// Path lookups shared by git and import resolution: scan-root-relative path → node.
pub struct PathIndex {
    pub map: HashMap<String, NodeId>,
}

impl PathIndex {
    pub fn new(tree: &Tree) -> Self {
        let mut map = tree.path_index(0);
        map.retain(|_, id| tree.get(*id).zone != Zone::Vcs);
        PathIndex { map }
    }

    pub fn get(&self, rel: &str) -> Option<NodeId> {
        if rel.is_empty() {
            return Some(0);
        }
        self.map.get(rel).copied()
    }
}

/// Find the repository enclosing the scan root (if any) and every work tree inside it.
pub fn discover(tree: &Tree, root: &Path, max_repos: usize) -> (Vec<Repo>, usize) {
    let mut repos = Vec::new();
    if let Ok(top) = run({
        let mut c = git(root);
        c.args(["rev-parse", "--show-toplevel"]);
        c
    }) {
        let top = PathBuf::from(text(top));
        let top = std::fs::canonicalize(&top).unwrap_or(top);
        let prefix = root
            .strip_prefix(&top)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty());
        // Ignore if the root sits inside `.git` itself.
        if !root.components().any(|c| c.as_os_str() == ".git") {
            repos.push(Repo {
                node: 0,
                path: ".".into(),
                prefix,
                toplevel: top,
                ..Default::default()
            });
        }
    }
    let candidates: Vec<NodeId> = (1..tree.len() as NodeId)
        .filter(|&id| {
            let n = tree.get(id);
            n.kind == Kind::Dir
                && n.zone.is_analysed()
                && tree.children(id).any(|c| &*tree.get(c).name == ".git")
        })
        .collect();
    // A `.git` entry is not proof of a work tree (an empty or broken one makes git fall
    // back to the parent repository): keep only directories git itself names as toplevel.
    let nested: Vec<Repo> = candidates
        .par_iter()
        .filter_map(|&id| {
            let dir = root.join(tree.rel_path(id));
            let top = run({
                let mut c = git(&dir);
                c.args(["rev-parse", "--show-toplevel"]);
                c
            })
            .ok()?;
            let top = PathBuf::from(text(top));
            let same = std::fs::canonicalize(&top).ok()? == std::fs::canonicalize(&dir).ok()?;
            same.then(|| Repo {
                node: id,
                path: tree.rel_path(id),
                toplevel: dir,
                ..Default::default()
            })
        })
        .collect();
    repos.extend(nested);
    let total = repos.len();
    repos.truncate(max_repos);
    (repos, total)
}

/// Owning (deepest) repo for every node, as an index into `repos` (or `u32::MAX`).
fn owners(tree: &Tree, repos: &[Repo]) -> Vec<u32> {
    let mut owner = vec![u32::MAX; tree.len()];
    let mut order: Vec<usize> = (0..repos.len()).collect();
    order.sort_by_key(|&i| repos[i].node);
    for i in order {
        for id in tree.subtree(repos[i].node) {
            owner[id as usize] = i as u32;
        }
    }
    owner
}

struct StatusData {
    tracked: Vec<String>,
    ignored: Vec<String>,
    generated: Vec<String>,
    vendored: Vec<String>,
    head: Option<String>,
    shallow: bool,
    partial: bool,
    hygiene: Option<Hygiene>,
}

fn split_z(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

fn collect_status(repo: &Repo) -> Result<StatusData> {
    let top = &repo.toplevel;
    let tracked: Vec<String> = split_z(&run({
        let mut c = git(top);
        c.args(["ls-files", "-z", "--full-name"]);
        c
    })?)
    .collect();
    let ignored: Vec<String> = split_z(&run({
        let mut c = git(top);
        c.args([
            "ls-files",
            "-z",
            "--full-name",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ]);
        c
    })?)
    .collect();

    // linguist attributes, only when some .gitattributes mentions them.
    let (mut generated, mut vendored) = (Vec::new(), Vec::new());
    let mentions_linguist = tracked
        .iter()
        .filter(|p| p.ends_with(".gitattributes"))
        .any(|p| std::fs::read_to_string(top.join(p)).is_ok_and(|t| t.contains("linguist-")));
    if mentions_linguist {
        let mut input = Vec::new();
        for p in &tracked {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let mut c = git(top);
        c.args([
            "check-attr",
            "-z",
            "--stdin",
            "linguist-generated",
            "linguist-vendored",
        ]);
        if let Ok(out) = run_with_stdin(c, input) {
            let parts: Vec<String> = split_z(&out).collect();
            for t in parts.chunks(3) {
                if let [path, attr, value] = t
                    && (value == "set" || value == "true")
                {
                    match attr.as_str() {
                        "linguist-generated" => generated.push(path.clone()),
                        "linguist-vendored" => vendored.push(path.clone()),
                        _ => {}
                    }
                }
            }
        }
    }

    let head = run({
        let mut c = git(top);
        c.args(["rev-parse", "--short=12", "HEAD"]);
        c
    })
    .ok()
    .map(text);
    let shallow = run({
        let mut c = git(top);
        c.args(["rev-parse", "--is-shallow-repository"]);
        c
    })
    .is_ok_and(|o| text(o) == "true");
    let partial = is_partial(top);

    Ok(StatusData {
        tracked,
        ignored,
        generated,
        vendored,
        head,
        shallow,
        partial,
        hygiene: collect_hygiene(top).ok(),
    })
}

/// Partial (promisor) clone: blobs may be missing locally. Older gits record it as
/// `extensions.partialclone`, newer ones as `remote.<name>.promisor = true`.
pub fn is_partial(top: &Path) -> bool {
    run({
        let mut c = git(top);
        c.args([
            "config",
            "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.promisor)$",
        ]);
        c
    })
    .is_ok_and(|o| {
        String::from_utf8_lossy(&o)
            .lines()
            .any(|l| l.starts_with("extensions.partialclone") || l.ends_with(" true"))
    })
}

fn collect_hygiene(top: &Path) -> Result<Hygiene> {
    let mut h = Hygiene::default();
    let status = run({
        let mut c = git_worktree(top);
        c.args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=no",
            "--ignore-submodules=dirty",
        ]);
        c
    })?;
    let mut entries = status.split(|&b| b == 0).filter(|s| !s.is_empty());
    while let Some(e) = entries.next() {
        let line = String::from_utf8_lossy(e);
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            h.branch = (rest != "(detached)").then(|| rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            h.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(a) = part.strip_prefix('+') {
                    h.ahead = a.parse().unwrap_or(0);
                } else if let Some(b) = part.strip_prefix('-') {
                    h.behind = b.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("u ") {
            h.dirty += 1;
        } else if line.starts_with("2 ") {
            h.dirty += 1;
            entries.next(); // rename source path
        }
    }
    h.stashes = run({
        let mut c = git(top);
        c.args(["rev-list", "--walk-reflogs", "--count", "refs/stash"]);
        c
    })
    .ok()
    .and_then(|o| text(o).parse().ok())
    .unwrap_or(0);
    if let Ok(out) = run({
        let mut c = git(top);
        c.args([
            "for-each-ref",
            "--format=%(refname:short)%09%(upstream:short)%09%(upstream:track)",
            "refs/heads",
        ]);
        c
    }) {
        for line in text(out).lines() {
            let mut f = line.split('\t');
            let (name, upstream, track) = (
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
            );
            if upstream.is_empty() || track.contains("ahead") {
                h.unpushed_branches.push(name.to_string());
            }
        }
    }
    if let Ok(out) = run({
        let mut c = git(top);
        c.args(["worktree", "list", "--porcelain"]);
        c
    }) {
        for line in text(out).lines() {
            if line.starts_with("worktree ") {
                h.worktrees += 1;
            } else if line.starts_with("prunable") {
                h.prunable_worktrees += 1;
            }
        }
    }
    Ok(h)
}

/// Mark git state (tracked / untracked / ignored) and linguist attributes on the tree.
/// Repos whose `git` calls fail keep `error` set and their nodes keep `git = None`.
pub fn apply_status(tree: &mut Tree, index: &PathIndex, repos: &mut [Repo]) {
    let data: Vec<Result<StatusData>> = repos.par_iter().map(collect_status).collect();
    let owner = owners(tree, repos);

    for (ri, (repo, data)) in repos.iter_mut().zip(data).enumerate() {
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                repo.error = Some(e.to_string());
                continue;
            }
        };
        let repo_rel = if repo.node == 0 {
            String::new()
        } else {
            tree.rel_path(repo.node)
        };
        let lookup = |p: &str| {
            let p = p.trim_end_matches('/');
            repo.to_scan_path(p, &repo_rel)
                .and_then(|s| index.get(&s))
                .filter(|&id| owner[id as usize] == ri as u32)
        };
        let mut tracked = 0u32;
        for p in &data.tracked {
            if let Some(id) = lookup(p) {
                tree.get_mut(id).git = Some(GitState::Tracked);
                tracked += 1;
            }
        }
        for p in &data.ignored {
            if let Some(id) = lookup(p) {
                for sub in tree.subtree(id) {
                    if owner[sub as usize] == ri as u32 && tree.get(sub).zone != Zone::Vcs {
                        let n = tree.get_mut(sub);
                        if n.git.is_none() {
                            n.git = Some(GitState::Ignored);
                        }
                    }
                }
            }
        }
        for p in &data.generated {
            if let Some(id) = lookup(p) {
                let n = tree.get_mut(id);
                n.flags.set(Flags::GENERATED);
                n.category = Category::Generated;
            }
        }
        for p in &data.vendored {
            if let Some(id) = lookup(p) {
                tree.get_mut(id).zone = Zone::Vendored;
            }
        }
        // Everything else the repo owns (outside .git) is untracked.
        let mut untracked = 0u32;
        let mut ignored_bytes = 0u64;
        for id in tree.subtree(repo.node) {
            if owner[id as usize] != ri as u32 {
                continue;
            }
            let n = tree.get_mut(id);
            if n.zone == Zone::Vcs || n.kind == Kind::Dir {
                continue;
            }
            match n.git {
                None => {
                    n.git = Some(GitState::Untracked);
                    untracked += 1;
                }
                Some(GitState::Ignored) => ignored_bytes += n.disk,
                _ => {}
            }
        }
        repo.tracked = tracked;
        repo.untracked = untracked;
        repo.ignored_bytes = ignored_bytes;
        repo.head = data.head;
        repo.shallow = data.shallow;
        repo.partial = data.partial;
        repo.hygiene = data.hygiene;
        tree.get_mut(repo.node).flags.set(Flags::REPO);
    }

    // Directory state: tracked if anything below is tracked, ignored if everything is.
    for i in (0..tree.len()).rev() {
        let id = i as NodeId;
        if tree.get(id).kind != Kind::Dir || owner[i] == u32::MAX || tree.get(id).zone == Zone::Vcs
        {
            continue;
        }
        if tree.get(id).git == Some(GitState::Ignored) {
            continue;
        }
        let mut any_tracked = false;
        let mut all_ignored = true;
        let mut any = false;
        for c in tree.children(id) {
            let n = tree.get(c);
            if n.zone == Zone::Vcs {
                continue;
            }
            any = true;
            match n.git {
                Some(GitState::Tracked) => {
                    any_tracked = true;
                    all_ignored = false;
                }
                Some(GitState::Ignored) => {}
                _ => all_ignored = false,
            }
        }
        tree.get_mut(id).git = Some(if any_tracked {
            GitState::Tracked
        } else if any && all_ignored {
            GitState::Ignored
        } else {
            GitState::Untracked
        });
    }
}
