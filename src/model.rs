//! The arena tree every stage reads and enriches, plus the snapshot types it serialises to.

use serde::Serialize;

pub type NodeId = u32;
pub const NO_PARENT: NodeId = NodeId::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// Sockets, FIFOs, devices.
    Special,
    /// Synthetic node created by pruning: "N smaller items" folded together.
    Rest,
}

/// Where a node comes from (its origin). Git state is tracked separately in `Node::git`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    /// Your code and content. Fully analysed.
    #[default]
    Source,
    /// Third-party code checked into the tree (`third_party/`, `vendor/`). Analysed, labelled.
    Vendored,
    /// Package-manager output (`node_modules/`, virtualenvs). Sized only.
    Dependency,
    /// Build output and caches (`target/`, `__pycache__/`, git-ignored build dirs). Sized only.
    Artifact,
    /// Version-control internals (`.git/`). Sized only.
    Vcs,
}

impl Zone {
    pub const ALL: [Zone; 5] = [
        Zone::Source,
        Zone::Vendored,
        Zone::Dependency,
        Zone::Artifact,
        Zone::Vcs,
    ];

    /// Origins whose files are read and analysed (lines, imports, history, duplicates).
    /// Git-ignored files are excluded separately (see `Node::is_analysed`).
    pub fn is_analysed(self) -> bool {
        matches!(self, Zone::Source | Zone::Vendored)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Zone::Source => "source",
            Zone::Vendored => "vendored",
            Zone::Dependency => "dependency",
            Zone::Artifact => "artifact",
            Zone::Vcs => "vcs",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Code,
    Docs,
    Config,
    Data,
    Image,
    Media,
    Archive,
    Binary,
    Font,
    /// Lockfiles, minified bundles, files marked `@generated` / `DO NOT EDIT`.
    Generated,
    #[default]
    Other,
}

impl Category {
    pub const ALL: [Category; 11] = [
        Category::Code,
        Category::Docs,
        Category::Config,
        Category::Data,
        Category::Image,
        Category::Media,
        Category::Archive,
        Category::Binary,
        Category::Font,
        Category::Generated,
        Category::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Code => "code",
            Category::Docs => "docs",
            Category::Config => "config",
            Category::Data => "data",
            Category::Image => "image",
            Category::Media => "media",
            Category::Archive => "archive",
            Category::Binary => "binary",
            Category::Font => "font",
            Category::Generated => "generated",
            Category::Other => "other",
        }
    }

    /// Categories whose files are worth reading as text.
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Category::Code
                | Category::Docs
                | Category::Config
                | Category::Data
                | Category::Generated
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitState {
    Tracked,
    Untracked,
    Ignored,
}

/// One entry in the arena. Directory metrics are recursive roll-ups of their subtree.
#[derive(Clone, Debug)]
pub struct Node {
    pub name: Box<str>,
    pub parent: NodeId,
    /// Number of nodes in this subtree including itself (DFS pre-order ⇒ `[id, id+span)`).
    pub span: u32,
    pub depth: u16,
    pub kind: Kind,
    pub zone: Zone,
    pub category: Category,
    /// Index into `languages::LANGS`.
    pub lang: Option<u16>,
    pub flags: Flags,

    /// Apparent size (sum of `len`). Hard links are counted at each path.
    pub bytes: u64,
    /// Allocated size on disk. Hard links are counted once (first path in sorted order).
    pub disk: u64,
    /// Number of regular files in the subtree (1 for a file).
    pub files: u32,
    /// Newest modification time in the subtree (unix seconds).
    pub mtime: i64,

    pub lines: u32,
    pub code: u32,
    pub comments: u32,
    pub blank: u32,
    /// Indentation complexity: Σ logical indent over code lines.
    pub complexity: u32,

    pub git: Option<GitState>,
    /// Commits inside the analysis window touching this file (sum over files for dirs).
    /// Excludes merges, bots, ignore-revs and very large commits.
    pub commits: u32,
    /// Recency-weighted revisions: Σ 0.5^(age / 90 days), age relative to the repo's
    /// newest commit (sum for dirs).
    pub recent: f32,
    /// Most recent commit time touching this node (unix seconds); 0 = none in window.
    pub last_commit: i64,
    /// Main author by degree-of-authorship; index into `Snapshot::authors`.
    pub owner: Option<u32>,
    /// Number of developers with authorship (normalised DOA > 0.75).
    pub authors: u16,
    /// Percentile(recent) × percentile(complexity) among analysed files, 0..1.
    pub hotspot: f32,

