//! The versioned JSON contract (`schema: "fsmap/1"`) shared by `scan --json`, the HTML
//! report and `diff`. Field reference: docs/snapshot-schema.md.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use serde::ser::{SerializeSeq, Serializer};

use crate::analysis::{Analysis, AuthorMode, Hotspot};
use crate::classify::{self, ZoneRoot};
use crate::deps::Graph;
use crate::dupes::Group;
use crate::git::Repo;
use crate::git::history::{CouplingPair, Truck};
use crate::insights::Finding;
use crate::model::{Category, Flags, GitState, Kind, NO_PARENT, Node, NodeId, Tree, Zone};
use crate::scan::ScanIssues;

pub const SCHEMA: &str = "fsmap/1";

pub const FLAG_NAMES: [&str; 12] = [
    "binary",
    "test",
    "generated",
    "mount",
    "unreadable",
    "repo",
    "hardlinkDup",
    "tooLarge",
    "manifest",
    "placeholder",
    "nonUtf8",
    "loop",
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot<'a> {
    pub schema: &'static str,
    pub tool: String,
    pub root: String,
    pub generated_at: i64,
    pub options: OptionsSummary,
    pub totals: Totals,
    pub flag_names: [&'static str; 12],
    pub languages: Vec<LangInfo>,
    #[serde(serialize_with = "ser_nodes")]
    pub nodes: &'a Tree,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repos_not_analysed: Option<usize>,
    #[serde(flatten)]
    pub parts: &'a Parts,
    pub scan_issues: &'a ScanIssues,
    pub timings_ms: BTreeMap<&'static str, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned: Option<PruneInfo>,
}

/// Everything in a snapshot that refers to node ids (so it can be remapped after pruning).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Parts {
    pub zone_roots: Vec<ZoneRoot>,
    pub repos: Vec<Repo>,
    pub hotspots: Vec<Hotspot>,
    pub coupling: Vec<CouplingPair>,
    pub activity: Vec<(NodeId, Vec<u16>)>,
    pub truck: Vec<Truck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deps: Option<Graph>,
    pub duplicates: Vec<Group>,
    pub findings: Vec<Finding>,
}

impl Parts {
    pub fn new(a: &Analysis, findings: &[Finding]) -> Self {
        let hide = a.options.authors == AuthorMode::None;
        Parts {
            zone_roots: a.zone_roots.clone(),
            repos: a.repos.clone(),
            hotspots: a.hotspots.iter().take(200).copied().collect(),
            coupling: a.history.coupling.clone(),
            activity: a.history.activity.clone(),
            truck: a
                .history
                .truck
                .iter()
                .map(|t| Truck {
                    authors: if hide { Vec::new() } else { t.authors.clone() },
                    ..t.clone()
                })
                .collect(),
            deps: a.graph.clone(),
            duplicates: a.duplicates.iter().take(200).cloned().collect(),
            findings: findings.to_vec(),
        }
    }

    /// Node ids every view needs to exist (kept through pruning).
    pub fn pinned(&self) -> HashSet<NodeId> {
        let mut p = HashSet::new();
        p.extend(self.zone_roots.iter().map(|z| z.node));
        p.extend(self.repos.iter().map(|r| r.node));
        p.extend(self.hotspots.iter().map(|h| h.node));
        p.extend(self.coupling.iter().flat_map(|c| [c.a, c.b]));
        p.extend(self.activity.iter().map(|a| a.0));
        p.extend(self.truck.iter().map(|t| t.node));
        p.extend(
            self.duplicates
                .iter()
                .take(50)
                .flat_map(|d| d.nodes.iter().copied()),
        );
        p.extend(self.findings.iter().flat_map(|f| f.nodes.iter().copied()));
        if let Some(g) = &self.deps {
            for c in &g.cycles {
                p.insert(c.parent);
                p.extend(c.members.iter().copied());
                p.extend(c.cut_files.iter().flat_map(|&(a, b)| [a, b]));
            }
        }
        p
    }

