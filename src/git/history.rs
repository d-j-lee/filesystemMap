//! Commit history → revisions, recency, authorship (DOA), truck factor, change coupling,
//! monthly activity. Reads `git log --raw -z` (tree diffs only: no blob reads, safe on
//! partial clones). See decision D-08 for why each rule exists.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rayon::prelude::*;
use serde::Serialize;

use super::{PathIndex, Repo, git, run};
use crate::model::{Category, Flags, NodeId, Tree};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Window {
    All,
    /// Seconds before the repository's newest commit.
    Relative(i64),
    /// `YYYY-MM-DD`.
    Since(String),
}

impl Window {
    /// `all`, `2y`, `18m`, `6w`, `90d`, or a `YYYY-MM-DD` date.
    pub fn parse(s: &str) -> Result<Window, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("all") {
            return Ok(Window::All);
        }
        let bytes = s.as_bytes();
        if bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && s.replace('-', "").chars().all(|c| c.is_ascii_digit())
        {
            return Ok(Window::Since(s.to_string()));
        }
        let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
        let n: i64 = num
            .parse()
            .map_err(|_| format!("bad window {s:?}: use all, 2y, 18m, 6w, 90d or YYYY-MM-DD"))?;
        let day = 86_400;
        let secs = match unit {
            "y" => n * 365 * day,
            "m" => n * 30 * day,
            "w" => n * 7 * day,
            "d" => n * day,
            _ => return Err(format!("bad window unit in {s:?}: use y, m, w or d")),
        };
        Ok(Window::Relative(secs))
    }

    pub fn describe(&self) -> String {
        match self {
            Window::All => "all history".into(),
            Window::Relative(s) => format!("{} days before the newest commit", s / 86_400),
            Window::Since(d) => format!("since {d}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// History window. Relative windows are measured back from each repository's newest
    /// commit, not from today, so an archived project still gets its history analysed.
    pub window: Window,
    /// Safety cap on commits read per repository.
    pub max_commits: u32,
    /// Commits touching more files than this don't count as revisions.
    pub large_commit: usize,
    /// Only commits touching at most this many files feed change coupling.
    pub coupling_max_files: usize,
    /// Minimum shared commits for a coupling pair.
    pub coupling_min_shared: u32,
    pub half_life_days: f64,
    /// An author is active if they committed within this many days of the repo's newest commit.
    pub active_days: i64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            window: Window::Relative(2 * 365 * 86_400),
            max_commits: 30_000,
            large_commit: 50,
            coupling_max_files: 30,
            coupling_min_shared: 5,
            half_life_days: 90.0,
            active_days: 180,
        }
    }
}

pub const MONTHS: usize = 24;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// Non-merge commits read.
    pub commits: u32,
    /// Commits counted as revisions (after skipping bots, ignore-revs, large commits).
    pub counted: u32,
    pub skipped_bots: u32,
    pub skipped_ignore_revs: u32,
    pub skipped_large: u32,
    pub skipped_shallow: u32,
    pub first: i64,
    pub last: i64,
    /// The commit cap was hit: older history was not read.
    pub truncated: bool,
    pub authors: u32,
    pub active_authors: u32,
    pub truck_factor: Option<u32>,
    pub truck_authors: Vec<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouplingPair {
    pub a: NodeId,
    pub b: NodeId,
    pub shared: u32,
    /// P(B changes | A changes).
    pub conf_ab: f32,
    /// P(A changes | B changes).
    pub conf_ba: f32,
    pub lift: f32,
    /// Filled by the dependency stage: is there an import path (≤ 2 hops) between them?
    /// `None` when either language has no import extractor.
    pub linked: Option<bool>,
    /// A test file and the file it most likely tests.
    pub test_pair: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Truck {
    pub node: NodeId,
    pub factor: u32,
    pub authors: Vec<u32>,
    pub files: u32,
}

#[derive(Default)]
pub struct Authors {
    pub names: Vec<String>,
    index: HashMap<String, u32>,
    /// Newest commit time per author (any repo).
    pub last_active: Vec<i64>,
}

impl Authors {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_string());
        self.last_active.push(0);
        self.index.insert(name.to_string(), i);
        i
    }
}