    /// Distinct internal files importing this one / imported by it.
    pub fan_in: u32,
    pub fan_out: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u16);

impl Flags {
    pub const BINARY: u16 = 1;
    pub const TEST: u16 = 1 << 1;
    pub const GENERATED: u16 = 1 << 2;
    /// A different filesystem mounted here; not descended (see `--cross-fs`).
    pub const MOUNT: u16 = 1 << 3;
    /// Could not be read (permissions, races). Sizes below are lower bounds.
    pub const UNREADABLE: u16 = 1 << 4;
    /// Root of a git work tree (has `.git`).
    pub const REPO: u16 = 1 << 5;
    /// A hard link whose disk usage is attributed to another path.
    pub const HARDLINK_DUP: u16 = 1 << 6;
    /// Content was too large to read (> `--max-file-size`).
    pub const TOO_LARGE: u16 = 1 << 7;
    /// Entry-point / manifest file (Cargo.toml, package.json, main.rs, …).
    pub const MANIFEST: u16 = 1 << 8;
    /// Cloud placeholder (iCloud dataless, OneDrive recall-on-access): never read.
    pub const PLACEHOLDER: u16 = 1 << 9;
    /// Name is not valid UTF-8; `name` is a lossy rendering.
    pub const NON_UTF8: u16 = 1 << 10;
    /// Directory already visited through another path (bind mount loop); not descended.
    pub const LOOP: u16 = 1 << 11;

    pub fn has(self, f: u16) -> bool {
        self.0 & f != 0
    }
    pub fn set(&mut self, f: u16) {
        self.0 |= f;
    }
}

impl Node {
    pub fn new(name: impl Into<Box<str>>, kind: Kind) -> Self {
        Node {
            name: name.into(),
            parent: NO_PARENT,
            span: 1,
            depth: 0,
            kind,
            zone: Zone::Source,
            category: Category::Other,
            lang: None,
            flags: Flags::default(),
            bytes: 0,
            disk: 0,
            files: 0,
            mtime: 0,
            lines: 0,
            code: 0,
            comments: 0,
            blank: 0,
            complexity: 0,
            git: None,
            commits: 0,
            recent: 0.0,
            last_commit: 0,
            owner: None,
            authors: 0,
            hotspot: 0.0,
            fan_in: 0,
            fan_out: 0,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }

    /// Whether the file's contents are analysed: authored origin, not git-ignored,
    /// not a cloud placeholder.
    pub fn is_analysed(&self) -> bool {
        self.zone.is_analysed()
            && self.git != Some(GitState::Ignored)
            && !self.flags.has(Flags::PLACEHOLDER)
    }
}

/// DFS pre-order arena. Node 0 is the root.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub fn root(&self) -> &Node {
        &self.nodes[0]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id as usize]
    }

    /// Ids of the direct children of `id`.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        let node = self.get(id);
        Children {
            tree: self,
            next: id + 1,
            end: id + node.span,
        }
    }

    /// Ids of every node in the subtree of `id`, including `id`.
    pub fn subtree(&self, id: NodeId) -> std::ops::Range<NodeId> {
        id..id + self.get(id).span
    }

    pub fn is_ancestor_or_self(&self, ancestor: NodeId, id: NodeId) -> bool {
        id >= ancestor && id < ancestor + self.get(ancestor).span
    }

    /// Path relative to the root, `/`-separated. The root itself is `""`.
    pub fn rel_path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != 0 && cur != NO_PARENT {
            let n = self.get(cur);
            parts.push(&*n.name);
            cur = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Path for display: relative path, or `.` for the root.
    pub fn display_path(&self, id: NodeId) -> String {
        if id == 0 {
            ".".to_string()
        } else {
            self.rel_path(id)
        }
    }

    /// Ancestors from parent up to root.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut cur = self.get(id).parent;
        std::iter::from_fn(move || {
            if cur == NO_PARENT {
                None
            } else {
                let out = cur;
                cur = self.get(cur).parent;
                Some(out)
            }
        })
    }

    /// Lowest common ancestor of two nodes.
    pub fn lca(&self, a: NodeId, b: NodeId) -> NodeId {
        let mut a = a;
        let mut b = b;
        while self.get(a).depth > self.get(b).depth {
            a = self.get(a).parent;
        }
        while self.get(b).depth > self.get(a).depth {
            b = self.get(b).parent;
        }
        while a != b {
            a = self.get(a).parent;
            b = self.get(b).parent;
        }
        a
    }

    /// The ancestor of `id` (or `id` itself) whose parent is `ancestor`.
    pub fn child_toward(&self, ancestor: NodeId, id: NodeId) -> NodeId {
        let mut cur = id;
        loop {
            let p = self.get(cur).parent;
            if p == ancestor || p == NO_PARENT {
                return cur;
            }
            cur = p;
        }
    }

    /// Find a node by `/`-separated relative path.
    pub fn find(&self, rel: &str) -> Option<NodeId> {
        let mut cur: NodeId = 0;
        for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
            cur = self.children(cur).find(|&c| &*self.get(c).name == part)?;
        }
        Some(cur)
    }

    /// Map of relative path → id for every node in the subtree of `base`, with paths
    /// relative to `base`. Used to join git / import data onto the tree.
    pub fn path_index(&self, base: NodeId) -> std::collections::HashMap<String, NodeId> {
        let mut map = std::collections::HashMap::with_capacity(self.get(base).span as usize);
        let mut prefix_stack: Vec<(NodeId, String)> = Vec::new();
        for id in self.subtree(base) {
            if id == base {
                continue;
            }
            let node = self.get(id);
            while let Some((top, _)) = prefix_stack.last() {
                if self.is_ancestor_or_self(*top, id) {
                    break;
                }
                prefix_stack.pop();
            }
            let path = match prefix_stack.last() {
                Some((_, p)) => format!("{p}/{}", node.name),
                None => node.name.to_string(),
            };
            if node.is_dir() {
                prefix_stack.push((id, path.clone()));
            }
            map.insert(path, id);
        }
        map
    }

    /// Recompute directory roll-ups (sizes, counts, lines, history sums, newest times).
    pub fn roll_up(&mut self) {
        for i in (0..self.nodes.len()).rev() {
            let node = &self.nodes[i];
            if node.is_dir() && node.span > 1 {
                // Children were already finalised (they come later in pre-order).
                let mut acc = Accum::default();
                let mut c = i + 1;
                let end = i + node.span as usize;
                while c < end {
                    acc.add(&self.nodes[c]);
                    c += self.nodes[c].span as usize;
                }
                acc.apply(&mut self.nodes[i]);
            }
        }
    }
}

