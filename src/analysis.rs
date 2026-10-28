//! Pipeline orchestration: scan → classify → git status → content → history → imports →
//! hotspots → roll-up → duplicates. Every stage after `scan` is optional.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use crate::classify::{self, ZoneRoot};
use crate::content;
use crate::deps::{self, Graph};
use crate::dupes;
use crate::git::history::{self, Authors};
use crate::git::{self, PathIndex, Repo};
use crate::model::{Category, Flags, NodeId, Tree};
use crate::scan::{self, ScanIssues, ScanOptions};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AuthorMode {
    /// Real names (as mapped by `.mailmap`). Emails are never output.
    #[default]
    Names,
    /// Stable pseudonyms (`dev-1a2b3c`), for sharing reports outside the team.
    Hashed,
    /// No author data at all.
    None,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub scan: ScanOptions,
    /// Read file contents (lines, complexity, imports, generated markers).
    pub read_content: bool,
    pub content: content::Options,
    pub git: bool,
    pub history: history::Options,
    pub max_repos: usize,
    pub deps: bool,
    pub dupes: Option<dupes::Options>,
    pub authors: AuthorMode,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            scan: ScanOptions::default(),
            read_content: true,
            content: content::Options::default(),
            git: true,
            history: history::Options::default(),
            max_repos: 64,
            deps: true,
            dupes: Some(dupes::Options::default()),
            authors: AuthorMode::Names,
        }
    }
}

/// Shared progress state, polled by the CLI's progress display.
#[derive(Default)]
pub struct Progress {
    pub count: AtomicU64,
    pub total: AtomicU64,
    phase: Mutex<&'static str>,
}

impl Progress {
    pub fn start(&self, phase: &'static str, total: u64) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
        self.count.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn phase(&self) -> &'static str {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hotspot {
    pub node: NodeId,
    /// Percentile of recency-weighted revisions among files with history.
    pub change_pct: f32,
    /// Percentile of indentation complexity among the same files.
    pub complexity_pct: f32,
    pub score: f32,
}

impl Hotspot {
    /// Flagged in findings: non-test file in the top quintile on both axes with real
    /// recent activity.
    pub fn flagged(&self, tree: &Tree) -> bool {
        let n = tree.get(self.node);
        self.change_pct >= 0.8
            && self.complexity_pct >= 0.8
            && n.commits >= 5
            && n.recent >= 1.0
            && !n.flags.has(Flags::TEST)
    }
}

pub struct Analysis {
    pub root: PathBuf,
    pub tree: Tree,
    pub zone_roots: Vec<ZoneRoot>,
    pub issues: ScanIssues,
    pub git_available: bool,
    pub repos: Vec<Repo>,
    pub repos_total: usize,
    pub authors: Authors,
    pub history: history::Output,
    pub graph: Option<Graph>,
    pub hotspots: Vec<Hotspot>,
    pub duplicates: Vec<dupes::Group>,
    pub timings: Vec<(&'static str, u64)>,
    pub options: Options,
    pub generated_at: i64,
}

pub fn run(path: &Path, opts: &Options, progress: &Progress) -> Result<Analysis> {
    let mut timings = Vec::new();
    let mut lap = Instant::now();
    let mut tick = |name: &'static str, timings: &mut Vec<(&'static str, u64)>| {
        timings.push((name, lap.elapsed().as_millis() as u64));
        lap = Instant::now();
    };

    progress.start("scanning", 0);
    let scanned = scan::scan(path, &opts.scan, &progress.count)?;
    let root = scanned.root_path;
    let mut tree = scanned.tree;
    tick("scan", &mut timings);

    let mut zone_roots = classify::classify_tree(&mut tree);
    let index = PathIndex::new(&tree);
    tick("classify", &mut timings);

    let git_dir = if root.is_dir() {
        root.as_path()
    } else {
        root.parent().unwrap_or(&root)
    };
    let git_available = opts.git && git::available(git_dir);
    let (mut repos, repos_total) = if git_available {
        progress.start("git status", 0);
        let (mut repos, total) = git::discover(&tree, &root, opts.max_repos);
        git::apply_status(&mut tree, &index, &mut repos);
        classify::settle_tracked_outputs(&mut tree, &mut zone_roots);
        (repos, total)
    } else {
        (Vec::new(), 0)
    };
    tick("git status", &mut timings);

    let readable = (0..tree.len() as NodeId)
        .filter(|&id| tree.get(id).is_file() && tree.get(id).is_analysed())
        .count() as u64;
    progress.start("reading files", readable);
    let content_opts = content::Options {
        imports: opts.deps,
        ..opts.content.clone()
    };
    let sources = if opts.read_content {
        content::analyse(&mut tree, &root, &content_opts, &progress.count)
    } else {
        deps::Sources::default()
    };
    tick("content", &mut timings);

