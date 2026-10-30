//! Parallel filesystem walk → arena tree.
//!
//! One `lstat` per entry (via `DirEntry::metadata`, which does not follow symlinks),
//! fork-join parallelism per directory with rayon, children sorted by name so output is
//! deterministic. Unreadable entries are recorded, never fatal.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use globset::GlobSet;
use rayon::prelude::*;

use crate::model::{Flags, Kind, NO_PARENT, Node, Tree};

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Descend into other mounted filesystems.
    pub cross_fs: bool,
    /// Globs matched against the `/`-separated path relative to the root; matches are skipped.
    pub exclude: Option<GlobSet>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanIssues {
    pub count: u64,
    /// First few `(path, error)` pairs.
    pub samples: Vec<(String, String)>,
}

pub struct ScanOutput {
    pub tree: Tree,
    pub issues: ScanIssues,
    pub root_path: PathBuf,
}

struct RawNode {
    name: String,
    kind: Kind,
    bytes: u64,
    disk: u64,
    mtime: i64,
    flags: Flags,
    /// (device, inode) for files with more than one hard link.
    link_key: Option<(u64, u64)>,
    children: Vec<RawNode>,
}

struct Ctx<'a> {
    opts: &'a ScanOptions,
    root_dev: Option<u64>,
    root: &'a Path,
    issues: Mutex<ScanIssues>,
    seen: &'a AtomicU64,
    /// Mount points below the root (Linux). Never stat'ed unless `cross_fs`: a dead NFS
    /// or autofs mount can hang `lstat`.
    mounts: HashSet<PathBuf>,
    /// `(dev, ino)` of directories already descended — only tracked with `cross_fs`,
    /// where bind mounts can form loops.
    visited: Mutex<HashSet<(u64, u64)>>,
}

const MAX_ISSUE_SAMPLES: usize = 50;

impl Ctx<'_> {
    fn issue(&self, path: &Path, err: &std::io::Error) {
        let mut issues = self.issues.lock().unwrap_or_else(|e| e.into_inner());
        issues.count += 1;
        if issues.samples.len() < MAX_ISSUE_SAMPLES {
            let rel = path.strip_prefix(self.root).unwrap_or(path);
            issues
                .samples
                .push((rel.to_string_lossy().into_owned(), err.to_string()));
        }
    }

    fn excluded(&self, path: &Path) -> bool {
        let Some(set) = &self.opts.exclude else {
            return false;
        };
        let rel = path.strip_prefix(self.root).unwrap_or(path);
        set.is_match(rel)
    }
}

/// Walk `root` and build the arena. `seen` is bumped once per entry (for progress display).
pub fn scan(root: &Path, opts: &ScanOptions, seen: &AtomicU64) -> Result<ScanOutput> {
    let root_path =
        dunce::canonicalize(root).with_context(|| format!("cannot access {}", root.display()))?;
    let meta =
        fs::metadata(&root_path).with_context(|| format!("cannot stat {}", root_path.display()))?;

    let ctx = Ctx {
        opts,
        root_dev: device(&meta),
        root: &root_path,
        issues: Mutex::new(ScanIssues::default()),
        seen,
        mounts: mount_points_below(&root_path),
        visited: Mutex::new(HashSet::new()),
    };

    let root_name = root_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_path.to_string_lossy().into_owned());

    let raw = if meta.is_dir() {
        let mut node = raw_from_meta(root_name, &meta, Kind::Dir);
        node.children = scan_dir(&root_path, &ctx, &mut node.flags);
        node
    } else {
        let mut node = raw_from_meta(root_name, &meta, Kind::File);
        node.link_key = None;
        node
    };

    let (mut tree, link_keys) = flatten(raw);
    dedupe_hard_links(&mut tree, link_keys);
    tree.roll_up();

    Ok(ScanOutput {
        tree,
        issues: ctx.issues.into_inner().unwrap_or_else(|e| e.into_inner()),
        root_path,
    })
}

fn scan_dir(dir: &Path, ctx: &Ctx<'_>, dir_flags: &mut Flags) -> Vec<RawNode> {
    let entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| match e {
                Ok(e) => Some(e),
                Err(err) => {
                    ctx.issue(dir, &err);
                    None
                }
            })
            .collect(),
        Err(err) => {
            ctx.issue(dir, &err);
            dir_flags.set(Flags::UNREADABLE);
            return Vec::new();
        }
    };

    let mut children: Vec<RawNode> = entries
        .into_par_iter()
        .filter_map(|entry| scan_entry(&entry, ctx))
        .collect();
    children.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    children
}

