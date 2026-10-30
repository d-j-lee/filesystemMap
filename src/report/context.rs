//! `fsmap context`: a token-budgeted Markdown map of a repository for AI agents.
//!
//! Deterministic by design (no timestamps, stable ordering) so it can live in a prompt
//! prefix or CLAUDE.md and benefit from prompt caching. Never includes file contents or
//! commit messages (both can carry prompt injection).

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;

use crate::analysis::Analysis;
use crate::classify;
use crate::insights::{Finding, Severity};
use crate::model::{Category, Flags, Kind, NodeId, Tree, Zone};
use crate::snapshot::totals;
use crate::util::{bytes, count, date, md};

pub struct Options {
    /// Approximate token budget for the whole document.
    pub budget: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { budget: 6000 }
    }
}

pub fn tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

fn dominant_lang(t: &Tree, dir: NodeId) -> Option<&'static str> {
    let mut by: std::collections::HashMap<u16, u64> = std::collections::HashMap::new();
    for id in t.subtree(dir) {
        let n = t.get(id);
        if let (true, Some(l)) = (n.is_file() && n.category == Category::Code, n.lang) {
            *by.entry(l).or_insert(0) += u64::from(n.code);
        }
    }
    by.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(l, _)| classify::lang(l).name)
}

struct Layout<'a> {
    t: &'a Tree,
    hot: &'a HashSet<NodeId>,
}

impl Layout<'_> {
    fn dir_line(&self, id: NodeId, depth: usize) -> String {
        let n = self.t.get(id);
        let mut s = format!("{}{}/", "  ".repeat(depth), md(&n.name));
        if n.zone != Zone::Source {
            let _ = write!(s, " [{} · {}]", n.zone.as_str(), bytes(n.disk));
            return s;
        }
        let mut parts = Vec::new();
        if n.code > 0 {
            parts.push(format!("{} code lines", count(u64::from(n.code))));
        }
        parts.push(format!("{} files", count(u64::from(n.files))));
        if let Some(l) = dominant_lang(self.t, id) {
            parts.push(l.to_string());
        }
        let _ = write!(s, " — {}", parts.join(", "));
        if self.t.subtree(id).any(|x| self.hot.contains(&x)) {
            s.push_str(" [hotspots]");
        }
        s
    }

    fn file_line(&self, id: NodeId, depth: usize) -> String {
        let n = self.t.get(id);
        let mut s = format!("{}{}", "  ".repeat(depth), md(&n.name));
        let mut tags = Vec::new();
        if n.code > 0 {
            tags.push(format!("{} lines", count(u64::from(n.code))));
        }
        if n.flags.has(Flags::MANIFEST) {
            tags.push("entry/manifest".into());
        }
        if self.hot.contains(&id) {
            tags.push("hotspot".into());
        }
        if n.flags.has(Flags::GENERATED) {
            tags.push("generated — do not edit".into());
        }
        if n.fan_in >= 5 {
            tags.push(format!("imported by {}", n.fan_in));
        }
        if !tags.is_empty() {
            let _ = write!(s, " — {}", tags.join(", "));
        }
        s
    }

    /// Files worth naming inside an expanded directory.
    fn notable_files(&self, dir: NodeId) -> (Vec<NodeId>, usize) {
        let all: Vec<NodeId> = self
            .t
            .children(dir)
            .filter(|&c| self.t.get(c).kind != Kind::Dir)
            .collect();
        let files: Vec<NodeId> = all
            .iter()
            .copied()
            .filter(|&c| {
                let n = self.t.get(c);
                n.code > 0 || n.flags.has(Flags::MANIFEST) || n.flags.has(Flags::GENERATED)
            })
            .collect();
        let hidden = all.len() - files.len();
        if files.len() <= 8 {
            return (files, hidden);
        }
        let mut ranked = files.clone();
        ranked.sort_by_key(|&c| {
            let n = self.t.get(c);
            (
                std::cmp::Reverse(n.flags.has(Flags::MANIFEST) || self.hot.contains(&c)),
                std::cmp::Reverse(n.code),
                c,
            )
        });
        let mut keep: Vec<NodeId> = ranked.into_iter().take(6).collect();
        keep.sort_unstable();
        let more = all.len() - keep.len();
        (keep, more)
    }
}