    let mut authors = Authors::default();
    let mut hist = if git_available && !repos.is_empty() {
        progress.start("git history", 0);
        history::analyse(&mut tree, &index, &mut repos, &opts.history, &mut authors)
    } else {
        history::Output::default()
    };
    tick("history", &mut timings);

    let graph = (opts.deps && opts.read_content).then(|| {
        progress.start("imports", 0);
        deps::build(&mut tree, &index, &sources, &mut hist.coupling)
    });
    drop(sources);
    drop(index);
    tick("imports", &mut timings);

    let hotspots = score_hotspots(&mut tree);
    tree.roll_up();

    let duplicates = match &opts.dupes {
        Some(d) => {
            progress.start("duplicates", 0);
            dupes::find(&tree, &root, d)
        }
        None => Vec::new(),
    };
    tick("duplicates", &mut timings);

    apply_author_mode(&mut authors, opts.authors);
    if opts.authors == AuthorMode::None {
        for n in &mut tree.nodes {
            n.owner = None;
        }
        for h in repos.iter_mut().filter_map(|r| r.history.as_mut()) {
            h.truck_authors.clear();
        }
    }

    Ok(Analysis {
        root,
        tree,
        zone_roots,
        issues: scanned.issues,
        git_available,
        repos,
        repos_total,
        authors,
        history: hist,
        graph,
        hotspots,
        duplicates,
        timings,
        options: opts.clone(),
        generated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    })
}

/// Percentile × percentile of recency-weighted revisions and complexity (D-08), within
/// each repository.
fn score_hotspots(tree: &mut Tree) -> Vec<Hotspot> {
    let cands: Vec<NodeId> = (0..tree.len() as NodeId)
        .filter(|&id| {
            let n = tree.get(id);
            n.is_file()
                && n.is_analysed()
                && n.category == Category::Code
                && !n.flags.has(Flags::GENERATED)
                && n.commits > 0
                && n.complexity > 0
        })
        .collect();
    if cands.is_empty() {
        return Vec::new();
    }
    let pct = |vals: Vec<f64>| -> Vec<f32> {
        // Percentile = share of candidates with a value ≤ this one (ties share the top rank).
        let mut order: Vec<usize> = (0..vals.len()).collect();
        order.sort_by(|&a, &b| vals[a].total_cmp(&vals[b]));
        let n = vals.len() as f32;
        let mut out = vec![0f32; vals.len()];
        let mut i = 0;
        while i < order.len() {
            let mut j = i;
            while j + 1 < order.len() && vals[order[j + 1]] == vals[order[i]] {
                j += 1;
            }
            for &k in &order[i..=j] {
                out[k] = (j + 1) as f32 / n;
            }
            i = j + 1;
        }
        out
    };
    // Percentiles are taken within each repository: in a multi-repo scan, a C codebase's
    // 5k-line files must not push every other repository's hotspots off the scale.
    let repo_of = |id: NodeId| -> NodeId {
        tree.ancestors(id)
            .find(|&a| tree.get(a).flags.has(Flags::REPO))
            .unwrap_or(0)
    };
    let mut groups: std::collections::BTreeMap<NodeId, Vec<NodeId>> =
        std::collections::BTreeMap::new();
    for &id in &cands {
        groups.entry(repo_of(id)).or_default().push(id);
    }
    let mut hot: Vec<Hotspot> = Vec::with_capacity(cands.len());
    for members in groups.values() {
        let pr = pct(members
            .iter()
            .map(|&id| f64::from(tree.get(id).recent))
            .collect());
        let pc = pct(members
            .iter()
            .map(|&id| f64::from(tree.get(id).complexity))
            .collect());
        hot.extend(members.iter().enumerate().map(|(i, &id)| Hotspot {
            node: id,
            change_pct: pr[i],
            complexity_pct: pc[i],
            score: pr[i] * pc[i],
        }));
    }
    for h in &hot {
        tree.get_mut(h.node).hotspot = h.score;
    }
    hot.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    hot
}

fn apply_author_mode(authors: &mut Authors, mode: AuthorMode) {
    match mode {
        AuthorMode::Names => {}
        AuthorMode::Hashed => {
            for n in &mut authors.names {
                *n = format!(
                    "dev-{:06x}",
                    xxhash_rust::xxh3::xxh3_64(n.as_bytes()) & 0xff_ffff
                );
            }
        }
        AuthorMode::None => {
            for (i, n) in authors.names.iter_mut().enumerate() {
                *n = format!("author {}", i + 1);
            }
        }
    }
}