#[derive(Default)]
pub struct Output {
    pub coupling: Vec<CouplingPair>,
    /// Monthly commit counts (index 0 = month ending at the repo's newest commit).
    pub activity: Vec<(NodeId, Vec<u16>)>,
    /// DOA authors of each file (for knowledge-risk and briefs).
    pub file_authors: HashMap<NodeId, Vec<u32>>,
    pub truck: Vec<Truck>,
    /// Authors active within `active_days` of their repo's newest commit.
    pub active: HashSet<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Add(String),
    Modify(String),
    Delete(String),
    Rename { from: String, to: String },
    Copy { to: String },
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub time: i64,
    pub author: String,
    pub email: String,
    pub changes: Vec<Change>,
}

/// Parse `git log --raw -z --no-abbrev --format=%x1e%H%x1f%ct%x1f%aN%x1f%aE`.
pub fn parse_log(bytes: &[u8]) -> Vec<Commit> {
    let mut commits = Vec::new();
    for rec in bytes.split(|&b| b == 0x1e).skip(1) {
        let Some(hdr_end) = rec.iter().position(|&b| b == 0) else {
            // Commit with no changes and no trailing NUL.
            if let Some(c) = parse_header(rec) {
                commits.push(c);
            }
            continue;
        };
        let Some(mut commit) = parse_header(&rec[..hdr_end]) else {
            continue;
        };
        let mut toks = rec[hdr_end + 1..]
            .split(|&b| b == 0)
            .map(|t| t.strip_prefix(b"\n").unwrap_or(t))
            .filter(|t| !t.is_empty());
        while let Some(meta) = toks.next() {
            if meta.first() != Some(&b':') {
                continue;
            }
            let meta = String::from_utf8_lossy(meta);
            let mut f = meta[1..].split(' ');
            let (src_mode, dst_mode) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
            let status = f.nth(2).unwrap_or("");
            let gitlink = src_mode == "160000" || dst_mode == "160000";
            let mut path = || toks.next().map(|p| String::from_utf8_lossy(p).into_owned());
            let change = match status.as_bytes().first() {
                Some(b'A') => path().map(Change::Add),
                Some(b'D') => path().map(Change::Delete),
                Some(b'R') => match (path(), path()) {
                    (Some(from), Some(to)) => Some(Change::Rename { from, to }),
                    _ => None,
                },
                Some(b'C') => match (path(), path()) {
                    (Some(_), Some(to)) => Some(Change::Copy { to }),
                    _ => None,
                },
                Some(_) => path().map(Change::Modify),
                None => None,
            };
            if let Some(c) = change
                && !gitlink
            {
                commit.changes.push(c);
            }
        }
        commits.push(commit);
    }
    commits
}

fn parse_header(h: &[u8]) -> Option<Commit> {
    let h = String::from_utf8_lossy(h);
    let mut f = h.split('\x1f');
    Some(Commit {
        hash: f.next()?.trim().to_string(),
        time: f.next()?.trim().parse().ok()?,
        author: f.next()?.to_string(),
        email: f.next().unwrap_or("").trim().to_string(),
        changes: Vec::new(),
    })
}

fn is_bot(c: &Commit) -> bool {
    let n = c.author.to_ascii_lowercase();
    let e = c.email.to_ascii_lowercase();
    n.contains("[bot]")
        || e.contains("[bot]")
        || matches!(
            n.as_str(),
            "dependabot" | "renovate" | "github-actions" | "greenkeeper" | "snyk-bot"
        )
}

struct RepoLog {
    commits: Vec<Commit>,
    shallow: HashSet<String>,
    ignore_revs: HashSet<String>,
}