#[derive(Default)]
struct Accum {
    bytes: u64,
    disk: u64,
    files: u32,
    mtime: i64,
    lines: u32,
    code: u32,
    comments: u32,
    blank: u32,
    complexity: u32,
    commits: u32,
    recent: f32,
    last_commit: i64,
    fan_in: u32,
    fan_out: u32,
}

impl Accum {
    fn add(&mut self, n: &Node) {
        self.bytes += n.bytes;
        self.disk += n.disk;
        self.files = self.files.saturating_add(n.files);
        self.mtime = self.mtime.max(n.mtime);
        self.lines = self.lines.saturating_add(n.lines);
        self.code = self.code.saturating_add(n.code);
        self.comments = self.comments.saturating_add(n.comments);
        self.blank = self.blank.saturating_add(n.blank);
        self.complexity = self.complexity.saturating_add(n.complexity);
        self.commits = self.commits.saturating_add(n.commits);
        self.recent += n.recent;
        self.last_commit = self.last_commit.max(n.last_commit);
        self.fan_in = self.fan_in.saturating_add(n.fan_in);
        self.fan_out = self.fan_out.saturating_add(n.fan_out);
    }

    fn apply(self, n: &mut Node) {
        n.bytes = self.bytes;
        n.disk = self.disk;
        n.files = self.files;
        n.mtime = self.mtime;
        n.lines = self.lines;
        n.code = self.code;
        n.comments = self.comments;
        n.blank = self.blank;
        n.complexity = self.complexity;
        n.commits = self.commits;
        n.recent = self.recent;
        n.last_commit = self.last_commit;
        n.fan_in = self.fan_in;
        n.fan_out = self.fan_out;
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeId,
    end: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        if self.next >= self.end {
            return None;
        }
        let id = self.next;
        self.next += self.tree.get(id).span;
        Some(id)
    }
}