fn scan_entry(entry: &fs::DirEntry, ctx: &Ctx<'_>) -> Option<RawNode> {
    ctx.seen.fetch_add(1, Ordering::Relaxed);
    let path = entry.path();
    if ctx.excluded(&path) {
        return None;
    }
    let os_name = entry.file_name();
    let name = os_name.to_string_lossy().into_owned();
    let non_utf8 = os_name.to_str().is_none();
    if !ctx.opts.cross_fs && ctx.mounts.contains(&path) {
        let mut n = RawNode::empty(name, Kind::Dir);
        n.flags.set(Flags::MOUNT);
        return Some(n);
    }
    let mut node = scan_meta(entry, &path, name, ctx)?;
    if non_utf8 {
        node.flags.set(Flags::NON_UTF8);
    }
    Some(node)
}

fn scan_meta(entry: &fs::DirEntry, path: &Path, name: String, ctx: &Ctx<'_>) -> Option<RawNode> {
    let path = path.to_path_buf();
    let meta = match entry.metadata() {
        Ok(m) => m,
        Err(err) => {
            ctx.issue(&path, &err);
            let mut n = RawNode::empty(name, Kind::Special);
            n.flags.set(Flags::UNREADABLE);
            return Some(n);
        }
    };
    let ft = meta.file_type();
    if ft.is_dir() {
        let mut node = raw_from_meta(name, &meta, Kind::Dir);
        if !ctx.opts.cross_fs && device(&meta) != ctx.root_dev {
            node.flags.set(Flags::MOUNT);
            return Some(node);
        }
        if ctx.opts.cross_fs
            && let Some(key) = dir_key(&meta)
            && !ctx
                .visited
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key)
        {
            node.flags.set(Flags::LOOP);
            return Some(node);
        }
        node.children = scan_dir(&path, ctx, &mut node.flags);
        Some(node)
    } else if ft.is_file() {
        let mut node = raw_from_meta(name, &meta, Kind::File);
        if is_placeholder(&meta) {
            node.flags.set(Flags::PLACEHOLDER);
        }
        Some(node)
    } else if ft.is_symlink() {
        Some(raw_from_meta(name, &meta, Kind::Symlink))
    } else {
        Some(raw_from_meta(name, &meta, Kind::Special))
    }
}

impl RawNode {
    fn empty(name: String, kind: Kind) -> Self {
        RawNode {
            name,
            kind,
            bytes: 0,
            disk: 0,
            mtime: 0,
            flags: Flags::default(),
            link_key: None,
            children: Vec::new(),
        }
    }
}

fn raw_from_meta(name: String, meta: &fs::Metadata, kind: Kind) -> RawNode {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let bytes = if kind == Kind::Special { 0 } else { meta.len() };
    let mut n = RawNode::empty(name, kind);
    n.bytes = bytes;
    n.disk = disk_usage(meta, bytes);
    n.mtime = mtime;
    if kind == Kind::File {
        n.link_key = hard_link_key(meta);
    }
    n
}

#[cfg(unix)]
fn device(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.dev())
}