/// Render the layout tree within `budget` tokens, expanding the biggest code areas first.
fn layout(t: &Tree, hot: &HashSet<NodeId>, budget: usize) -> String {
    let l = Layout { t, hot };
    let mut expanded: BTreeSet<NodeId> = BTreeSet::new();
    let mut used = 0usize;
    let mut frontier: Vec<NodeId> = vec![0];
    let cost_of = |dir: NodeId, expanded_children: &dyn Fn(NodeId) -> String| -> usize {
        tokens(&expanded_children(dir))
    };
    let render_children = |dir: NodeId| -> String {
        let depth = (t.get(dir).depth as usize).saturating_sub(t.root().depth as usize) + 1;
        let mut s = String::new();
        for c in t.children(dir).filter(|&c| t.get(c).is_dir()) {
            s.push_str(&l.dir_line(c, depth));
            s.push('\n');
        }
        let (files, more) = l.notable_files(dir);
        for f in files {
            s.push_str(&l.file_line(f, depth));
            s.push('\n');
        }
        if more > 0 {
            let _ = writeln!(s, "{}… {more} more files", "  ".repeat(depth));
        }
        s
    };
    while let Some(pos) = frontier
        .iter()
        .enumerate()
        .max_by(|a, b| {
            let (na, nb) = (t.get(*a.1), t.get(*b.1));
            na.code
                .cmp(&nb.code)
                .then(nb.files.cmp(&na.files))
                .then(b.1.cmp(a.1))
        })
        .map(|(i, _)| i)
    {
        let dir = frontier.swap_remove(pos);
        let cost = cost_of(dir, &render_children);
        if used + cost > budget && !expanded.is_empty() {
            continue;
        }
        used += cost;
        expanded.insert(dir);
        frontier.extend(
            t.children(dir).filter(|&c| {
                t.get(c).is_dir() && t.get(c).zone.is_analysed() && t.get(c).files > 0
            }),
        );
    }
    // Emit in pre-order.
    let mut out = String::new();
    let _ = writeln!(out, "{}", l.dir_line(0, 0).trim_start());
    fn emit(t: &Tree, l: &Layout<'_>, dir: NodeId, expanded: &BTreeSet<NodeId>, out: &mut String) {
        let depth = (t.get(dir).depth as usize).saturating_sub(t.root().depth as usize) + 1;
        for c in t.children(dir).filter(|&c| t.get(c).is_dir()) {
            out.push_str(&l.dir_line(c, depth));
            out.push('\n');
            if expanded.contains(&c) {
                emit(t, l, c, expanded, out);
            }
        }
        let (files, more) = l.notable_files(dir);
        for f in files {
            out.push_str(&l.file_line(f, depth));
            out.push('\n');
        }
        if more > 0 {
            let _ = writeln!(out, "{}… {more} more files", "  ".repeat(depth));
        }
    }
    emit(t, &l, 0, &expanded, &mut out);
    out
}

