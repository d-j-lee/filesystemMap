//! Import graph: resolve extracted imports to files inside the scan, then find
//! architecture cycles on the *lifted sibling graph*.
//!
//! Lifting: an edge `a → b` becomes an edge between the two children of `lca(a, b)` that
//! contain them. Every SCC of that graph consists of siblings, so a cycle is reported at
//! exactly the level where it exists ("`net/` ↔ `base/`"), and the same edge set yields
//! the dependency matrix for any directory.

pub mod extract;

use std::collections::{HashMap, HashSet};

use petgraph::graphmap::DiGraphMap;
use serde::Serialize;

use crate::classify::{ImportLang, lang};
use crate::git::PathIndex;
use crate::git::history::CouplingPair;
use crate::model::{Flags, Kind, NodeId, Tree};
use extract::Import;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    /// Import statements behind this edge.
    pub weight: u32,
    /// Child module reaching into its own parent module file (`super::`, `__init__.py`):
    /// idiomatic, excluded from cycle detection.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub up: bool,
    /// The edge does not shape the runtime architecture: every import behind it is
    /// deferred or type-only (indented Python import, TS `import type`, JS `import()`), or
    /// it starts or ends in test code. Excluded from cycle detection and the matrix.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub weak: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cycle {
    /// Directory whose children form the cycle.
    pub parent: NodeId,
    pub members: Vec<NodeId>,
    /// Import statements on lifted edges inside the cycle.
    pub weight: u32,
    /// Lifted edges whose removal makes the members acyclic (weighted greedy feedback arc
    /// set, Eades–Lin–Smyth), lightest first: (from, to, import count).
    pub cuts: Vec<(NodeId, NodeId, u32)>,
    /// The lightest of `cuts` (kept for consumers that show one suggestion).
    pub cut: (NodeId, NodeId, u32),
    /// File-level imports behind every cut edge (up to 5 per edge).
    pub cut_files: Vec<(NodeId, NodeId)>,
}