    /// Rewrite node ids after pruning (`map[old] = new`).
    pub fn remap(&mut self, map: &[NodeId]) {
        let m = |id: &mut NodeId| *id = map[*id as usize];
        self.zone_roots.iter_mut().for_each(|z| m(&mut z.node));
        self.repos.iter_mut().for_each(|r| m(&mut r.node));
        self.hotspots.iter_mut().for_each(|h| m(&mut h.node));
        for c in &mut self.coupling {
            m(&mut c.a);
            m(&mut c.b);
        }
        self.activity.iter_mut().for_each(|a| m(&mut a.0));
        self.truck.iter_mut().for_each(|t| m(&mut t.node));
        for d in &mut self.duplicates {
            d.nodes.iter_mut().for_each(m);
        }
        for f in &mut self.findings {
            f.nodes.iter_mut().for_each(m);
        }
        if let Some(g) = &mut self.deps {
            let mut merged: std::collections::HashMap<(NodeId, NodeId), (u32, bool, bool)> =
                std::collections::HashMap::new();
            for e in &g.edges {
                let (a, b) = (map[e.from as usize], map[e.to as usize]);
                if a != b {
                    let entry = merged.entry((a, b)).or_insert((0, true, true));
                    entry.0 += e.weight;
                    entry.1 &= e.up;
                    entry.2 &= e.weak;
                }
            }
            g.edges = merged
                .into_iter()
                .map(|((from, to), (weight, up, weak))| crate::deps::Edge {
                    from,
                    to,
                    weight,
                    up,
                    weak,
                })
                .collect();
            g.edges.sort_unstable_by_key(|e| (e.from, e.to));
            for c in &mut g.cycles {
                m(&mut c.parent);
                c.members.iter_mut().for_each(m);
                m(&mut c.cut.0);
                m(&mut c.cut.1);
                for (a, b) in &mut c.cut_files {
                    m(a);
                    m(b);
                }
            }
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PruneInfo {
    pub original_nodes: usize,
    pub kept_nodes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionsSummary {
    pub git: bool,
    pub since: Option<String>,
    pub max_commits: u32,
    pub deps: bool,
    pub duplicates: bool,
    pub cross_fs: bool,
    pub authors: AuthorMode,
}

#[derive(Serialize)]
pub struct LangInfo {
    pub name: &'static str,
    pub color: &'static str,
}

#[derive(Serialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub files: u64,
    pub bytes: u64,
    pub disk: u64,
    pub code: u64,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub disk: u64,
    pub lines: u64,
    pub code: u64,
    pub comments: u64,
    /// Rough LLM token cost of analysed text (bytes / 4).
    pub tokens: u64,
    pub by_zone: BTreeMap<&'static str, Bucket>,
    pub by_category: BTreeMap<&'static str, Bucket>,
    pub by_git: BTreeMap<&'static str, Bucket>,
    pub by_language: BTreeMap<&'static str, Bucket>,
    /// Disk bytes saved by hard links (attributed to another path).
    pub hardlink_shared: u64,
}

pub fn totals(tree: &Tree) -> Totals {
    let mut t = Totals::default();
    for n in &tree.nodes {
        match n.kind {
            Kind::Dir => t.dirs += 1,
            Kind::File => {}
            _ => continue,
        }
        if n.kind != Kind::File {
            continue;
        }
        t.files += 1;
        t.bytes += n.bytes;
        t.disk += n.disk;
        t.lines += u64::from(n.lines);
        t.code += u64::from(n.code);
        t.comments += u64::from(n.comments);
        if n.is_analysed() && n.lines > 0 {
            t.tokens += n.bytes / 4;
        }
        if n.flags.has(Flags::HARDLINK_DUP) {
            t.hardlink_shared += n.bytes;
        }
        let add = |b: &mut Bucket| {
            b.files += 1;
            b.bytes += n.bytes;
            b.disk += n.disk;
            b.code += u64::from(n.code);
        };
        add(t.by_zone.entry(n.zone.as_str()).or_default());
        add(t.by_category.entry(n.category.as_str()).or_default());
        let g = match n.git {
            Some(GitState::Tracked) => "tracked",
            Some(GitState::Untracked) => "untracked",
            Some(GitState::Ignored) => "ignored",
            None => "none",
        };
        add(t.by_git.entry(g).or_default());
        if let Some(l) = n.lang {
            add(t.by_language.entry(classify::lang(l).name).or_default());
        }
    }
    t
}

impl<'a> Snapshot<'a> {
    pub fn new(
        a: &'a Analysis,
        tree: &'a Tree,
        parts: &'a Parts,
        pruned: Option<PruneInfo>,
    ) -> Self {
        let o = &a.options;
        Snapshot {
            schema: SCHEMA,
            tool: format!("fsmap {}", env!("CARGO_PKG_VERSION")),
            root: a.root.to_string_lossy().into_owned(),
            generated_at: a.generated_at,
            options: OptionsSummary {
                git: a.git_available,
                since: o.history.since.clone(),
                max_commits: o.history.max_commits,
                deps: o.deps,
                duplicates: o.dupes.is_some(),
                cross_fs: o.scan.cross_fs,
                authors: o.authors,
            },
            totals: totals(tree),
            flag_names: FLAG_NAMES,
            languages: classify::LANGS
                .iter()
                .map(|l| LangInfo {
                    name: l.name,
                    color: l.color,
                })
                .collect(),
            nodes: tree,
            authors: if o.authors == AuthorMode::None {
                Vec::new()
            } else {
                a.authors.names.clone()
            },
            repos_not_analysed: (a.repos_total > a.repos.len())
                .then(|| a.repos_total - a.repos.len()),
            parts,
            scan_issues: &a.issues,
            timings_ms: a.timings.iter().copied().collect(),
            pruned,
        }
    }
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeOut<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<NodeId>,
    kind: Kind,
    #[serde(skip_serializing_if = "is_source")]
    zone: Zone,
    #[serde(skip_serializing_if = "is_other")]
    cat: Category,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<u16>,
    #[serde(skip_serializing_if = "is_zero_u16")]
    flags: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitState>,
    #[serde(skip_serializing_if = "is_zero_u64")]
    bytes: u64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    disk: u64,
    #[serde(skip_serializing_if = "is_zero_u32")]
    files: u32,
    #[serde(skip_serializing_if = "is_zero_i64")]
    mtime: i64,
    #[serde(skip_serializing_if = "is_zero_u32")]
    lines: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    code: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    comments: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    cx: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    commits: u32,
    #[serde(skip_serializing_if = "is_zero_f32")]
    recent: f32,
    #[serde(skip_serializing_if = "is_zero_i64")]
    last_commit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<u32>,
    #[serde(skip_serializing_if = "is_zero_u16")]
    authors: u16,
    #[serde(skip_serializing_if = "is_zero_f32")]
    hotspot: f32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    fan_in: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    fan_out: u32,
}

fn is_source(z: &Zone) -> bool {
    *z == Zone::Source
}
fn is_other(c: &Category) -> bool {
    *c == Category::Other
}

fn round3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

impl<'a> From<&'a Node> for NodeOut<'a> {
    fn from(n: &'a Node) -> Self {
        NodeOut {
            name: &n.name,
            parent: (n.parent != NO_PARENT).then_some(n.parent),
            kind: n.kind,
            zone: n.zone,
            cat: n.category,
            lang: n.lang,
            flags: n.flags.0,
            git: n.git,
            bytes: n.bytes,
            disk: n.disk,
            files: if n.kind == Kind::File { 0 } else { n.files },
            mtime: n.mtime,
            lines: n.lines,
            code: n.code,
            comments: n.comments,
            cx: n.complexity,
            commits: n.commits,
            recent: round3(n.recent),
            last_commit: n.last_commit,
            owner: n.owner,
            authors: n.authors,
            hotspot: round3(n.hotspot),
            fan_in: n.fan_in,
            fan_out: n.fan_out,
        }
    }
}

fn ser_nodes<S: Serializer>(tree: &&Tree, s: S) -> Result<S::Ok, S::Error> {
    let mut seq = s.serialize_seq(Some(tree.len()))?;
    for n in &tree.nodes {
        seq.serialize_element(&NodeOut::from(n))?;
    }
    seq.end()
}

// FLAG_NAMES[i] names bit i of `model::Flags`.
const _: () = assert!(Flags::LOOP == 1 << (FLAG_NAMES.len() - 1));
