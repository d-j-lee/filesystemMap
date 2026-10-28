//! Node budget for the HTML report: keep the most important nodes by *any* metric
//! (disk, bytes, code, commits) plus everything a finding or table refers to, and fold
//! the rest of each directory into one `Rest` node so every total still adds up.

use std::collections::HashSet;

use crate::model::{Kind, NO_PARENT, Node, NodeId, Tree};

pub struct Pruned {
    pub tree: Tree,
    /// Old id → new id of the node itself or its nearest kept ancestor.
    pub map: Vec<NodeId>,
}

fn share(v: f64, total: f64) -> f64 {
    if total > 0.0 { v / total } else { 0.0 }
}

/// Returns `None` when the tree already fits the budget.
pub fn prune(tree: &Tree, budget: usize, pinned: &HashSet<NodeId>) -> Option<Pruned> {
    if tree.len() <= budget.max(1) {
        return None;
    }
    let root = tree.root();
    let (td, tb, tc, tm) = (
        root.disk as f64,
        root.bytes as f64,
        f64::from(root.code),
        f64::from(root.commits),
    );
    let importance = |n: &Node| -> f64 {
        share(n.disk as f64, td)
            .max(share(n.bytes as f64, tb))
            .max(share(f64::from(n.code), tc))
            .max(share(f64::from(n.commits), tm))
    };

    let mut keep = vec![false; tree.len()];
    let mut kept = 0usize;
    let mark = |id: NodeId, keep: &mut Vec<bool>, kept: &mut usize| -> usize {
        // Returns how many nodes were newly kept (the node and missing ancestors).
        let mut added = 0;
        let mut cur = id;
        while cur != NO_PARENT && !keep[cur as usize] {
            keep[cur as usize] = true;
            added += 1;
            cur = tree.get(cur).parent;
        }
        *kept += added;
        added
    };
    mark(0, &mut keep, &mut kept);
    let mut pins: Vec<NodeId> = pinned
        .iter()
        .copied()
        .filter(|&p| (p as usize) < tree.len())
        .collect();
    pins.sort_unstable();
    for p in pins {
        mark(p, &mut keep, &mut kept);
    }

    // Leave ~10 % of the budget for the per-directory Rest nodes.
    let limit = budget * 9 / 10;
    let mut order: Vec<(NodeId, f64)> = (1..tree.len() as NodeId)
        .map(|id| (id, importance(tree.get(id))))
        .collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    for (id, _) in order {
        if kept >= limit {
            break;
        }
        mark(id, &mut keep, &mut kept);
    }

    // Kept children per kept node, and the folded remainder per kept directory.
    let n = tree.len();
    let mut kids: Vec<Vec<NodeId>> = vec![Vec::new(); n];
    let mut rest: Vec<Option<Node>> = vec![None; n];
    for id in 1..n as NodeId {
        let p = tree.get(id).parent;
        if !keep[p as usize] {
            continue;
        }
        if keep[id as usize] {
            kids[p as usize].push(id);
        } else {
            let src = tree.get(id);
            let r = rest[p as usize].get_or_insert_with(|| {
                let mut r = Node::new("", Kind::Rest);
                r.zone = tree.get(p).zone;
                r
            });
            r.span += 1; // count of folded entries (reset below)
            r.bytes += src.bytes;
            r.disk += src.disk;
            r.files = r.files.saturating_add(src.files);
            r.mtime = r.mtime.max(src.mtime);
            r.lines = r.lines.saturating_add(src.lines);
            r.code = r.code.saturating_add(src.code);
            r.comments = r.comments.saturating_add(src.comments);
            r.blank = r.blank.saturating_add(src.blank);
            r.complexity = r.complexity.saturating_add(src.complexity);
            r.commits = r.commits.saturating_add(src.commits);
            r.recent += src.recent;
            r.last_commit = r.last_commit.max(src.last_commit);
        }
    }

    // Emit in pre-order with each Rest node as its directory's last child.
    let mut nodes: Vec<Node> = Vec::with_capacity(kept + kept / 8);
    let mut new_id = vec![NO_PARENT; n];
    enum Item {
        Old(NodeId, NodeId),
        Rest(NodeId, NodeId),
    }
    let mut stack = vec![Item::Old(0, NO_PARENT)];
    while let Some(item) = stack.pop() {
        match item {
            Item::Old(old, parent) => {
                let id = nodes.len() as NodeId;
                new_id[old as usize] = id;
                let mut node = tree.get(old).clone();
                node.parent = parent;
                node.span = 1;
                nodes.push(node);
                if rest[old as usize].is_some() {
                    stack.push(Item::Rest(old, id));
                }
                for &c in kids[old as usize].iter().rev() {
                    stack.push(Item::Old(c, id));
                }
            }
            Item::Rest(old, parent) => {
                let mut r = rest[old as usize].take().expect("rest exists");
                r.name = format!("… {} smaller items", r.span).into();
                r.span = 1;
                r.parent = parent;
                nodes.push(r);
            }
        }
    }
    for i in (1..nodes.len()).rev() {
        let span = nodes[i].span;
        let p = nodes[i].parent as usize;
        nodes[p].span += span;
    }
    for i in 1..nodes.len() {
        let p = nodes[i].parent as usize;
        nodes[i].depth = nodes[p].depth + 1;
    }

    let mut map = vec![0; n];
    for id in 0..n {
        map[id] = if keep[id] {
            new_id[id]
        } else {
            map[tree.get(id as NodeId).parent as usize]
        };
    }
    Some(Pruned {
        tree: Tree { nodes },
        map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with(sizes: &[u64]) -> Tree {
        let mut nodes = vec![Node::new("root", Kind::Dir)];
        for (i, &s) in sizes.iter().enumerate() {
            let mut n = Node::new(format!("f{i}"), Kind::File);
            n.parent = 0;
            n.depth = 1;
            n.bytes = s;
            n.disk = s;
            n.files = 1;
            nodes.push(n);
        }
        nodes[0].span = nodes.len() as u32;
        let mut t = Tree { nodes };
        t.roll_up();
        t
    }

    #[test]
    fn keeps_big_and_pinned_and_conserves_totals() {
        let t = tree_with(&[1000, 1, 2, 3, 500, 4, 5]);
        let pinned: HashSet<NodeId> = [2].into_iter().collect();
        let p = prune(&t, 5, &pinned).unwrap();
        let names: Vec<&str> = p.tree.nodes.iter().map(|n| &*n.name).collect();
        assert!(names.contains(&"f0"));
        assert!(names.contains(&"f1"), "pinned node kept");
        let total: u64 = p.tree.children(0).map(|c| p.tree.get(c).bytes).sum();
        assert_eq!(total, t.root().bytes);
        assert_eq!(p.tree.root().span as usize, p.tree.len());
        let rest = p.tree.nodes.iter().find(|n| n.kind == Kind::Rest).unwrap();
        assert!(rest.name.contains("smaller items"));
        // A dropped node maps to its kept parent.
        let dropped = (1..t.len())
            .find(|&i| !names.contains(&&*t.nodes[i].name))
            .unwrap();
        assert_eq!(p.map[dropped], 0);
    }

    #[test]
    fn small_trees_untouched() {
        let t = tree_with(&[1, 2]);
        assert!(prune(&t, 10, &HashSet::new()).is_none());
    }
}