/// Weighted Eades–Lin–Smyth heuristic: order the vertices so that few, light edges point
/// backwards; those backward edges are the feedback arc set.
fn feedback_arcs(nodes: &[NodeId], edges: &[(NodeId, NodeId, u32)]) -> Vec<(NodeId, NodeId, u32)> {
    let mut alive: HashSet<NodeId> = nodes.iter().copied().collect();
    let weight_in = |v: NodeId, alive: &HashSet<NodeId>| -> i64 {
        edges
            .iter()
            .filter(|e| e.1 == v && alive.contains(&e.0))
            .map(|e| i64::from(e.2))
            .sum()
    };
    let weight_out = |v: NodeId, alive: &HashSet<NodeId>| -> i64 {
        edges
            .iter()
            .filter(|e| e.0 == v && alive.contains(&e.1))
            .map(|e| i64::from(e.2))
            .sum()
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    while !alive.is_empty() {
        let mut changed = true;
        while changed {
            changed = false;
            let mut sorted: Vec<NodeId> = alive.iter().copied().collect();
            sorted.sort_unstable();
            for v in sorted {
                if weight_out(v, &alive) == 0 {
                    right.push(v);
                    alive.remove(&v);
                    changed = true;
                } else if weight_in(v, &alive) == 0 {
                    left.push(v);
                    alive.remove(&v);
                    changed = true;
                }
            }
        }
        if let Some(&v) = alive.iter().max_by_key(|&&v| {
            (
                weight_out(v, &alive) - weight_in(v, &alive),
                std::cmp::Reverse(v),
            )
        }) {
            left.push(v);
            alive.remove(&v);
        }
    }
    right.reverse();
    left.extend(right);
    let pos: HashMap<NodeId, usize> = left.iter().enumerate().map(|(i, &v)| (v, i)).collect();
    let mut back: Vec<(NodeId, NodeId, u32)> = edges
        .iter()
        .copied()
        .filter(|e| pos[&e.0] > pos[&e.1])
        .collect();
    back.sort_by_key(|e| (e.2, e.0, e.1));
    back
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LangStats {
    pub lang: &'static str,
    pub files: u32,
    pub imports: u32,
    pub internal: u32,
    pub external: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Graph {
    pub edges: Vec<Edge>,
    pub cycles: Vec<Cycle>,
    pub stats: Vec<LangStats>,
}

/// Inputs gathered by the content stage.
#[derive(Default)]
pub struct Sources {
    pub imports: Vec<(NodeId, ImportLang, Vec<Import>)>,
    /// Text of manifests the resolver understands (Cargo.toml, package.json, go.mod, tsconfig).
    pub manifests: Vec<(NodeId, String)>,
}

/// tsconfig/jsconfig: (config dir, `baseUrl` dir if set, `paths` patterns → targets).
/// Without `baseUrl`, `paths` targets are relative to the config's directory and bare
/// specifiers resolve to packages, not local files.
type TsConfig = (String, Option<String>, Vec<(String, Vec<String>)>);

/// Lifted sibling edge → (weight, sample file edges).
type Lifted = HashMap<(NodeId, NodeId), (u32, Vec<(NodeId, NodeId)>)>;

struct Resolver<'a> {
    tree: &'a Tree,
    paths: &'a PathIndex,
    by_name: HashMap<&'a str, Vec<NodeId>>,
    py: HashMap<String, Vec<NodeId>>,
    jvm: HashMap<String, Vec<NodeId>>,
    /// Rust crate name (underscored) → directory of its library root file.
    crates: HashMap<String, String>,
    /// Directories holding a crate root file (lib/bin/src/bin), deepest first.
    rust_roots: Vec<String>,
    rust_root_files: HashSet<String>,
    /// npm package name → (package dir path, main entry).
    js_pkgs: HashMap<String, (String, Option<String>)>,
    /// tsconfig/jsconfig: (dir path, baseUrl dir path, [(pattern, targets)]), deepest first.
    ts: Vec<TsConfig>,
    /// go.mod: (module path, dir path), longest module first.
    go: Vec<(String, String)>,
}

fn parent_dir(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

fn file_name(p: &str) -> &str {
    p.rfind('/').map_or(p, |i| &p[i + 1..])
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else if rel.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Join and normalise `.` / `..`. `None` if it escapes the scan root.
fn join_norm(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

fn tree_has_file(paths: &PathIndex, tree: &Tree, p: &str) -> bool {
    paths
        .get(p)
        .is_some_and(|id| tree.get(id).kind == Kind::File)
}

/// Minimal TOML scan: `(section, key, string value)` for `key = "value"` lines.
/// `[[bin]]` array tables report section `bin`. Enough for Cargo.toml crate roots.
fn toml_entries(text: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(sec) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            section = sec.trim().to_string();
        } else if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = sec.trim().to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            if let Some(v) = v.strip_prefix('"').and_then(|v| v.split('"').next()) {
                out.push((section.clone(), k.trim().to_string(), v.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
fn toml_value<'t>(text: &'t str, section: &str, key: &str) -> Option<&'t str> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if in_section
            && let Some(rest) = line.strip_prefix(key)
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return Some(v.trim().trim_matches('"'));
        }
    }
    None
}

impl<'a> Resolver<'a> {
    fn new(tree: &'a Tree, paths: &'a PathIndex, src: &Sources) -> Self {
        let mut r = Resolver {
            tree,
            paths,
            by_name: HashMap::new(),
            py: HashMap::new(),
            jvm: HashMap::new(),
            crates: HashMap::new(),
            rust_roots: Vec::new(),
            rust_root_files: HashSet::new(),
            js_pkgs: HashMap::new(),
            ts: Vec::new(),
            go: Vec::new(),
        };
        let mut py_roots: Vec<String> = vec![String::new()];
        for (id, text) in &src.manifests {
            let path = tree.rel_path(*id);
            let dir = parent_dir(&path).to_string();
            match file_name(&path) {
                "Cargo.toml" => {
                    let entries = toml_entries(text);
                    let get = |sec: &str, key: &str| {
                        entries
                            .iter()
                            .find(|e| e.0 == sec && e.1 == key)
                            .map(|e| e.2.as_str())
                    };
                    let lib_path = get("lib", "path")
                        .map(|p| join(&dir, p))
                        .unwrap_or_else(|| join(&dir, "src/lib.rs"));
                    if let Some(name) = get("package", "name") {
                        let lib = get("lib", "name").unwrap_or(name).replace('-', "_");
                        if tree_has_file(paths, tree, &lib_path) {
                            r.crates.insert(lib, parent_dir(&lib_path).to_string());
                        }
                    }
                    let mut roots = vec![
                        lib_path,
                        join(&dir, "src/main.rs"),
                        join(&dir, "src/bin/main.rs"),
                    ];
                    roots.extend(
                        entries
                            .iter()
                            .filter(|e| {
                                matches!(e.0.as_str(), "bin" | "example" | "test" | "bench")
                                    && e.1 == "path"
                            })
                            .map(|e| join(&dir, &e.2)),
                    );
                    for root in roots {
                        if tree_has_file(paths, tree, &root) {
                            r.rust_roots.push(parent_dir(&root).to_string());
                            r.rust_root_files.insert(root);
                        }
                    }
                }
                "package.json" => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
                        && let Some(name) = v.get("name").and_then(|n| n.as_str())
                    {
                        let main = ["source", "module", "main", "types"]
                            .iter()
                            .find_map(|k| v.get(*k).and_then(|m| m.as_str()))
                            .map(str::to_string);
                        r.js_pkgs.insert(name.to_string(), (dir.clone(), main));
                    }
                }
                "go.mod" => {
                    if let Some(m) = text.lines().find_map(|l| l.trim().strip_prefix("module ")) {
                        r.go.push((m.trim().trim_matches('"').to_string(), dir.clone()));
                    }
                }
                "tsconfig.json" | "jsconfig.json" => {
                    let syn = lang(crate::classify::lang_by_name("JavaScript").unwrap_or(0)).syntax;
                    let (stripped, _) = crate::strip::strip(text.as_bytes(), &syn);
                    let cleaned = String::from_utf8_lossy(&stripped).replace(",}", "}");
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned)
                        && let Some(opts) = v.get("compilerOptions")
                    {
                        let base = opts
                            .get("baseUrl")
                            .and_then(|b| b.as_str())
                            .and_then(|b| join_norm(&dir, b));
                        let mut pats = Vec::new();
                        if let Some(map) = opts.get("paths").and_then(|p| p.as_object()) {
                            for (k, vals) in map {
                                let targets = vals
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|t| t.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                pats.push((k.clone(), targets));
                            }
                        }
                        r.ts.push((dir.clone(), base, pats));
                    }
                }
                "pyproject.toml" | "setup.py" | "setup.cfg" => {
                    py_roots.push(dir.clone());
                    py_roots.push(join(&dir, "src"));
                }
                _ => {}
            }
        }
        r.rust_roots.sort_by_key(|d| std::cmp::Reverse(d.len()));
        r.rust_roots.dedup();
        r.ts.sort_by_key(|t| std::cmp::Reverse(t.0.len()));
        r.go.sort_by_key(|g| std::cmp::Reverse(g.0.len()));
        py_roots.sort();
        py_roots.dedup();

        // Name and module indexes over analysed files.
        for id in 0..tree.len() as NodeId {
            let n = tree.get(id);
            if !n.is_file() || !n.is_analysed() {
                continue;
            }
            r.by_name.entry(&*n.name).or_default().push(id);
            let Some(l) = n.lang else { continue };
            let path = tree.rel_path(id);
            match lang(l).imports {
                Some(ImportLang::Python) => {
                    for name in r.python_names(&path, &py_roots) {
                        r.py.entry(name).or_default().push(id);
                    }
                }
                Some(ImportLang::Jvm) => {
                    let stem = path.rsplit_once('.').map_or(path.as_str(), |(s, _)| s);
                    let segs: Vec<&str> = stem.split('/').collect();
                    for k in 2..=segs.len() {
                        r.jvm
                            .entry(segs[segs.len() - k..].join("."))
                            .or_default()
                            .push(id);
                    }
                }
                _ => {}
            }
        }
        r
    }

    fn file(&self, p: &str) -> Option<NodeId> {
        self.paths
            .get(p)
            .filter(|&id| self.tree.get(id).kind == Kind::File)
    }

    fn dir(&self, p: &str) -> Option<NodeId> {
        self.paths
            .get(p)
            .filter(|&id| self.tree.get(id).kind == Kind::Dir)
    }

    fn has_file(&self, p: &str) -> bool {
        self.file(p).is_some()
    }

    /// Dotted module names for a Python file: from its `__init__.py` package chain and
    /// relative to each source root above it.
    fn python_names(&self, path: &str, roots: &[String]) -> Vec<String> {
        let stem = path
            .strip_suffix(".pyi")
            .or_else(|| path.strip_suffix(".py"))
            .unwrap_or(path);
        let module_path = stem.strip_suffix("/__init__").unwrap_or(stem);
        let mut out = Vec::new();
        // Package chain.
        let mut top = parent_dir(path).to_string();
        let mut pkg_root = None;
        while !top.is_empty() && self.has_file(&join(&top, "__init__.py")) {
            pkg_root = Some(parent_dir(&top).to_string());
            top = parent_dir(&top).to_string();
        }
        let base = pkg_root.unwrap_or_else(|| parent_dir(path).to_string());
        let rel = if base.is_empty() {
            module_path
        } else {
            module_path
                .strip_prefix(&format!("{base}/"))
                .unwrap_or(module_path)
        };
        if !rel.is_empty() {
            out.push(rel.replace('/', "."));
        }
        for root in roots {
            let rel = if root.is_empty() {
                Some(module_path)
            } else {
                module_path.strip_prefix(&format!("{root}/"))
            };
            if let Some(rel) = rel.filter(|r| !r.is_empty()) {
                out.push(rel.replace('/', "."));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn closest(&self, from: &str, cands: &[NodeId]) -> Option<NodeId> {
        cands.iter().copied().max_by_key(|&c| {
            let p = self.tree.rel_path(c);
            (common_prefix_len(from, &p), std::cmp::Reverse(p.len()))
        })
    }

    fn resolve(&self, from: NodeId, from_path: &str, imp: &Import) -> Resolved {
        match imp {
            Import::RustUse(segs) => self.rust(from_path, segs),
            Import::RustMod(name) => self.rust_mod(from_path, name),
            Import::Python {
                level,
                module,
                names,
                ..
            } => self.python(from_path, *level, module, names),
            Import::Js(spec) | Import::JsWeak(spec) => self.js(from_path, spec),
            Import::Go(p) => self.go(p),
            Import::Jvm(p) => self.jvm(from_path, p),
            Import::Include { path, quoted } => self.include(from_path, path, *quoted),
            Import::Path(p) => self.include(from_path, p, true),
        }
        .filter_self(from)
    }

    // ---- Rust ----

    /// The file that defines module directory `dir` (`dir.rs`, `dir/mod.rs`, or a crate
    /// root file when `dir` holds one).
    fn rust_module_file(&self, dir: &str) -> Option<NodeId> {
        if self.rust_roots.iter().any(|r| r == dir)
            && let Some(id) = self
                .file(&join(dir, "lib.rs"))
                .or_else(|| self.file(&join(dir, "main.rs")))
        {
            return Some(id);
        }
        self.file(&format!("{dir}.rs"))
            .or_else(|| self.file(&join(dir, "mod.rs")))
    }

    /// Longest prefix of `segs` that names a module file under `base`.
    fn rust_path(&self, base: &str, segs: &[String]) -> Option<NodeId> {
        for k in (1..=segs.len()).rev() {
            let p = join(base, &segs[..k].join("/"));
            if let Some(id) = self
                .file(&format!("{p}.rs"))
                .or_else(|| self.file(&join(&p, "mod.rs")))
            {
                return Some(id);
            }
        }
        self.rust_module_file(base)
    }

    /// Directory holding the child modules of the module defined by `from_path`.
    fn rust_module_dir(&self, from_path: &str) -> String {
        let dir = parent_dir(from_path);
        let name = file_name(from_path);
        if self.rust_root_files.contains(from_path)
            || matches!(name, "lib.rs" | "main.rs" | "mod.rs")
        {
            dir.to_string()
        } else {
            join(dir, name.trim_end_matches(".rs"))
        }
    }

    fn rust_mod(&self, from_path: &str, name: &str) -> Resolved {
        let base = join(&self.rust_module_dir(from_path), name);
        self.file(&format!("{base}.rs"))
            .or_else(|| self.file(&join(&base, "mod.rs")))
            .map_or(Resolved::Unresolved, Resolved::Internal)
    }

    fn rust(&self, from_path: &str, segs: &[String]) -> Resolved {
        let Some(first) = segs.first() else {
            return Resolved::External;
        };
        let dir = parent_dir(from_path);
        let crate_base = self
            .rust_roots
            .iter()
            .find(|r| r.is_empty() || dir == r.as_str() || dir.starts_with(&format!("{r}/")))
            .map_or(dir, String::as_str);
        let module_dir = self.rust_module_dir(from_path);
        let found = match first.as_str() {
            "crate" => self.rust_path(crate_base, &segs[1..]),
            "self" => self.rust_path(&module_dir, &segs[1..]),
            "super" => {
                let mut base = module_dir.clone();
                let mut i = 0;
                while segs.get(i).is_some_and(|s| s == "super") {
                    base = parent_dir(&base).to_string();
                    i += 1;
                }
                self.rust_path(&base, &segs[i..])
            }
            "std" | "core" | "alloc" | "proc_macro" | "test" => return Resolved::External,
            other => {
                if let Some(lib_dir) = self.crates.get(other) {
                    self.rust_path(lib_dir, &segs[1..])
                } else {
                    // 2018 uniform paths: a child module of this one.
                    let local = join(&module_dir, other);
                    if self.has_file(&format!("{local}.rs"))
                        || self.has_file(&join(&local, "mod.rs"))
                    {
                        self.rust_path(&module_dir, segs)
                    } else {
                        return Resolved::External;
                    }
                }
            }
        };
        found.map_or(Resolved::Unresolved, Resolved::Internal)
    }

    // ---- Python ----

    fn py_module(&self, from_path: &str, dotted: &str) -> Option<NodeId> {
        self.py.get(dotted).and_then(|c| self.closest(from_path, c))
    }

    fn py_file(&self, path: &str) -> Option<NodeId> {
        self.file(&format!("{path}.py"))
            .or_else(|| self.file(&format!("{path}.pyi")))
            .or_else(|| self.file(&join(path, "__init__.py")))
    }

    fn python(&self, from_path: &str, level: u8, module: &str, names: &[String]) -> Resolved {
        if level == 0 {
            for n in names {
                if let Some(id) = self.py_module(from_path, &format!("{module}.{n}")) {
                    return Resolved::Internal(id);
                }
            }
            let mut m = module;
            loop {
                if let Some(id) = self.py_module(from_path, m) {
                    return Resolved::Internal(id);
                }
                match m.rfind('.') {
                    Some(i) => m = &m[..i],
                    None => return Resolved::External,
                }
            }
        }
        let mut base = parent_dir(from_path).to_string();
        for _ in 1..level {
            if base.is_empty() {
                return Resolved::Unresolved;
            }
            base = parent_dir(&base).to_string();
        }
        let target = join(&base, &module.replace('.', "/"));
        for n in names {
            if let Some(id) = self.py_file(&join(&target, n)) {
                return Resolved::Internal(id);
            }
        }
        self.py_file(&target)
            .map_or(Resolved::Unresolved, Resolved::Internal)
    }

    // ---- JS / TS ----

    fn js_file(&self, p: &str) -> Option<NodeId> {
        const EXTS: [&str; 12] = [
            ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
            ".svelte", ".json",
        ];
        if let Some(id) = self.file(p) {
            return Some(id);
        }
        for (js, ts) in [
            (".js", ".ts"),
            (".js", ".tsx"),
            (".jsx", ".tsx"),
            (".mjs", ".mts"),
            (".cjs", ".cts"),
        ] {
            if let Some(stem) = p.strip_suffix(js)
                && let Some(id) = self.file(&format!("{stem}{ts}"))
            {
                return Some(id);
            }
        }
        for e in EXTS {
            if let Some(id) = self.file(&format!("{p}{e}")) {
                return Some(id);
            }
        }
        if self.dir(p).is_some() {
            for e in EXTS {
                if let Some(id) = self.file(&format!("{p}/index{e}")) {
                    return Some(id);
                }
            }
        }
        None
    }

    fn js(&self, from_path: &str, spec: &str) -> Resolved {
        if spec.contains(':') && !spec.starts_with('.') {
            return Resolved::External; // node:fs, https://…
        }
        let dir = parent_dir(from_path);
        if spec.starts_with('.') {
            return join_norm(dir, spec)
                .and_then(|p| self.js_file(&p).or_else(|| self.dir(&p)))
                .map_or(Resolved::Unresolved, Resolved::Internal);
        }
        if let Some((cfg_dir, base, pats)) = self
            .ts
            .iter()
            .find(|(d, _, _)| d.is_empty() || from_path.starts_with(&format!("{d}/")))
        {
            let paths_base = base.as_deref().unwrap_or(cfg_dir);
            for (pat, targets) in pats {
                let rest = match pat.strip_suffix('*') {
                    Some(prefix) => spec.strip_prefix(prefix),
                    None => (spec == pat).then_some(""),
                };
                if let Some(rest) = rest {
                    for t in targets {
                        if let Some(p) = join_norm(paths_base, &t.replace('*', rest))
                            && let Some(id) = self.js_file(&p)
                        {
                            return Resolved::Internal(id);
                        }
                    }
                }
            }
            if let Some(base) = base
                && let Some(p) = join_norm(base, spec)
                && let Some(id) = self.js_file(&p)
            {
                return Resolved::Internal(id);
            }
        }
        let (pkg, sub) = if spec.starts_with('@') {
            let mut it = spec.splitn(3, '/');
            let scope = it.next().unwrap_or("");
            let name = it.next().unwrap_or("");
            (format!("{scope}/{name}"), it.next().unwrap_or(""))
        } else {
            let mut it = spec.splitn(2, '/');
            (it.next().unwrap_or("").to_string(), it.next().unwrap_or(""))
        };
        if let Some((pdir, main)) = self.js_pkgs.get(&pkg) {
            let found = if !sub.is_empty() {
                join_norm(pdir, sub).and_then(|p| self.js_file(&p))
            } else {
                main.as_deref()
                    .and_then(|m| join_norm(pdir, m))
                    .and_then(|p| self.js_file(&p))
                    .or_else(|| self.js_file(&join(pdir, "src/index")))
                    .or_else(|| self.js_file(&join(pdir, "index")))
                    .or_else(|| self.dir(pdir))
            };
            return found.map_or(Resolved::Unresolved, Resolved::Internal);
        }
        Resolved::External
    }

    // ---- Go ----

    fn go(&self, path: &str) -> Resolved {
        for (module, dir) in &self.go {
            let rest = if path == module {
                Some("")
            } else {
                path.strip_prefix(&format!("{module}/"))
            };
            if let Some(rest) = rest {
                return self
                    .dir(&join(dir, rest))
                    .map_or(Resolved::Unresolved, Resolved::Internal);
            }
        }
        Resolved::External
    }

    // ---- JVM ----

    fn jvm(&self, from_path: &str, dotted: &str) -> Resolved {
        if dotted.ends_with(".*") {
            return Resolved::External;
        }
        let mut name = dotted;
        for _ in 0..2 {
            if let Some(c) = self.jvm.get(name) {
                return self
                    .closest(from_path, c)
                    .map_or(Resolved::Unresolved, Resolved::Internal);
            }
            match name.rfind('.') {
                Some(i) if name[..i].contains('.') => name = &name[..i],
                _ => break,
            }
        }
        Resolved::External
    }

    // ---- C family / path imports ----

    fn include(&self, from_path: &str, inc: &str, quoted: bool) -> Resolved {
        if quoted
            && let Some(p) = join_norm(parent_dir(from_path), inc)
            && let Some(id) = self.file(&p)
        {
            return Resolved::Internal(id);
        }
        if let Some(id) = self.file(inc) {
            return Resolved::Internal(id);
        }
        // Elsewhere in the tree (include paths set by the build). A bare generic name like
        // "config.h" only matches inside the includer's own top-level component, so vendored
        // libraries don't get wired to the main project's headers by name alone.
        let suffix = format!("/{inc}");
        let top = |p: &str| p.split('/').next().unwrap_or("").to_string();
        let from_top = top(from_path);
        let specific = inc.contains('/');
        if let Some(cands) = self.by_name.get(file_name(inc)) {
            let matching: Vec<NodeId> = cands
                .iter()
                .copied()
                .filter(|&c| {
                    let p = self.tree.rel_path(c);
                    p.ends_with(&suffix) && (specific || top(&p) == from_top)
                })
                .collect();
            if let Some(id) = self.closest(from_path, &matching) {
                return Resolved::Internal(id);
            }
        }
        if quoted {
            Resolved::Unresolved
        } else {
            Resolved::External
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    Internal(NodeId),
    External,
    Unresolved,
}

impl Resolved {
    fn filter_self(self, from: NodeId) -> Self {
        match self {
            Resolved::Internal(id) if id == from => Resolved::External,
            r => r,
        }
    }
}

/// Is `to` the parent-module file of `from` (Rust `mod.rs`/`lib.rs`/`<dir>.rs`,
/// Python `__init__.py`) — an idiomatic upward reference rather than a layering edge?
fn is_up_edge(tree: &Tree, from: NodeId, to: NodeId) -> bool {
    let t = tree.get(to);
    let to_parent = t.parent;
    let from_parent = tree.get(from).parent;
    match &*t.name {
        "mod.rs" | "lib.rs" | "main.rs" | "__init__.py" => {
            tree.is_ancestor_or_self(to_parent, from_parent)
        }
        n if n.ends_with(".rs") => {
            // `src/a.rs` is the module file of directory `src/a/`.
            let stem = n.trim_end_matches(".rs");
            tree.ancestors(from)
                .any(|d| tree.get(d).parent == to_parent && &*tree.get(d).name == stem)
        }
        _ => false,
    }
}

/// Resolve imports, build the edge list, set fan-in/out, find cycles, and mark coupling
/// pairs as statically linked or not.
pub fn build(
    tree: &mut Tree,
    paths: &PathIndex,
    src: &Sources,
    coupling: &mut [CouplingPair],
) -> Graph {
    let resolver = Resolver::new(tree, paths, src);
    // (weight, all imports deferred)
    let mut weights: HashMap<(NodeId, NodeId), (u32, bool)> = HashMap::new();
    let mut stats: HashMap<ImportLang, LangStats> = HashMap::new();
    for (from, il, imports) in &src.imports {
        let from_path = tree.rel_path(*from);
        let st = stats.entry(*il).or_insert_with(|| LangStats {
            lang: lang_label(*il),
            ..Default::default()
        });
        st.files += 1;
        for imp in imports {
            st.imports += 1;
            match resolver.resolve(*from, &from_path, imp) {
                Resolved::Internal(to) => {
                    st.internal += 1;
                    let deferred =
                        matches!(imp, Import::Python { nested: true, .. } | Import::JsWeak(_));
                    let e = weights.entry((*from, to)).or_insert((0, true));
                    e.0 += 1;
                    e.1 &= deferred;
                }
                Resolved::External | Resolved::Unresolved => st.external += 1,
            }
        }
    }
    drop(resolver);

    let mut edges: Vec<Edge> = weights
        .into_iter()
        .map(|((from, to), (weight, deferred))| Edge {
            from,
            to,
            weight,
            up: is_up_edge(tree, from, to),
            weak: deferred
                || tree.get(from).flags.has(Flags::TEST)
                || tree.get(to).flags.has(Flags::TEST),
        })
        .collect();
    edges.sort_unstable_by_key(|e| (e.from, e.to));

    // An import of a directory (a Go package) reaches every file directly in it.
    let reached = |tree: &Tree, to: NodeId| -> Vec<NodeId> {
        if tree.get(to).is_dir() {
            tree.children(to)
                .filter(|&c| tree.get(c).is_file())
                .collect()
        } else {
            vec![to]
        }
    };
    // Fan-in / fan-out (distinct files).
    let mut importers: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in &edges {
        tree.get_mut(e.from).fan_out += 1;
        for f in reached(tree, e.to) {
            if f != e.from {
                importers.entry(f).or_default().insert(e.from);
            }
        }
    }
    for (f, from) in importers {
        tree.get_mut(f).fan_in = from.len() as u32;
    }

    let cycles = find_cycles(tree, &edges);

    // Static link check for coupling pairs: direct edge or a shared neighbour.
    let mut nbrs: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in &edges {
        for to in reached(tree, e.to) {
            nbrs.entry(e.from).or_default().insert(to);
            nbrs.entry(to).or_default().insert(e.from);
        }
    }
    let supported = |id: NodeId| tree.get(id).lang.is_some_and(|l| lang(l).imports.is_some());
    for p in coupling.iter_mut() {
        if !supported(p.a) || !supported(p.b) {
            continue;
        }
        let na = nbrs.get(&p.a);
        let nb = nbrs.get(&p.b);
        let direct = na.is_some_and(|s| s.contains(&p.b));
        let two_hop = match (na, nb) {
            (Some(x), Some(y)) => x.iter().any(|n| y.contains(n)),
            _ => false,
        };
        p.linked = Some(direct || two_hop);
    }

    let mut stats: Vec<LangStats> = stats.into_values().collect();
    stats.sort_by_key(|s| std::cmp::Reverse(s.imports));
    Graph {
        edges,
        cycles,
        stats,
    }
}

fn lang_label(l: ImportLang) -> &'static str {
    match l {
        ImportLang::Rust => "Rust",
        ImportLang::Python => "Python",
        ImportLang::Js => "JavaScript/TypeScript",
        ImportLang::Go => "Go",
        ImportLang::Jvm => "Java/Kotlin/Scala",
        ImportLang::CFamily => "C/C++/Objective-C",
        ImportLang::PathImport => "Protobuf/Mojom",
    }
}

fn find_cycles(tree: &Tree, edges: &[Edge]) -> Vec<Cycle> {
    let mut lifted: Lifted = HashMap::new();
    for e in edges.iter().filter(|e| !e.up && !e.weak) {
        let l = tree.lca(e.from, e.to);
        if l == e.from || l == e.to {
            continue;
        }
        let a = tree.child_toward(l, e.from);
        let b = tree.child_toward(l, e.to);
        let entry = lifted.entry((a, b)).or_insert((0, Vec::new()));
        entry.0 += e.weight;
        if entry.1.len() < 5 {
            entry.1.push((e.from, e.to));
        }
    }
    let mut g: DiGraphMap<NodeId, u32> = DiGraphMap::new();
    for (&(a, b), &(w, _)) in &lifted {
        g.add_edge(a, b, w);
    }
    let mut cycles = Vec::new();
    for mut scc in petgraph::algo::tarjan_scc(&g) {
        if scc.len() < 2 {
            continue;
        }
        scc.sort_unstable();
        let set: HashSet<NodeId> = scc.iter().copied().collect();
        let inner: Vec<(NodeId, NodeId, u32)> = scc
            .iter()
            .flat_map(|&a| {
                g.edges(a)
                    .filter(|e| set.contains(&e.1))
                    .map(|(a, b, &w)| (a, b, w))
                    .collect::<Vec<_>>()
            })
            .collect();
        let weight = inner.iter().map(|e| e.2).sum();
        let cuts = feedback_arcs(&scc, &inner);
        let Some(&cut) = cuts.first() else { continue };
        let cut_files = cuts
            .iter()
            .flat_map(|c| lifted[&(c.0, c.1)].1.clone())
            .collect();
        cycles.push(Cycle {
            parent: tree.get(scc[0]).parent,
            members: scc,
            weight,
            cuts,
            cut,
            cut_files,
        });
    }
    cycles.sort_by(|a, b| {
        tree.get(a.parent)
            .depth
            .cmp(&tree.get(b.parent).depth)
            .then(b.weight.cmp(&a.weight))
            .then(a.members.cmp(&b.members))
    });
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_arc_set_breaks_the_light_back_edge() {
        // helper→utils ×13, utils→helper ×1, plus a 3-cycle a→b→c→a with c→a lightest.
        let fas = feedback_arcs(&[1, 2], &[(1, 2, 13), (2, 1, 1)]);
        assert_eq!(fas, vec![(2, 1, 1)]);
        let fas = feedback_arcs(&[3, 4, 5], &[(3, 4, 5), (4, 5, 4), (5, 3, 1)]);
        assert_eq!(fas, vec![(5, 3, 1)]);
    }

    #[test]
    fn path_helpers() {
        assert_eq!(join_norm("a/b", "../c/./d"), Some("a/c/d".into()));
        assert_eq!(join_norm("a", "../../x"), None);
        assert_eq!(join_norm("", "x"), Some("x".into()));
        assert_eq!(parent_dir("a/b/c.rs"), "a/b");
        assert_eq!(parent_dir("c.rs"), "");
        assert_eq!(common_prefix_len("a/b/c", "a/b/d"), 2);
        let toml = "[package]\nname = \"my-crate\"\n[lib]\nname = \"mylib\"\n";
        assert_eq!(toml_value(toml, "package", "name"), Some("my-crate"));
        assert_eq!(toml_value(toml, "lib", "name"), Some("mylib"));
        let bins = toml_entries("[[bin]]\nname = \"rg\"\npath = \"crates/core/main.rs\"\n");
        assert!(bins.contains(&("bin".into(), "path".into(), "crates/core/main.rs".into())));
    }
}
