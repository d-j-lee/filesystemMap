//! Findings: independent rules over the analysed tree. Each finding carries evidence, an
//! action and (where safe) a copyable command. IDs are stable — `rule:hash(key paths)` —
//! so a baseline can ratchet: CI fails only on findings that are new.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use xxhash_rust::xxh3::xxh3_64;

use crate::analysis::{Analysis, AuthorMode};
use crate::model::{Category, Flags, GitState, Kind, NodeId, Tree, Zone};
use crate::util::{bytes, count, date, duration, shell_quote};

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub rule: &'static str,
    pub severity: Severity,
    pub title: String,
    /// Evidence: what was measured.
    pub detail: String,
    /// What to do about it.
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub nodes: Vec<NodeId>,
    pub paths: Vec<String>,
    /// Size involved, for ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Not present in the baseline (only set when a baseline was given).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
}

/// Rule catalogue: id → one-line description (used for SARIF and docs).
pub const RULES: &[(&str, &str)] = &[
    ("dep-cycle", "Import cycle between sibling modules"),
    (
        "committed-artifact",
        "Build output or dependency directory tracked in git",
    ),
    (
        "sensitive-file",
        "File whose name suggests secrets, tracked or about to be",
    ),
    (
        "large-tracked-file",
        "Large file tracked in git (consider Git LFS)",
    ),
    (
        "untracked-artifact",
        "Build output or dependency directory not git-ignored",
    ),
    (
        "hotspot",
        "File with both high recent change rate and high complexity",
    ),
    (
        "hidden-coupling",
        "Files that change together without any import relation",
    ),
    ("knowledge-risk", "Important file with no active author"),
    ("truck-factor", "Few developers hold most of the authorship"),
    ("vcs-bloat", "Git history much larger than the working tree"),
    (
        "unpushed-work",
        "Repository with work that exists only locally",
    ),
    (
        "large-untracked-file",
        "Large untracked file one `git add` away from history",
    ),
    ("duplicates", "Identical files stored more than once"),
    (
        "rebuildable-space",
        "Disk used by re-creatable build output and dependencies",
    ),
    ("big-source-file", "Very large hand-written source file"),
    (
        "portability",
        "Paths that break on case-insensitive or Windows filesystems",
    ),
    (
        "stale-code",
        "Sizeable code untouched for the whole analysis window",
    ),
    (
        "history-limits",
        "Git history was truncated; history metrics are partial",
    ),
    ("scan-issues", "Entries that could not be read"),
];

struct Ctx<'a> {
    a: &'a Analysis,
    t: &'a Tree,
    out: Vec<Finding>,
    /// No repository anywhere, but the root looks like a project (has a manifest).
    project_without_git: bool,
}