fn collect(repo: &Repo, opts: &Options) -> Result<RepoLog> {
    let top = &repo.toplevel;
    let since = match &opts.window {
        Window::All => None,
        Window::Since(d) => Some(d.clone()),
        Window::Relative(secs) => {
            let mut last = git(top);
            last.args(["log", "-1", "--no-show-signature", "--format=%ct"]);
            String::from_utf8_lossy(&run(last)?)
                .trim()
                .parse::<i64>()
                .ok()
                .map(|newest| newest - secs)
                // A window reaching before 1970 means "everything".
                .filter(|&start| start > 0)
                .map(crate::util::date)
        }
    };
    // Inexact rename detection compares blob contents; in a partial clone those blobs are
    // not local (and lazy fetching is disabled), so only exact renames are detected there.
    let renames = if repo.partial { "-M100%" } else { "-M" };
    let mut log = log_command(top, opts, since.as_deref(), renames);
    let out = match run(log) {
        Ok(out) => out,
        Err(_) if !repo.partial => {
            log = log_command(top, opts, since.as_deref(), "-M100%");
            run(log)?
        }
        Err(e) => return Err(e),
    };
    // No pathspec even when the scan root is a subdirectory: a file moved into it from
    // elsewhere must keep its earlier history, and the large-commit rule must see whole
    // commits. Paths outside the scan are dropped when joined onto the tree.
    let commits = parse_log(&out);
    collect_rest(repo, commits)
}

fn log_command(
    top: &std::path::Path,
    opts: &Options,
    since: Option<&str>,
    renames: &str,
) -> std::process::Command {
    let mut c = git(top);
    c.args([
        "log",
        "--no-merges",
        renames,
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-show-signature",
        "--no-relative",
        "--no-textconv",
        "--no-ext-diff",
        "--no-color",
        "--format=%x1e%H%x1f%ct%x1f%aN%x1f%aE",
    ]);
    c.arg(format!("--max-count={}", opts.max_commits));
    if let Some(since) = since {
        c.arg(format!("--since={since}"));
    }
    c
}