pub fn render(a: &Analysis, findings: &[Finding], opts: &Options) -> String {
    let t = &a.tree;
    let tot = totals(t);
    let hot: HashSet<NodeId> = a
        .hotspots
        .iter()
        .filter(|h| h.flagged(t))
        .map(|h| h.node)
        .collect();
    let mut head = String::new();
    let name = a
        .root
        .file_name()
        .map_or_else(|| a.root.to_string_lossy(), |n| n.to_string_lossy());
    let _ = writeln!(head, "# Repository map: {name}\n");
    let heads: Vec<String> = a
        .repos
        .iter()
        .filter_map(|r| {
            r.head
                .as_ref()
                .map(|h| format!("{}@{h}", if r.path == "." { "" } else { &r.path }))
        })
        .take(3)
        .collect();
    let _ = writeln!(
        head,
        "_Generated by `fsmap context` ({}). Structure and history only — no file contents. Regenerate after large changes._\n",
        if heads.is_empty() {
            "no git".to_string()
        } else {
            heads.join(", ")
        }
    );

    // At a glance.
    let mut langs: Vec<(&str, u64)> = tot
        .by_language
        .iter()
        .filter(|(n, b)| {
            b.code > 0
                && classify::lang_by_name(n)
                    .is_some_and(|i| classify::lang(i).category == Category::Code)
        })
        .map(|(n, b)| (*n, b.code))
        .collect();
    langs.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(y.0)));
    let code_total: u64 = langs.iter().map(|l| l.1).sum();
    let mix = langs
        .iter()
        .take(5)
        .map(|(n, c)| format!("{n} {:.0}%", *c as f64 * 100.0 / code_total.max(1) as f64))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(head, "## At a glance");
    let _ = writeln!(
        head,
        "- {} files, {} code lines ({mix}); ~{} tokens to read all analysed text.",
        count(tot.files),
        count(tot.code),
        count(tot.tokens)
    );
    for r in &a.repos {
        if let Some(h) = &r.history
            && h.commits > 0
        {
            let _ = writeln!(
                head,
                "- Git{}: {} commits {} → {}, {} authors ({} active){}.",
                if r.path == "." {
                    String::new()
                } else {
                    format!(" `{}`", r.path)
                },
                count(u64::from(h.commits)),
                date(h.first),
                date(h.last),
                h.authors,
                h.active_authors,
                h.truck_factor
                    .map(|f| format!(", truck factor {f}"))
                    .unwrap_or_default()
            );
        }
    }
    let regen: u64 = tot
        .by_zone
        .iter()
        .filter(|(z, _)| matches!(**z, "artifact" | "dependency"))
        .map(|(_, b)| b.disk)
        .sum();
    if regen > 0 {
        let _ = writeln!(
            head,
            "- On disk {}; {} of it is re-creatable build output / dependencies.",
            bytes(tot.disk),
            bytes(regen)
        );
    }
    head.push('\n');

    // Sections that compete for the remaining budget.
    let mut sections: Vec<String> = Vec::new();

    // Grouped by rule, one action each; hotspots get their own section below.
    let mut by_rule: Vec<(&str, Vec<&Finding>)> = Vec::new();
    for f in findings
        .iter()
        .filter(|f| f.severity >= Severity::Medium && f.rule != "hotspot")
    {
        match by_rule.iter_mut().find(|(r, _)| *r == f.rule) {
            Some((_, v)) => v.push(f),
            None => by_rule.push((f.rule, vec![f])),
        }
    }
    if !by_rule.is_empty() {
        let mut s = String::from("## Watch out\n");
        for (_, fs) in by_rule.iter().take(8) {
            let titles: Vec<String> = fs.iter().take(4).map(|f| md(&f.title)).collect();
            let more = if fs.len() > 4 {
                format!(" (+{} more)", fs.len() - 4)
            } else {
                String::new()
            };
            let _ = writeln!(
                s,
                "- **{}** {}{more}. {}",
                fs[0].severity.as_str(),
                titles.join("; "),
                fs[0].action
            );
        }
        sections.push(s);
    }

    let manifests: Vec<NodeId> = (0..t.len() as NodeId)
        .filter(|&id| {
            let n = t.get(id);
            let doc = matches!(&*n.name, "README.md" | "CLAUDE.md" | "AGENTS.md");
            n.flags.has(Flags::MANIFEST)
                && n.is_analysed()
                && !n.flags.has(Flags::TEST)
                && (!doc || n.parent == 0)
                && n.depth <= t.root().depth + 3
        })
        .take(25)
        .collect();
    if !manifests.is_empty() {
        let mut s = String::from("## Entry points & manifests\n");
        let list: Vec<String> = manifests
            .iter()
            .map(|&id| format!("`{}`", md(&t.rel_path(id))))
            .collect();
        let _ = writeln!(s, "{}", list.join(", "));
        sections.push(s);
    }

    if let Some(g) = &a.graph {
        let mut lifted: std::collections::HashMap<(NodeId, NodeId), u32> =
            std::collections::HashMap::new();
        for e in g.edges.iter().filter(|e| !e.up && !e.weak) {
            let l = t.lca(e.from, e.to);
            if l == e.from || l == e.to || t.get(l).depth > t.root().depth + 1 {
                continue;
            }
            let (x, y) = (t.child_toward(l, e.from), t.child_toward(l, e.to));
            if t.get(x).is_dir() && t.get(y).is_dir() {
                *lifted.entry((x, y)).or_insert(0) += e.weight;
            }
        }
        let mut edges: Vec<((NodeId, NodeId), u32)> = lifted.into_iter().collect();
        edges.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if !edges.is_empty() || !g.cycles.is_empty() {
            let mut s =
                String::from("## Module dependencies (imports between top-level directories)\n");
            for ((x, y), w) in edges.iter().take(15) {
                let _ = writeln!(
                    s,
                    "- `{}` → `{}` ({w})",
                    md(&t.rel_path(*x)),
                    md(&t.rel_path(*y))
                );
            }
            for c in g.cycles.iter().take(5) {
                let names: Vec<String> = c.members.iter().map(|&m| md(&t.get(m).name)).collect();
                let _ = writeln!(
                    s,
                    "- Cycle in `{}`: {} (break it by removing {})",
                    md(&t.display_path(c.parent)),
                    names.join(" ↔ "),
                    c.cuts
                        .iter()
                        .take(4)
                        .map(|&(x, y, w)| format!(
                            "{} → {} ({w})",
                            md(&t.get(x).name),
                            md(&t.get(y).name)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            sections.push(s);
        }
    }

    let hot_list: Vec<_> = a
        .hotspots
        .iter()
        .filter(|h| h.flagged(t))
        .take(10)
        .collect();
    if !hot_list.is_empty() {
        let mut s =
            String::from("## Hotspots (frequent change × high complexity — edit with tests)\n");
        for h in hot_list {
            let n = t.get(h.node);
            let _ = writeln!(
                s,
                "- `{}` — {} commits, {} code lines",
                md(&t.rel_path(h.node)),
                n.commits,
                count(u64::from(n.code))
            );
        }
        sections.push(s);
    }

    let is_code = |id: NodeId| t.get(id).category == Category::Code;
    let mut pairs: Vec<_> = a.history.coupling.iter().filter(|p| !p.test_pair).collect();
    // Code ⇄ code first: config/docs pairs are conventions, code pairs are contracts.
    pairs.sort_by_key(|p| std::cmp::Reverse(is_code(p.a) && is_code(p.b)));
    pairs.truncate(10);
    if !pairs.is_empty() {
        let mut s = String::from("## Files that change together (update both)\n");
        for p in pairs {
            let link = match p.linked {
                Some(false) => " — no import link",
                _ => "",
            };
            let _ = writeln!(
                s,
                "- `{}` ⇄ `{}` — {} shared commits ({:.0}% / {:.0}%){link}",
                md(&t.rel_path(p.a)),
                md(&t.rel_path(p.b)),
                p.shared,
                p.conf_ab * 100.0,
                p.conf_ba * 100.0
            );
        }
        sections.push(s);
    }

    let skip = skip_list(a);
    if !skip.is_empty() {
        let mut s = String::from("## Skip when reading (low value, high token cost)\n");
        for (path, why, tok) in skip.iter().take(12) {
            let _ = writeln!(s, "- `{}` — {why}, ~{} tokens", md(path), count(*tok));
        }
        sections.push(s);
    }

    let fixed: usize = tokens(&head) + sections.iter().map(|s| tokens(s) + 1).sum::<usize>();
    let layout_budget = opts.budget.saturating_sub(fixed).max(opts.budget * 3 / 10);
    let mut out = head;
    let _ = writeln!(out, "## Layout\n```");
    out.push_str(&layout(t, &hot, layout_budget));
    let _ = writeln!(out, "```\n");
    for s in sections {
        out.push_str(&s);
        out.push('\n');
    }
    out
}

/// Paths an agent should not read: (path, reason, approx tokens), largest first.
pub fn skip_list(a: &Analysis) -> Vec<(String, String, u64)> {
    let t = &a.tree;
    let mut out: Vec<(String, String, u64)> = Vec::new();
    let mut covered: Vec<NodeId> = Vec::new();
    for z in &a.zone_roots {
        if t.ancestors(z.node).any(|x| covered.contains(&x)) {
            continue;
        }
        let n = t.get(z.node);
        if matches!(n.zone, Zone::Source | Zone::Vcs) || n.bytes < 64 * 1024 {
            continue;
        }
        covered.push(z.node);
        out.push((
            format!("{}/", t.rel_path(z.node)),
            format!("{} ({})", n.zone.as_str(), z.reason),
            n.bytes / 4,
        ));
    }
    // Generated and bulky data files inside the analysed zones, grouped by directory.
    let mut by_dir: std::collections::BTreeMap<NodeId, (u64, &'static str)> =
        std::collections::BTreeMap::new();
    for id in 0..t.len() as NodeId {
        let n = t.get(id);
        if !n.is_file() || !n.is_analysed() || covered.iter().any(|&c| t.is_ancestor_or_self(c, id))
        {
            continue;
        }
        let why = if n.flags.has(Flags::GENERATED) || n.category == Category::Generated {
            "generated"
        } else if n.category == Category::Data && n.bytes >= 256 * 1024 {
            "data"
        } else {
            continue;
        };
        let e = by_dir.entry(n.parent).or_insert((0, why));
        e.0 += n.bytes;
    }
    for (dir, (b, why)) in by_dir {
        if b >= 64 * 1024 {
            let p = t.rel_path(dir);
            out.push((
                if p.is_empty() {
                    "./".into()
                } else {
                    format!("{p}/")
                } + &format!("{why} files"),
                why.to_string(),
                b / 4,
            ));
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    out
}