#[cfg(not(unix))]
fn device(_meta: &fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn dir_key(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn dir_key(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Cloud-storage placeholders download their contents when read; never read them.
#[cfg(target_os = "macos")]
fn is_placeholder(meta: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(windows)]
fn is_placeholder(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(not(any(target_os = "macos", windows)))]
fn is_placeholder(_meta: &fs::Metadata) -> bool {
    false
}

/// Mount points strictly below `root`, from `/proc/self/mountinfo` (Linux only).
fn mount_points_below(root: &Path) -> HashSet<PathBuf> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|p| PathBuf::from(unescape_mountinfo(p)))
        .filter(|p| p != root && p.starts_with(root))
        .collect()
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo` octal.
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
fn disk_usage(meta: &fs::Metadata, _bytes: u64) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn disk_usage(_meta: &fs::Metadata, bytes: u64) -> u64 {
    bytes
}

#[cfg(unix)]
fn hard_link_key(meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn hard_link_key(_meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Convert the recursive scan result into a DFS pre-order arena, iteratively.
/// Also returns each node's hard-link key, indexed by node id.
fn flatten(root: RawNode) -> (Tree, Vec<Option<(u64, u64)>>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut keys: Vec<Option<(u64, u64)>> = Vec::new();
    // (raw node, parent id, depth)
    let mut stack: Vec<(RawNode, u32, u16)> = vec![(root, NO_PARENT, 0)];
    while let Some((mut raw, parent, depth)) = stack.pop() {
        let id = nodes.len() as u32;
        let mut node = Node::new(std::mem::take(&mut raw.name), raw.kind);
        node.parent = parent;
        node.depth = depth;
        node.bytes = raw.bytes;
        node.disk = raw.disk;
        node.mtime = raw.mtime;
        node.flags = raw.flags;
        node.files = u32::from(raw.kind == Kind::File);
        nodes.push(node);
        keys.push(raw.link_key);
        // Push in reverse so the first child is processed next (pre-order, sorted).
        for child in raw.children.into_iter().rev() {
            stack.push((child, id, depth.saturating_add(1)));
        }
    }
    // Spans: reverse pass, each node adds its span to its parent.
    for i in (1..nodes.len()).rev() {
        let span = nodes[i].span;
        let parent = nodes[i].parent as usize;
        nodes[parent].span += span;
    }
    (Tree { nodes }, keys)
}

/// Attribute each hard-linked inode's disk usage to its first path (pre-order = sorted).
fn dedupe_hard_links(tree: &mut Tree, keys: Vec<Option<(u64, u64)>>) {
    let mut seen = std::collections::HashSet::new();
    for (i, key) in keys.into_iter().enumerate() {
        if let Some(key) = key
            && !seen.insert(key)
        {
            let n = &mut tree.nodes[i];
            n.disk = 0;
            n.flags.set(Flags::HARDLINK_DUP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_sorted_preorder_with_rollups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("b/inner")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/x.txt"), "hello").unwrap();
        fs::write(root.join("b/inner/y.txt"), "worldwide").unwrap();
        fs::write(root.join("top.txt"), "").unwrap();

        let seen = AtomicU64::new(0);
        let out = scan(root, &ScanOptions::default(), &seen).unwrap();
        let t = &out.tree;
        let names: Vec<&str> = t.nodes.iter().map(|n| &*n.name).collect();
        assert_eq!(
            &names[1..],
            &["a", "x.txt", "b", "inner", "y.txt", "top.txt"]
        );
        assert_eq!(t.root().files, 3);
        assert_eq!(t.root().bytes, 5 + 9);
        assert_eq!(t.root().span as usize, t.len());
        let b = t.find("b").unwrap();
        assert_eq!(t.get(b).bytes, 9);
        assert_eq!(
            t.rel_path(t.find("b/inner/y.txt").unwrap()),
            "b/inner/y.txt"
        );
        assert_eq!(t.children(0).count(), 3);
        assert_eq!(seen.load(Ordering::Relaxed), 6);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_count_disk_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.bin"), vec![7u8; 64 * 1024]).unwrap();
        fs::hard_link(root.join("a.bin"), root.join("b.bin")).unwrap();
        let seen = AtomicU64::new(0);
        let out = scan(root, &ScanOptions::default(), &seen).unwrap();
        let t = &out.tree;
        let a = t.get(t.find("a.bin").unwrap());
        let b = t.get(t.find("b.bin").unwrap());
        assert!(a.disk > 0);
        assert_eq!(b.disk, 0);
        assert!(b.flags.has(Flags::HARDLINK_DUP));
        assert_eq!(t.root().disk, a.disk);
        assert_eq!(t.root().bytes, 2 * 64 * 1024);
    }

    #[test]
    fn mountinfo_unescape() {
        assert_eq!(unescape_mountinfo("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
    }

    #[test]
    fn excludes_by_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("skip/deep")).unwrap();
        fs::write(root.join("skip/deep/f"), "x").unwrap();
        fs::write(root.join("keep"), "x").unwrap();
        let mut b = globset::GlobSetBuilder::new();
        b.add(globset::Glob::new("skip").unwrap());
        let opts = ScanOptions {
            exclude: Some(b.build().unwrap()),
            ..Default::default()
        };
        let seen = AtomicU64::new(0);
        let out = scan(root, &opts, &seen).unwrap();
        assert!(out.tree.find("skip").is_none());
        assert!(out.tree.find("keep").is_some());
    }
}