impl Ctx<'_> {
    /// Code-quality rules apply to code someone maintains: inside a git work tree, or a
    /// project directory without git. Not to `/usr/include` or a downloads folder.
    fn in_code_scope(&self, id: NodeId) -> bool {
        self.t.get(id).git.is_some() || self.project_without_git
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        rule: &'static str,
        severity: Severity,
        nodes: Vec<NodeId>,
        title: String,
        detail: String,
        action: impl Into<String>,
        command: Option<String>,
        size: Option<u64>,
    ) {
        let paths: Vec<String> = nodes.iter().map(|&n| self.t.display_path(n)).collect();
        let key = format!("{rule}|{}", paths.join("|"));
        self.out.push(Finding {
            id: format!("{rule}:{:08x}", xxh3_64(key.as_bytes()) as u32),
            rule,
            severity,
            title,
            detail,
            action: action.into(),
            command,
            nodes,
            paths,
            bytes: size,
            new: false,
        });
    }

    /// Path for titles and details; the root shows as its directory name.
    fn p(&self, id: NodeId) -> String {
        if id == 0 {
            format!("{}/", self.t.root().name)
        } else {
            self.t.display_path(id)
        }
    }

    fn author(&self, idx: u32) -> String {
        if self.a.options.authors == AuthorMode::None {
            "a developer".into()
        } else {
            self.a
                .authors
                .names
                .get(idx as usize)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// Path of `id` relative to the work tree root of its repo (for git commands).
    fn repo_rel(&self, id: NodeId) -> (Option<NodeId>, String) {
        let repo = self
            .a
            .repos
            .iter()
            .filter(|r| self.t.is_ancestor_or_self(r.node, id))
            .max_by_key(|r| r.node);
        match repo {
            Some(r) => {
                let full = self.t.rel_path(id);
                let base = self.t.rel_path(r.node);
                let rel = if base.is_empty() {
                    full
                } else {
                    full.strip_prefix(&format!("{base}/"))
                        .unwrap_or(&full)
                        .to_string()
                };
                let rel = match &r.prefix {
                    Some(prefix) => format!("{prefix}/{rel}"),
                    None => rel,
                };
                (Some(r.node), rel)
            }
            None => (None, self.t.rel_path(id)),
        }
    }
}

/// Run every rule. Findings are sorted by severity, then size, then id.
pub fn run(a: &Analysis) -> Vec<Finding> {
    let t = &a.tree;
    let mut c = Ctx {
        a,
        t,
        out: Vec::new(),
        project_without_git: a.repos.is_empty()
            && t.children(0).any(|ch| t.get(ch).flags.has(Flags::MANIFEST)),
    };
    artifacts(&mut c);
    large_files(&mut c);
    sensitive(&mut c);
    hotspots(&mut c);
    cycles(&mut c);
    coupling(&mut c);
    knowledge(&mut c);
    repos(&mut c);
    duplicates(&mut c);
    big_files(&mut c);
    stale(&mut c);
    portability(&mut c);
    scan_issues(&mut c);
    let mut out = c.out;
    out.sort_by(|x, y| {
        y.severity
            .cmp(&x.severity)
            .then(y.bytes.unwrap_or(0).cmp(&x.bytes.unwrap_or(0)))
            .then(x.id.cmp(&y.id))
    });
    out
}

fn artifacts(c: &mut Ctx<'_>) {
    let t = c.t;
    // Outermost artifact/dependency roots only.
    let roots: Vec<(NodeId, &'static str)> =
        c.a.zone_roots
            .iter()
            .filter(|z| matches!(z.zone, Zone::Artifact | Zone::Dependency))
            .filter(|z| {
                !t.ancestors(z.node)
                    .any(|anc| matches!(t.get(anc).zone, Zone::Artifact | Zone::Dependency))
            })
            .map(|z| (z.node, z.reason))
            .collect();

    let mut rebuildable: Vec<(NodeId, u64)> = Vec::new();
    for &(id, reason) in &roots {
        let n = t.get(id);
        rebuildable.push((id, n.disk));
        let tracked: Vec<NodeId> = t
            .subtree(id)
            .filter(|&s| t.get(s).is_file() && t.get(s).git == Some(GitState::Tracked))
            .collect();
        let tracked_bytes: u64 = tracked.iter().map(|&s| t.get(s).bytes).sum();
        if !tracked.is_empty() && (tracked.len() >= 5 || tracked_bytes >= 100 * 1024) {
            let (_, rel) = c.repo_rel(id);
            let sev = if tracked.len() >= 50 || tracked_bytes >= 10 << 20 {
                Severity::High
            } else {
                Severity::Medium
            };
            c.push(
                "committed-artifact",
                sev,
                vec![id],
                format!("{} is committed to git", c.p(id)),
                format!(
                    "{} tracked files ({}) inside a directory classified as {} ({reason}).",
                    count(tracked.len() as u64),
                    bytes(tracked_bytes),
                    n.zone.as_str()
                ),
                "Stop tracking it and ignore it; regenerate it in the build. If it is \
                 committed on purpose (vendored deps, zero-installs), add it to the baseline.",
                Some(format!(
                    "git rm -r --cached {q} && echo {q}/ >> .gitignore",
                    q = shell_quote(&rel)
                )),
                Some(tracked_bytes),
            );
        } else if n.git == Some(GitState::Untracked) && n.disk >= 1 << 20 {
            let (_, rel) = c.repo_rel(id);
            c.push(
                "untracked-artifact",
                Severity::Medium,
                vec![id],
                format!("{} is not git-ignored", c.p(id)),
                format!(
                    "{} of {} ({reason}) shows up as untracked — one `git add .` away from history.",
                    bytes(n.disk),
                    n.zone.as_str()
                ),
                "Add it to .gitignore.",
                Some(format!("echo {}/ >> .gitignore", shell_quote(&rel))),
                Some(n.disk),
            );
        }
    }

    rebuildable.sort_by_key(|r| std::cmp::Reverse(r.1));
    let total: u64 = rebuildable.iter().map(|r| r.1).sum();
    if total >= 100 << 20 {
        let top: Vec<(NodeId, u64)> = rebuildable.iter().take(8).copied().collect();
        let list = top
            .iter()
            .map(|&(id, d)| format!("{} ({})", c.p(id), bytes(d)))
            .collect::<Vec<_>>()
            .join(", ");
        c.push(
            "rebuildable-space",
            if total >= 1 << 30 {
                Severity::Low
            } else {
                Severity::Info
            },
            top.iter().map(|x| x.0).collect(),
            format!(
                "{} of re-creatable build output and dependencies",
                bytes(total)
            ),
            format!("{} directories; largest: {list}.", rebuildable.len()),
            "Reclaim with the owning tool (`cargo clean`, `npm prune`, removing stale \
             virtualenvs) — they are recreated on the next build/install.",
            Some(format!(
                "du -sh {}",
                top.iter()
                    .map(|&(id, _)| shell_quote(&c.p(id)))
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
            Some(total),
        );
    }
}

fn large_files(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut tracked: Vec<NodeId> = Vec::new();
    let mut untracked: Vec<NodeId> = Vec::new();
    for id in 0..t.len() as NodeId {
        let n = t.get(id);
        if !n.is_file() || n.zone == Zone::Vcs {
            continue;
        }
        match n.git {
            Some(GitState::Tracked) if n.bytes >= 10 << 20 => tracked.push(id),
            Some(GitState::Untracked) if n.bytes >= 20 << 20 && n.zone.is_analysed() => {
                untracked.push(id)
            }
            _ => {}
        }
    }
    tracked.sort_by_key(|&id| std::cmp::Reverse(t.get(id).bytes));
    for &id in tracked.iter().take(10) {
        let b = t.get(id).bytes;
        let (_, rel) = c.repo_rel(id);
        c.push(
            "large-tracked-file",
            if b >= 50 << 20 {
                Severity::High
            } else {
                Severity::Medium
            },
            vec![id],
            format!("{} ({}) is tracked in git", c.p(id), bytes(b)),
            "Every version of a large file stays in every clone forever; GitHub warns at 50 MB \
             and rejects 100 MB."
                .into(),
            "Move it to Git LFS or external storage; rewrite history if it must leave the repo.",
            Some(format!(
                "git lfs track {q} && git lfs migrate import --include={q}",
                q = shell_quote(&rel)
            )),
            Some(b),
        );
    }
    if !untracked.is_empty() {
        untracked.sort_by_key(|&id| std::cmp::Reverse(t.get(id).bytes));
        untracked.truncate(10);
        let total: u64 = untracked.iter().map(|&id| t.get(id).bytes).sum();
        c.push(
            "large-untracked-file",
            Severity::Low,
            untracked.clone(),
            format!(
                "{} large untracked files ({})",
                untracked.len(),
                bytes(total)
            ),
            untracked
                .iter()
                .map(|&id| format!("{} ({})", c.p(id), bytes(t.get(id).bytes)))
                .collect::<Vec<_>>()
                .join(", "),
            "Ignore them or move them out of the work tree before someone commits them.",
            None,
            Some(total),
        );
    }
}

fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|x| x.1).unwrap_or("");
    matches!(
        lower.as_str(),
        ".env"
            | ".env.local"
            | ".env.production"
            | ".env.prod"
            | ".env.dev"
            | ".env.development"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | ".netrc"
            | ".pypirc"
            | ".pgpass"
            | "credentials"
            | "credentials.json"
            | "secrets.json"
            | "secrets.yaml"
            | "secrets.yml"
            | "service-account.json"
            | ".htpasswd"
            | "terraform.tfstate"
    ) || matches!(
        ext,
        "pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "kdbx" | "ppk"
    )
}

fn sensitive(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut tracked = Vec::new();
    let mut exposed = Vec::new();
    let mut fixtures = Vec::new();
    for id in 0..t.len() as NodeId {
        let n = t.get(id);
        if !n.is_file() || n.zone == Zone::Vcs || !is_sensitive_name(&n.name) {
            continue;
        }
        match n.git {
            Some(GitState::Tracked) if n.flags.has(Flags::TEST) => fixtures.push(id),
            Some(GitState::Tracked) => tracked.push(id),
            Some(GitState::Untracked) => exposed.push(id),
            _ => {}
        }
    }
    for (ids, sev, verb, action) in [
        (
            fixtures,
            Severity::Low,
            "tracked inside test directories",
            "Probably fixtures — confirm they hold no real credentials, then baseline this finding.",
        ),
        (
            tracked,
            Severity::High,
            "tracked in git",
            "Assume the secret is leaked: rotate it, remove the file from history, ignore it.",
        ),
        (
            exposed,
            Severity::Medium,
            "untracked but not ignored",
            "Add it to .gitignore before it gets committed.",
        ),
    ] {
        if ids.is_empty() {
            continue;
        }
        let shown: Vec<NodeId> = ids.iter().take(10).copied().collect();
        c.push(
            "sensitive-file",
            sev,
            shown.clone(),
            format!("{} secret-looking file(s) {verb}", ids.len()),
            format!(
                "By name only (contents are never read into the report): {}.",
                shown.iter().map(|&i| c.p(i)).collect::<Vec<_>>().join(", ")
            ),
            action,
            None,
            None,
        );
    }
}

fn hotspots(c: &mut Ctx<'_>) {
    let flagged: Vec<_> =
        c.a.hotspots
            .iter()
            .filter(|h| h.flagged(c.t))
            .take(10)
            .copied()
            .collect();
    for h in flagged {
        let n = c.t.get(h.node);
        let owner = n
            .owner
            .map(|o| format!("; main author {}", c.author(o)))
            .unwrap_or_default();
        let (_, rel) = c.repo_rel(h.node);
        c.push(
            "hotspot",
            Severity::Medium,
            vec![h.node],
            format!("Hotspot: {}", c.p(h.node)),
            format!(
                "{} commits ({:.1} recency-weighted, top {}%), complexity {} (top {}%), {} code lines{owner}.",
                n.commits,
                n.recent,
                top_pct(h.change_pct),
                n.complexity,
                top_pct(h.complexity_pct),
                n.code
            ),
            "Frequent change × high complexity is where defects concentrate. Add tests \
             around it before the next change; split it along the seams the history shows.",
            Some(format!(
                "git log --format='%h %as %an %s' -n 20 -- {}",
                shell_quote(&rel)
            )),
            None,
        );
    }
}

/// Percentile 0.97 → "top 3%" (never "top 0%").
fn top_pct(pct: f32) -> u32 {
    (((1.0 - pct) * 100.0).ceil() as u32).max(1)
}

fn cycles(c: &mut Ctx<'_>) {
    let Some(g) = &c.a.graph else { return };
    let in_scope: Vec<&crate::deps::Cycle> = g
        .cycles
        .iter()
        .filter(|cy| c.in_code_scope(cy.cut.0))
        .take(30)
        .collect();
    for cy in in_scope {
        let t = c.t;
        let dirs = cy.members.iter().filter(|&&m| t.get(m).is_dir()).count();
        // Inside one Rust crate, module cycles compile fine — an organisation smell only.
        let rust_only = cy.cut_files.iter().all(|&(a, b)| {
            [a, b].iter().all(|&f| {
                t.get(f)
                    .lang
                    .is_some_and(|l| crate::classify::lang(l).name == "Rust")
            })
        });
        // Third-party code is not ours to restructure.
        let vendored = cy.members.iter().any(|&m| t.get(m).zone == Zone::Vendored);
        let sev = if dirs == 0 || rust_only || vendored {
            Severity::Low
        } else if t.get(cy.parent).depth <= 1 && dirs >= 2 {
            Severity::High
        } else {
            Severity::Medium
        };
        let names: Vec<String> = cy
            .members
            .iter()
            .map(|&m| t.get(m).name.to_string())
            .collect();
        let shown = if names.len() > 4 {
            format!("{} … ({} members)", names[..4].join(" ↔ "), names.len())
        } else {
            names.join(" ↔ ")
        };
        let files = cy
            .cut_files
            .iter()
            .take(6)
            .map(|&(a, b)| format!("{} → {}", c.p(a), c.p(b)))
            .collect::<Vec<_>>()
            .join("; ");
        let cut_imports: u32 = cy.cuts.iter().map(|x| x.2).sum();
        let mut cut_list = cy
            .cuts
            .iter()
            .take(6)
            .map(|&(a, b, w)| format!("{} → {} ({w})", t.get(a).name, t.get(b).name))
            .collect::<Vec<_>>()
            .join(", ");
        if cy.cuts.len() > 6 {
            cut_list.push_str(&format!(" and {} more edges", cy.cuts.len() - 6));
        }
        c.push(
            "dep-cycle",
            sev,
            cy.members.clone(),
            format!("Dependency cycle in {}: {shown}", c.p(cy.parent)),
            format!(
                "{} imports form the cycle. Removing {cut_imports} import{} breaks it: {cut_list}. Files: {files}.",
                cy.weight,
                if cut_imports == 1 { "" } else { "s" }
            ),
            "Cycles make modules impossible to build, test or reason about separately. Change \
             those imports: move the shared piece down a layer or invert the dependency.",
            None,
            None,
        );
    }
}

fn coupling(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut shown = 0;
    for p in &c.a.history.coupling {
        if shown >= 10 {
            break;
        }
        let test = |id: NodeId| t.get(id).flags.has(Flags::TEST);
        // Tests co-changing with the code they exercise is expected, not hidden.
        if p.linked != Some(false)
            || test(p.a)
            || test(p.b)
            || t.get(p.a).parent == t.get(p.b).parent
        {
            continue;
        }
        if p.conf_ab.max(p.conf_ba) < 0.5 {
            continue;
        }
        shown += 1;
        c.push(
            "hidden-coupling",
            Severity::Medium,
            vec![p.a, p.b],
            format!("Hidden coupling: {} ⇄ {}", t.get(p.a).name, t.get(p.b).name),
            format!(
                "{} and {} changed together in {} commits (P(2nd|1st) {:.0}%, P(1st|2nd) {:.0}%, lift {:.1}) \
                 but neither imports the other, even through one intermediate file.",
                c.p(p.a),
                c.p(p.b),
                p.shared,
                p.conf_ab * 100.0,
                p.conf_ba * 100.0,
                p.lift
            ),
            "An implicit contract (duplicated logic, shared format, protocol) lives between \
             them. Make it explicit in one module, or at least document it next to both.",
            None,
            None,
        );
    }
}

fn knowledge(c: &mut Ctx<'_>) {
    let t = c.t;
    let a = c.a;
    if a.history.file_authors.is_empty() {
        return;
    }
    let flagged: HashSet<NodeId> = a
        .hotspots
        .iter()
        .filter(|h| h.flagged(t))
        .map(|h| h.node)
        .collect();
    let mut fan: Vec<u32> = a
        .history
        .file_authors
        .keys()
        .map(|&id| t.get(id).fan_in)
        .collect();
    fan.sort_unstable();
    let p95 = fan
        .get(fan.len() * 95 / 100)
        .copied()
        .unwrap_or(u32::MAX)
        .max(5);
    let mut risky: Vec<(NodeId, &Vec<u32>)> = a
        .history
        .file_authors
        .iter()
        .filter(|(id, auths)| {
            !auths.iter().any(|x| a.history.active.contains(x))
                && (flagged.contains(id) || t.get(**id).fan_in >= p95)
        })
        .map(|(id, auths)| (*id, auths))
        .collect();
    risky.sort_by_key(|(id, _)| (std::cmp::Reverse(t.get(*id).fan_in), *id));
    for (id, auths) in risky.into_iter().take(10) {
        let n = t.get(id);
        let names = auths
            .iter()
            .take(3)
            .map(|&x| {
                let last = a.authors.last_active.get(x as usize).copied().unwrap_or(0);
                let newest = a
                    .repos
                    .iter()
                    .filter_map(|r| r.history.as_ref())
                    .map(|h| h.last)
                    .max()
                    .unwrap_or(last);
                format!(
                    "{} (last commit {} earlier)",
                    c.author(x),
                    duration(newest - last)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let why = if flagged.contains(&id) {
            "a hotspot".to_string()
        } else {
            format!("imported by {} files", n.fan_in)
        };
        c.push(
            "knowledge-risk",
            Severity::Medium,
            vec![id],
            format!("No active author: {}", c.p(id)),
            format!("It is {why}; its authors by degree-of-authorship: {names}."),
            "Pair a current maintainer with this code, write down its invariants, and add \
             characterisation tests before changing it.",
            None,
            None,
        );
    }

    for tr in &a.history.truck {
        let is_repo = a.repos.iter().any(|r| r.node == tr.node);
        if tr.factor != 1 || tr.files < 20 {
            continue;
        }
        let who = tr.authors.first().map(|&x| c.author(x)).unwrap_or_default();
        c.push(
            "truck-factor",
            if is_repo { Severity::Medium } else { Severity::Low },
            vec![tr.node],
            format!("Truck factor 1 in {}", c.p(tr.node)),
            format!(
                "{who} is an author of more than half of its {} files with history (degree-of-authorship, Avelino et al.).",
                tr.files
            ),
            "Spread knowledge deliberately: reviews by others, pairing, docs for the parts only \
             one person changes.",
            None,
            None,
        );
    }
}

fn repos(c: &mut Ctx<'_>) {
    let t = c.t;
    let multi = c.a.repos.len() > 1;
    for r in &c.a.repos {
        if let Some(h) = &r.hygiene {
            let mut parts = Vec::new();
            if !h.unpushed_branches.is_empty() {
                parts.push(format!(
                    "{} unpushed branch(es) ({})",
                    h.unpushed_branches.len(),
                    h.unpushed_branches
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if h.stashes > 0 {
                parts.push(format!("{} stash(es)", h.stashes));
            }
            if h.dirty > 0 {
                parts.push(format!("{} modified file(s)", h.dirty));
            }
            if h.prunable_worktrees > 0 {
                parts.push(format!("{} prunable worktree(s)", h.prunable_worktrees));
            }
            if !parts.is_empty() {
                c.push(
                    "unpushed-work",
                    if multi && !h.unpushed_branches.is_empty() {
                        Severity::Low
                    } else {
                        Severity::Info
                    },
                    vec![r.node],
                    format!("Local-only work in {}", c.p(r.node)),
                    format!("{}.", parts.join(", ")),
                    "Push or delete what you still need; it is lost with this disk otherwise.",
                    Some(format!(
                        "git -C {} status --short --branch && git -C {} stash list",
                        shell_quote(&r.toplevel.to_string_lossy()),
                        shell_quote(&r.toplevel.to_string_lossy())
                    )),
                    None,
                );
            }
        }
        if let Some(hist) = &r.history
            && (r.shallow || hist.truncated)
        {
            c.push(
                "history-limits",
                Severity::Info,
                vec![r.node],
                format!("Partial history in {}", c.p(r.node)),
                format!(
                    "{}{} commits read from {} to {}; hotspots, ownership and coupling only reflect this window.",
                    if r.shallow { "Shallow clone. " } else { "Commit cap reached. " },
                    hist.commits,
                    date(hist.first),
                    date(hist.last)
                ),
                "Fetch full history (`git fetch --unshallow`) or raise --git-max-commits for a complete picture.",
                None,
                None,
            );
        }
        // .git much larger than the work tree.
        if let Some(git_dir) = t
            .children(r.node)
            .find(|&ch| &*t.get(ch).name == ".git" && t.get(ch).is_dir())
        {
            let vcs = t.get(git_dir).disk;
            let tracked: u64 = t
                .subtree(r.node)
                .filter(|&s| t.get(s).is_file() && t.get(s).git == Some(GitState::Tracked))
                .map(|s| t.get(s).disk)
                .sum();
            if vcs >= 200 << 20 && vcs >= tracked.saturating_mul(3) {
                c.push(
                    "vcs-bloat",
                    Severity::Low,
                    vec![git_dir],
                    format!("{} is {} — {:.0}× the tracked files", c.p(git_dir), bytes(vcs), vcs as f64 / tracked.max(1) as f64),
                    "Large blobs committed in the past stay in every clone even after deletion.".into(),
                    "Find the biggest historical blobs, then decide on `git gc`, LFS migration or a history rewrite.",
                    Some(format!(
                        "git -C {} rev-list --objects --all | git -C {} cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' | sort -k2 -n -r | head -20",
                        shell_quote(&r.toplevel.to_string_lossy()),
                        shell_quote(&r.toplevel.to_string_lossy())
                    )),
                    Some(vcs),
                );
            }
        }
    }
}

fn duplicates(c: &mut Ctx<'_>) {
    let groups = &c.a.duplicates;
    let wasted: u64 = groups.iter().map(|g| g.wasted).sum();
    if wasted < 1 << 20 {
        return;
    }
    let top: Vec<&crate::dupes::Group> = groups.iter().take(5).collect();
    let detail = top
        .iter()
        .map(|g| {
            format!(
                "{}× {} ({} and {})",
                g.nodes.len(),
                bytes(g.bytes),
                c.p(g.nodes[0]),
                c.p(g.nodes[1])
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    c.push(
        "duplicates",
        if wasted >= 100 << 20 { Severity::Low } else { Severity::Info },
        top.iter().flat_map(|g| g.nodes.iter().take(2).copied()).collect(),
        format!("{} in {} groups of identical files", bytes(wasted), count(groups.len() as u64)),
        format!("Largest: {detail}. (Upper bound: copy-on-write clones share blocks.)"),
        "Keep one copy (or hard-link / symlink) — but check the copies are not intentionally independent.",
        None,
        Some(wasted),
    );
}

fn big_files(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut big: Vec<NodeId> = (0..t.len() as NodeId)
        .filter(|&id| {
            let n = t.get(id);
            n.is_file()
                && n.is_analysed()
                && n.category == Category::Code
                && !n.flags.has(Flags::GENERATED)
                && !n.flags.has(Flags::TEST)
                && n.code >= 3000
                && c.in_code_scope(id)
        })
        .collect();
    big.sort_by_key(|&id| std::cmp::Reverse(t.get(id).code));
    for id in big.into_iter().take(10) {
        let n = t.get(id);
        c.push(
            "big-source-file",
            Severity::Low,
            vec![id],
            format!("{} has {} code lines", c.p(id), count(u64::from(n.code))),
            format!(
                "Complexity {}, imported by {} files, {} commits in the window.",
                n.complexity, n.fan_in, n.commits
            ),
            "Large files are hard to review and to load into an AI context window; split by responsibility.",
            None,
            None,
        );
    }
}

fn stale(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut hits = Vec::new();
    for r in &c.a.repos {
        let Some(h) = &r.history else { continue };
        if h.commits == 0 || r.shallow {
            continue;
        }
        let base = t.get(r.node).depth;
        for id in t.subtree(r.node) {
            let n = t.get(id);
            if n.is_dir()
                && n.depth > base
                && n.depth <= base + 2
                && n.zone == Zone::Source
                && n.git == Some(GitState::Tracked)
                && n.code >= 1000
                && n.last_commit == 0
                && !t
                    .ancestors(id)
                    .any(|anc| hits.iter().any(|&(x, _)| x == anc))
            {
                hits.push((id, h.first));
            }
        }
    }
    for (id, since) in hits.into_iter().take(10) {
        c.push(
            "stale-code",
            Severity::Info,
            vec![id],
            format!("{} unchanged since at least {}", c.p(id), date(since)),
            format!("{} code lines with no commit in the analysis window.", count(u64::from(t.get(id).code))),
            "Stable or abandoned? If nothing uses it, archiving it shrinks what humans and agents must read.",
            None,
            None,
        );
    }
}

fn portability(c: &mut Ctx<'_>) {
    let t = c.t;
    let mut collisions = Vec::new();
    let mut reserved = Vec::new();
    for id in 0..t.len() as NodeId {
        let n = t.get(id);
        if n.kind != Kind::Dir || !n.zone.is_analysed() || n.git.is_none() {
            continue;
        }
        let mut seen: HashMap<String, NodeId> = HashMap::new();
        for ch in t.children(id) {
            let cn = t.get(ch);
            if cn.git != Some(GitState::Tracked) {
                continue;
            }
            let lower = cn.name.to_lowercase();
            if let Some(&other) = seen.get(&lower) {
                collisions.push(other);
                collisions.push(ch);
            } else {
                seen.insert(lower, ch);
            }
            let stem = cn.name.split('.').next().unwrap_or("").to_ascii_uppercase();
            let is_reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && stem.as_bytes()[3].is_ascii_digit()
                    && stem.as_bytes()[3] != b'0');
            if is_reserved || cn.name.ends_with('.') || cn.name.ends_with(' ') {
                reserved.push(ch);
            }
        }
    }
    if !collisions.is_empty() {
        collisions.truncate(10);
        c.push(
            "portability",
            Severity::Low,
            collisions.clone(),
            "Paths that differ only by case".into(),
            collisions
                .iter()
                .map(|&i| c.p(i))
                .collect::<Vec<_>>()
                .join(", "),
            "On macOS/Windows checkouts one silently overwrites the other. Rename one.",
            None,
            None,
        );
    }
    if !reserved.is_empty() {
        reserved.truncate(10);
        c.push(
            "portability",
            Severity::Low,
            reserved.clone(),
            "Names that Windows cannot check out".into(),
            reserved.iter().map(|&i| c.p(i)).collect::<Vec<_>>().join(", "),
            "Reserved device names (CON, AUX, NUL, COM1…) and trailing dots/spaces fail on Windows. Rename them.",
            None,
            None,
        );
    }
}

fn scan_issues(c: &mut Ctx<'_>) {
    let issues = &c.a.issues;
    if issues.count == 0 {
        return;
    }
    let sample = issues
        .samples
        .iter()
        .take(5)
        .map(|(p, e)| format!("{p}: {e}"))
        .collect::<Vec<_>>()
        .join("; ");
    c.push(
        "scan-issues",
        if issues.count > 100 {
            Severity::Low
        } else {
            Severity::Info
        },
        vec![0],
        format!("{} entries could not be read", count(issues.count)),
        format!("Sizes below them are lower bounds. {sample}"),
        "Re-run with the needed permissions if those areas matter.",
        None,
        None,
    );
}

/// Mark findings absent from `baseline` as new.
pub fn mark_new(findings: &mut [Finding], baseline: &HashSet<String>) {
    for f in findings {
        f.new = !baseline.contains(&f.id);
    }
}

#[derive(Serialize, serde::Deserialize)]
pub struct Baseline {
    pub schema: String,
    pub findings: Vec<String>,
}

pub fn baseline_of(findings: &[Finding]) -> Baseline {
    let mut ids: Vec<String> = findings.iter().map(|f| f.id.clone()).collect();
    ids.sort();
    ids.dedup();
    Baseline {
        schema: "fsmap-baseline/1".into(),
        findings: ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_names() {
        assert!(is_sensitive_name(".env"));
        assert!(is_sensitive_name("id_ed25519"));
        assert!(is_sensitive_name("server.PEM"));
        assert!(!is_sensitive_name(".env.example"));
        assert!(!is_sensitive_name("id_rsa.pub"));
        assert!(!is_sensitive_name("keys.rs"));
    }
}
