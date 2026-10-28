//! Duplicate files: size buckets → 16 KiB prefix hash → full hash (xxh3-128).
//! Only buckets with at least two members are ever read.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use rayon::prelude::*;
use serde::Serialize;
use xxhash_rust::xxh3::{Xxh3, xxh3_128};

use crate::model::{Flags, NodeId, Tree};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub bytes: u64,
    pub nodes: Vec<NodeId>,
    /// Bytes that would be freed by keeping one copy (an upper bound with reflinks/clones).
    pub wasted: u64,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub min_size: u64,
    /// Include dependency/artifact zones (off: package managers duplicate by design).
    pub all_zones: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            min_size: 1024,
            all_zones: false,
        }
    }
}

const PREFIX: usize = 16 * 1024;

fn prefix_hash(path: &Path) -> Option<u128> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; PREFIX];
    let mut n = 0;
    while n < PREFIX {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return None,
        }
    }
    Some(xxh3_128(&buf[..n]))
}

fn full_hash(path: &Path) -> Option<u128> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => h.update(&buf[..k]),
            Err(_) => return None,
        }
    }
    Some(h.digest128())
}

pub fn find(tree: &Tree, root: &Path, opts: &Options) -> Vec<Group> {
    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    for id in 0..tree.len() as NodeId {
        let n = tree.get(id);
        if !n.is_file()
            || n.bytes < opts.min_size
            || n.flags.has(Flags::HARDLINK_DUP)
            || n.flags.has(Flags::PLACEHOLDER)
            || !(opts.all_zones || n.is_analysed())
        {
            continue;
        }
        by_size.entry(n.bytes).or_default().push(id);
    }
    let buckets: Vec<(u64, Vec<NodeId>)> =
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();

    let mut groups: Vec<Group> = buckets
        .into_par_iter()
        .flat_map_iter(|(size, ids)| {
            let path = |id: NodeId| root.join(tree.rel_path(id));
            let mut by_prefix: HashMap<u128, Vec<NodeId>> = HashMap::new();
            for id in ids {
                if let Some(h) = prefix_hash(&path(id)) {
                    by_prefix.entry(h).or_default().push(id);
                }
            }
            let mut out = Vec::new();
            for (_, cands) in by_prefix.into_iter().filter(|(_, v)| v.len() > 1) {
                let by_full: HashMap<u128, Vec<NodeId>> = if size as usize <= PREFIX {
                    // The prefix hash already covered the whole file.
                    HashMap::from([(0, cands)])
                } else {
                    let mut m: HashMap<u128, Vec<NodeId>> = HashMap::new();
                    for id in cands {
                        if let Some(h) = full_hash(&path(id)) {
                            m.entry(h).or_default().push(id);
                        }
                    }
                    m
                };
                for (_, mut nodes) in by_full.into_iter().filter(|(_, v)| v.len() > 1) {
                    nodes.sort_unstable();
                    out.push(Group {
                        bytes: size,
                        wasted: size * (nodes.len() as u64 - 1),
                        nodes,
                    });
                }
            }
            out
        })
        .collect();
    groups.sort_by(|a, b| b.wasted.cmp(&a.wasted).then(a.nodes.cmp(&b.nodes)));
    groups
}