fn collect_rest(repo: &Repo, commits: Vec<Commit>) -> Result<RepoLog> {
    let top = &repo.toplevel;

    let read_hashes = |path: std::path::PathBuf| -> HashSet<String> {
        std::fs::read_to_string(path)
            .map(|t| {
                t.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .filter_map(|l| l.split_whitespace().next())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let shallow = if repo.shallow {
        run({
            let mut c = git(top);
            c.args(["rev-parse", "--git-path", "shallow"]);
            c
        })
        .map(|p| {
            let p = std::path::PathBuf::from(String::from_utf8_lossy(&p).trim());
            read_hashes(if p.is_absolute() { p } else { top.join(p) })
        })
        .unwrap_or_default()
    } else {
        HashSet::new()
    };
    let ignore_revs = read_hashes(top.join(".git-blame-ignore-revs"));
    Ok(RepoLog {
        commits,
        shallow,
        ignore_revs,
    })
}

#[derive(Default)]
pub(crate) struct Acc {
    commits: u32,
    recent: f64,
    last: i64,
    creator: Option<u32>,
    deliveries: Vec<(u32, u32)>,
    tx: u32,
}

/// Read history for every repo and write per-file metrics onto the tree.
pub fn analyse(
    tree: &mut Tree,
    index: &PathIndex,
    repos: &mut [Repo],
    opts: &Options,
    authors: &mut Authors,
) -> Output {
    let logs: Vec<Result<RepoLog>> = repos.par_iter().map(|r| collect(r, opts)).collect();
    let owner = super::owners(tree, repos);
    let mut out = Output::default();

    for (ri, (repo, log)) in repos.iter_mut().zip(logs).enumerate() {
        if repo.error.is_some() {
            continue;
        }
        match log {
            Ok(log) => {
                let summary = process(
                    tree, index, &owner, ri as u32, repo, log, opts, authors, &mut out,
                );
                repo.history = Some(summary);
            }
            Err(e) => repo.error = Some(format!("history: {e}")),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn process(
    tree: &mut Tree,
    index: &PathIndex,
    owner: &[u32],
    ri: u32,
    repo: &Repo,
    log: RepoLog,
    opts: &Options,
    authors: &mut Authors,
    out: &mut Output,
) -> Summary {
    let mut s = Summary {
        commits: log.commits.len() as u32,
        truncated: log.commits.len() as u32 >= opts.max_commits,
        ..Default::default()
    };
    let Some(newest) = log.commits.first().map(|c| c.time) else {
        return s;
    };
    s.last = newest;
    s.first = log.commits.last().map_or(newest, |c| c.time);

    let repo_rel = if repo.node == 0 {
        String::new()
    } else {
        tree.rel_path(repo.node)
    };
    let mut node_cache: HashMap<String, Option<NodeId>> = HashMap::new();
    let mut node_of = |path: &str, tree: &Tree| -> Option<NodeId> {
        if let Some(v) = node_cache.get(path) {
            return *v;
        }
        let v = repo
            .to_scan_path(path, &repo_rel)
            .and_then(|p| index.get(&p))
            .filter(|&id| owner[id as usize] == ri && tree.get(id).is_file());
        node_cache.insert(path.to_string(), v);
        v
    };

    // Path (as of some older commit) → identity of the file it denotes today: `Some(path
    // at HEAD)`, or `None` when the file at that path no longer exists. Values are stored
    // fully resolved, so a lookup never chains — tombstoning a path cannot cut off a file
    // that was merely renamed *through* it.
    let mut alias: HashMap<String, Option<String>> = HashMap::new();
    let resolve = |alias: &HashMap<String, Option<String>>, p: &str| -> Option<String> {
        match alias.get(p) {
            Some(ident) => ident.clone(),
            None => Some(p.to_string()),
        }
    };

    let mut acc: HashMap<NodeId, Acc> = HashMap::new();
    let mut pairs: HashMap<(NodeId, NodeId), u32> = HashMap::new();
    let mut transactions = 0u32;
    let mut activity: HashMap<NodeId, Vec<u16>> = HashMap::new();
    let repo_depth = tree.get(repo.node).depth;
    let half_life = opts.half_life_days * 86_400.0;
    let mut repo_authors: HashSet<u32> = HashSet::new();

    for commit in &log.commits {
        if log.shallow.contains(&commit.hash) {
            // Boundary of a shallow clone: its diff is "everything added". Skip entirely.
            s.skipped_shallow += 1;
            continue;
        }
        let bot = is_bot(commit);
        let ignored = log.ignore_revs.contains(&commit.hash);
        let large = commit.changes.len() > opts.large_commit;
        let author = (!bot).then(|| authors.intern(&commit.author));
        if let Some(a) = author {
            repo_authors.insert(a);
            let la = &mut authors.last_active[a as usize];
            *la = (*la).max(commit.time);
        }
        let counts = !bot && !ignored && !large;
        if bot {
            s.skipped_bots += 1;
        } else if ignored {
            s.skipped_ignore_revs += 1;
        } else if large {
            s.skipped_large += 1;
        }
        if counts {
            s.counted += 1;
        }

        // Phase 1: everything except renames, against the alias state of newer commits.
        let mut touched: Vec<(NodeId, bool)> = Vec::new(); // (node, created here)
        for ch in &commit.changes {
            match ch {
                Change::Modify(p) => {
                    if let Some(id) = resolve(&alias, p).and_then(|cur| node_of(&cur, tree)) {
                        touched.push((id, false));
                    }
                }
                Change::Add(p) | Change::Copy { to: p } => {
                    if let Some(id) = resolve(&alias, p).and_then(|cur| node_of(&cur, tree)) {
                        touched.push((id, true));
                    }
                    // Anything older at this path is a different file.
                    alias.insert(p.clone(), None);
                }
                Change::Delete(_) | Change::Rename { .. } => {}
            }
        }
        // Phase 2: renames. The old path's older history belongs to the current file, and
        // anything older at the *destination* path was a different file. Resolve every
        // rename against the same alias state first, so renames within one commit
        // (a→b, b→c) don't see each other.
        let renames: Vec<(&String, &String, Option<String>)> = commit
            .changes
            .iter()
            .filter_map(|ch| match ch {
                Change::Rename { from, to } => Some((from, to, resolve(&alias, to))),
                _ => None,
            })
            .collect();
        for (_, to, cur) in &renames {
            if let Some(id) = cur.as_deref().and_then(|c| node_of(c, tree)) {
                touched.push((id, false));
            }
            alias.insert((*to).clone(), None);
        }
        for (from, _, cur) in renames {
            alias.insert(from.clone(), cur);
        }
        touched.sort_unstable();
        touched.dedup_by(|later, kept| {
            let same = later.0 == kept.0;
            if same {
                kept.1 |= later.1;
            }
            same
        });
        if bot || ignored {
            continue;
        }

        let age = (newest - commit.time).max(0) as f64;
        let w = 0.5f64.powf(age / half_life);
        for &(id, created) in &touched {
            let a = acc.entry(id).or_default();
            a.last = a.last.max(commit.time);
            if created && a.creator.is_none() {
                a.creator = author;
            }
            if counts {
                a.commits += 1;
                a.recent += w;
                if let Some(author) = author {
                    match a.deliveries.iter_mut().find(|(x, _)| *x == author) {
                        Some((_, n)) => *n += 1,
                        None => a.deliveries.push((author, 1)),
                    }
                }
            }
        }
        // Change coupling over small, ordinary commits.
        if counts && touched.len() >= 2 && touched.len() <= opts.coupling_max_files {
            transactions += 1;
            for (i, &(x, _)) in touched.iter().enumerate() {
                acc.entry(x).or_default().tx += 1;
                for &(y, _) in &touched[i + 1..] {
                    *pairs.entry((x, y)).or_insert(0) += 1;
                }
            }
        } else if counts && touched.len() == 1 {
            transactions += 1;
            acc.entry(touched[0].0).or_default().tx += 1;
        }
        // Monthly activity for directories up to three levels below the repo root.
        let month = (age / (30.44 * 86_400.0)) as usize;
        if counts && month < MONTHS {
            let mut dirs: HashSet<NodeId> = HashSet::new();
            for &(id, _) in &touched {
                for d in tree.ancestors(id) {
                    if tree.get(d).depth < repo_depth || !tree.is_ancestor_or_self(repo.node, d) {
                        break;
                    }
                    if tree.get(d).depth <= repo_depth + 3 {
                        dirs.insert(d);
                    }
                }
            }
            for d in dirs {
                let v = activity.entry(d).or_insert_with(|| vec![0; MONTHS]);
                v[month] = v[month].saturating_add(1);
            }
        }
    }

    // Apply per-file metrics and authorship.
    let active_cutoff = newest - opts.active_days * 86_400;
    let mut file_authors: Vec<(NodeId, Vec<u32>)> = Vec::new();
    for (&id, a) in &acc {
        let n = tree.get_mut(id);
        n.commits = a.commits;
        n.recent = a.recent as f32;
        n.last_commit = a.last;
        let (owner, auths) = degree_of_authorship(a);
        n.owner = owner;
        n.authors = auths.len() as u16;
        if !auths.is_empty() {
            file_authors.push((id, auths));
        }
    }
    file_authors.sort_unstable_by_key(|(id, _)| *id);

    // Truck factor for the repo and each top-level directory with enough history.
    let mut scopes = vec![repo.node];
    scopes.extend(tree.children(repo.node).filter(|&c| tree.get(c).is_dir()));
    for scope in scopes {
        let files: Vec<&Vec<u32>> = file_authors
            .iter()
            .filter(|(id, _)| tree.is_ancestor_or_self(scope, *id))
            .map(|(_, a)| a)
            .collect();
        if files.len() < if scope == repo.node { 1 } else { 10 } {
            continue;
        }
        let (factor, top) = truck_factor(&files);
        if scope == repo.node {
            s.truck_factor = Some(factor);
            s.truck_authors = top.clone();
        }
        out.truck.push(Truck {
            node: scope,
            factor,
            authors: top,
            files: files.len() as u32,
        });
    }

    s.authors = repo_authors.len() as u32;
    for &a in &repo_authors {
        if authors.last_active[a as usize] >= active_cutoff {
            out.active.insert(a);
            s.active_authors += 1;
        }
    }

    // Coupling pairs: support, confidence both ways, lift.
    let n_tx = f64::from(transactions.max(1));
    for ((x, y), shared) in pairs {
        if shared < opts.coupling_min_shared {
            continue;
        }
        let (tx_x, tx_y) = (acc[&x].tx.max(1), acc[&y].tx.max(1));
        let lift = (f64::from(shared) * n_tx / (f64::from(tx_x) * f64::from(tx_y))) as f32;
        let conf_ab = shared as f32 / tx_x as f32;
        let conf_ba = shared as f32 / tx_y as f32;
        if lift < 2.0 || conf_ab.max(conf_ba) < 0.3 {
            continue;
        }
        if tree.get(x).category == Category::Generated
            || tree.get(y).category == Category::Generated
        {
            continue; // lockfiles etc. co-change with their manifests by construction
        }
        let tx = tree.get(x).flags.has(Flags::TEST);
        let ty = tree.get(y).flags.has(Flags::TEST);
        let test_pair = tx != ty && same_stem(&tree.get(x).name, &tree.get(y).name);
        out.coupling.push(CouplingPair {
            a: x,
            b: y,
            shared,
            conf_ab,
            conf_ba,
            lift,
            linked: None,
            test_pair,
        });
    }
    out.coupling.sort_by(|p, q| {
        let sp = p.shared as f32 * p.conf_ab.max(p.conf_ba);
        let sq = q.shared as f32 * q.conf_ab.max(q.conf_ba);
        sq.total_cmp(&sp).then(p.a.cmp(&q.a)).then(p.b.cmp(&q.b))
    });
    out.coupling.truncate(300);

    let mut act: Vec<(NodeId, Vec<u16>)> = activity.into_iter().collect();
    act.sort_unstable_by_key(|(id, _)| *id);
    out.activity.extend(act);
    out.file_authors.extend(file_authors);
    s
}

/// `foo_test.go` ~ `foo.go`, `test_foo.py` ~ `foo.py`, `Foo.spec.ts` ~ `Foo.ts`.
fn same_stem(a: &str, b: &str) -> bool {
    fn core(n: &str) -> String {
        let stem = n.split('.').next().unwrap_or(n).to_ascii_lowercase();
        let stem = stem.strip_prefix("test_").unwrap_or(&stem).to_string();
        for suf in [
            "_test",
            "_tests",
            "_spec",
            "_unittest",
            "_browsertest",
            "test",
            "tests",
        ] {
            if let Some(s) = stem.strip_suffix(suf)
                && !s.is_empty()
            {
                return s.trim_end_matches('_').to_string();
            }
        }
        stem
    }
    core(a) == core(b)
}

/// Degree-of-authorship (Fritz et al. 2014; thresholds from Avelino et al. 2016):
/// DOA = 3.293 + 1.098·FA + 0.164·DL − 0.321·ln(1 + AC). Authors: normalised DOA > 0.75
/// and absolute DOA ≥ 3.293. Returns (top author, authors).
pub(crate) fn degree_of_authorship(a: &Acc) -> (Option<u32>, Vec<u32>) {
    let total: u32 = a.deliveries.iter().map(|(_, n)| n).sum();
    let mut cands: Vec<(u32, f64)> = a
        .deliveries
        .iter()
        .map(|&(who, dl)| (who, doa(a.creator == Some(who), dl, total - dl)))
        .collect();
    if let Some(c) = a.creator
        && !a.deliveries.iter().any(|(w, _)| *w == c)
    {
        cands.push((c, doa(true, 0, total)));
    }
    let Some(max) = cands.iter().map(|c| c.1).max_by(f64::total_cmp) else {
        return (None, Vec::new());
    };
    cands.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
    let owner = cands.first().map(|c| c.0);
    let auths = cands
        .iter()
        .filter(|(_, d)| *d / max > 0.75 && *d >= 3.293)
        .map(|c| c.0)
        .collect();
    (owner, auths)
}

fn doa(first_author: bool, deliveries: u32, accepts: u32) -> f64 {
    3.293 + 1.098 * f64::from(u8::from(first_author)) + 0.164 * f64::from(deliveries)
        - 0.321 * (1.0 + f64::from(accepts)).ln()
}

/// Avelino et al.'s greedy truck factor: remove the author of most files until more
/// than half the files have no remaining author. Returns (factor, removed authors).
pub fn truck_factor(files: &[&Vec<u32>]) -> (u32, Vec<u32>) {
    let mut removed: HashSet<u32> = HashSet::new();
    let mut order = Vec::new();
    let total = files.len();
    loop {
        let orphaned = files
            .iter()
            .filter(|a| a.iter().all(|x| removed.contains(x)))
            .count();
        if orphaned * 2 > total {
            break;
        }
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for f in files {
            for &x in f.iter() {
                if !removed.contains(&x) {
                    *counts.entry(x).or_insert(0) += 1;
                }
            }
        }
        let Some((&top, _)) = counts.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0))) else {
            break;
        };
        removed.insert(top);
        order.push(top);
    }
    (order.len() as u32, order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_z() {
        let mut b = Vec::new();
        b.extend_from_slice(b"\x1eaaa\x1f100\x1fAlice\x1fa@x\0\n");
        b.extend_from_slice(b":100644 100644 1111 2222 M\0src/a.rs\0");
        b.extend_from_slice(b":100644 100644 1111 2222 R087\0old name.rs\0new\tname.rs\0");
        b.extend_from_slice(b":000000 160000 0000 3333 A\0sub\0");
        b.extend_from_slice(
            b"\x1ebbb\x1f90\x1fBob\x1fb@x\0\n:000000 100644 0000 1111 A\0src/a.rs\0",
        );
        let c = parse_log(&b);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].changes.len(), 2, "gitlink skipped");
        assert_eq!(
            c[0].changes[1],
            Change::Rename {
                from: "old name.rs".into(),
                to: "new\tname.rs".into()
            }
        );
        assert_eq!(c[1].changes, vec![Change::Add("src/a.rs".into())]);
        assert_eq!(c[1].time, 90);
    }

    #[test]
    fn doa_prefers_creator_and_frequent_committer() {
        let a = Acc {
            creator: Some(1),
            deliveries: vec![(1, 3), (2, 10)],
            ..Default::default()
        };
        let (owner, auths) = degree_of_authorship(&a);
        assert_eq!(owner, Some(2));
        assert!(auths.contains(&2));
        let solo = Acc {
            creator: Some(7),
            deliveries: vec![(7, 1)],
            ..Default::default()
        };
        assert_eq!(degree_of_authorship(&solo), (Some(7), vec![7]));
    }

    #[test]
    fn truck_factor_greedy() {
        let f1 = vec![1];
        let f2 = vec![1];
        let f3 = vec![2];
        let f4 = vec![1, 2];
        let files = vec![&f1, &f2, &f3, &f4];
        // Remove 1 → f1,f2 orphaned (2/4, not > half) → remove 2 → all orphaned.
        assert_eq!(truck_factor(&files).0, 2);
        let g = [vec![1], vec![1], vec![1], vec![2]];
        let refs: Vec<&Vec<u32>> = g.iter().collect();
        assert_eq!(truck_factor(&refs), (1, vec![1]));
    }

    #[test]
    fn windows() {
        assert_eq!(Window::parse("all"), Ok(Window::All));
        assert_eq!(Window::parse("2y"), Ok(Window::Relative(2 * 365 * 86_400)));
        assert_eq!(Window::parse("90d"), Ok(Window::Relative(90 * 86_400)));
        assert_eq!(
            Window::parse("2024-01-31"),
            Ok(Window::Since("2024-01-31".into()))
        );
        assert!(Window::parse("2 years ago").is_err());
    }

    #[test]
    fn stems() {
        assert!(same_stem("foo_test.go", "foo.go"));
        assert!(same_stem("test_foo.py", "foo.py"));
        assert!(same_stem("Foo.spec.ts", "Foo.ts"));
        assert!(same_stem("values_unittest.cc", "values.cc"));
        assert!(!same_stem("bar.rs", "foo.rs"));
    }
}
