//! `fsmap diff old.json new.json`: what grew, shrank, appeared or disappeared between two
//! snapshots, and which findings are new or resolved. Works on unpruned `scan` output.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::util::{bytes, count, date, duration};

#[derive(Deserialize)]
struct NodeIn {
    name: String,
    parent: Option<u32>,
    kind: String,
    #[serde(default)]
    disk: u64,
    #[serde(default)]
    code: u32,
    #[serde(default)]
    files: u32,
}

#[derive(Deserialize)]
struct FindingIn {
    id: String,
    severity: String,
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapIn {
    schema: String,
    generated_at: i64,
    nodes: Vec<NodeIn>,
    #[serde(default)]
    findings: Vec<FindingIn>,
    #[serde(default)]
    pruned: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Default)]
struct Vals {
    disk: u64,
    code: u64,
    files: u64,
    dir: bool,
    depth: usize,
}

fn load(path: &str) -> Result<SnapIn> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let s: SnapIn =
        serde_json::from_str(&text).with_context(|| format!("{path} is not an fsmap snapshot"))?;
    if !s.schema.starts_with("fsmap/") {
        bail!("{path}: unsupported schema {}", s.schema);
    }
    Ok(s)
}

fn index(s: &SnapIn) -> HashMap<String, Vals> {
    let mut paths: Vec<String> = Vec::with_capacity(s.nodes.len());
    let mut out = HashMap::with_capacity(s.nodes.len());
    for n in &s.nodes {
        let (path, depth) = match n.parent {
            None => (String::new(), 0),
            Some(p) => {
                let pp = &paths[p as usize];
                let path = if pp.is_empty() {
                    n.name.clone()
                } else {
                    format!("{pp}/{}", n.name)
                };
                let depth = path.matches('/').count() + 1;
                (path, depth)
            }
        };
        paths.push(path.clone());
        if n.kind == "rest" {
            continue;
        }
        out.insert(
            path,
            Vals {
                disk: n.disk,
                code: u64::from(n.code),
                files: if n.kind == "file" {
                    1
                } else {
                    u64::from(n.files)
                },
                dir: n.kind == "dir",
                depth,
            },
        );
    }
    out
}

fn signed(v: i128, f: impl Fn(u64) -> String) -> String {
    if v >= 0 {
        format!("+{}", f(v as u64))
    } else {
        format!("-{}", f((-v) as u64))
    }
}

pub fn render(old_path: &str, new_path: &str, top: usize) -> Result<String> {
    let old = load(old_path)?;
    let new = load(new_path)?;
    let mut out = String::new();
    if old.pruned.is_some() || new.pruned.is_some() {
        let _ = writeln!(
            out,
            "note: a snapshot was pruned; small entries may appear or vanish. Diff unpruned `fsmap scan` output for exact results.\n"
        );
    }
    let (a, b) = (index(&old), index(&new));
    let root = |m: &HashMap<String, Vals>| m.get("").copied().unwrap_or_default();
    let (ra, rb) = (root(&a), root(&b));
    let span = match duration(new.generated_at - old.generated_at) {
        d if d == "today" => "under a day apart".to_string(),
        d => format!("{d} apart"),
    };
    let _ = writeln!(
        out,
        "fsmap diff · {} → {} ({span})\n  disk  {} → {} ({})\n  files {} → {} ({})\n  code  {} → {} ({})\n",
        date(old.generated_at),
        date(new.generated_at),
        bytes(ra.disk),
        bytes(rb.disk),
        signed(i128::from(rb.disk) - i128::from(ra.disk), bytes),
        count(ra.files),
        count(rb.files),
        signed(i128::from(rb.files) - i128::from(ra.files), count),
        count(ra.code),
        count(rb.code),
        signed(i128::from(rb.code) - i128::from(ra.code), count),
    );

    // Directory growth at depth ≤ 3 (deepest changes are summarised by their parents).
    let mut deltas: Vec<(String, i128, Vals, Vals)> = Vec::new();
    let keys: HashSet<&String> = a.keys().chain(b.keys()).collect();
    for k in keys {
        let (va, vb) = (
            a.get(k).copied().unwrap_or_default(),
            b.get(k).copied().unwrap_or_default(),
        );
        let dir = va.dir || vb.dir;
        let depth = va.depth.max(vb.depth);
        if k.is_empty() || !dir || depth > 3 {
            continue;
        }
        let d = i128::from(vb.disk) - i128::from(va.disk);
        if d != 0 {
            deltas.push((k.clone(), d, va, vb));
        }
    }
    deltas.sort_by(|x, y| y.1.abs().cmp(&x.1.abs()).then(x.0.cmp(&y.0)));
    if !deltas.is_empty() {
        let _ = writeln!(out, "Largest directory changes (disk):");
        for (k, d, va, vb) in deltas.iter().take(top) {
            let tag = if va.depth == 0 && va.disk == 0 {
                " (new)"
            } else if vb.disk == 0 && vb.depth == 0 {
                " (gone)"
            } else {
                ""
            };
            let _ = writeln!(out, "  {:>10}  {k}/{tag}", signed(*d, bytes));
        }
        out.push('\n');
    }

    // New and removed large files.
    let mut appeared: Vec<(&String, u64)> = b
        .iter()
        .filter(|(k, v)| !v.dir && !a.contains_key(*k) && v.disk >= 1 << 20)
        .map(|(k, v)| (k, v.disk))
        .collect();
    appeared.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(y.0)));
    if !appeared.is_empty() {
        let _ = writeln!(out, "New files ≥ 1 MiB:");
        for (k, d) in appeared.iter().take(top) {
            let _ = writeln!(out, "  {:>10}  {k}", bytes(*d));
        }
        out.push('\n');
    }

    let ids_a: BTreeMap<&str, &FindingIn> =
        old.findings.iter().map(|f| (f.id.as_str(), f)).collect();
    let ids_b: BTreeMap<&str, &FindingIn> =
        new.findings.iter().map(|f| (f.id.as_str(), f)).collect();
    let fresh: Vec<&&FindingIn> = ids_b
        .iter()
        .filter(|(k, _)| !ids_a.contains_key(*k))
        .map(|(_, f)| f)
        .collect();
    let gone: Vec<&&FindingIn> = ids_a
        .iter()
        .filter(|(k, _)| !ids_b.contains_key(*k))
        .map(|(_, f)| f)
        .collect();
    if !fresh.is_empty() || !gone.is_empty() {
        let _ = writeln!(
            out,
            "Findings: {} new, {} resolved",
            fresh.len(),
            gone.len()
        );
        for f in fresh.iter().take(top) {
            let _ = writeln!(out, "  + [{}] {}", f.severity, f.title);
        }
        for f in gone.iter().take(top) {
            let _ = writeln!(out, "  - [{}] {}", f.severity, f.title);
        }
    }
    Ok(out)
}
