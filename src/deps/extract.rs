//! Per-language import extraction over comment-stripped source (see `strip`).
//! Heuristic by design (decision D-04): shallow syntax, no macro or build-system evaluation.

use std::sync::LazyLock;

use regex::Regex;

use crate::classify::ImportLang;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Import {
    /// Rust `use` / `extern crate` path, e.g. `["crate", "scan", "scan"]`.
    RustUse(Vec<String>),
    /// Rust `mod name;` — the declaring file owns the child module file.
    RustMod(String),
    /// Python: `level` leading dots, dotted `module`, and imported `names` for `from … import`.
    /// `nested`: indented (inside a function or `if TYPE_CHECKING:`), so not executed at
    /// import time — it cannot create an import-time cycle.
    Python {
        level: u8,
        module: String,
        names: Vec<String>,
        nested: bool,
    },
    /// JS/TS module specifier as written (static `import`/`export … from`/`require`).
    Js(String),
    /// JS/TS import that does not run at module evaluation: dynamic `import("…")`, or a
    /// type-only `import type` / `export type` (erased by the compiler).
    JsWeak(String),
    /// Go import path.
    Go(String),
    /// Java / Kotlin / Scala / Groovy dotted import.
    Jvm(String),
    /// C-family `#include` / `#import`; `quoted` = `"…"` rather than `<…>`.
    Include { path: String, quoted: bool },
    /// Protobuf / Mojom `import "a/b.proto";`.
    Path(String),
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("valid regex"));
    };
}

re!(
    RUST_USE,
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?use[ \t]+([^;]+);"
);
re!(
    RUST_MOD,
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*;"
);
re!(
    RUST_EXTERN,
    r"(?m)^[ \t]*extern[ \t]+crate[ \t]+([A-Za-z_][A-Za-z0-9_]*)"
);
re!(
    PY_IMPORT,
    r"(?m)^([ \t]*)import[ \t]+([\w.]+(?:[ \t]+as[ \t]+\w+)?(?:[ \t]*,[ \t]*[\w.]+(?:[ \t]+as[ \t]+\w+)?)*)"
);
re!(
    PY_FROM,
    r"(?m)^([ \t]*)from[ \t]+(\.*)([\w.]*)[ \t]+import[ \t]+(\([^)]*\)|[^\n]+)"
);
// `import … from '…'` / `export … from '…'`; group 2 = `type` modifier, group 3 = clause.
// The clause may span lines but never contains quotes, so a side-effect import without
// `from` cannot swallow the next statement.
re!(
    JS_FROM,
    r#"\b(import|export)(\s+type)?\s+([^;'"]*?)\s*\bfrom\s*['"]([^'"\n]+)['"]"#
);
re!(
    JS_TYPE_CLAUSE,
    r"^\{\s*(?:type\s+[\w$]+(?:\s+as\s+[\w$]+)?\s*,?\s*)+\}$"
);
re!(JS_SIDE, r#"\bimport\s*['"]([^'"\n]+)['"]"#);
re!(JS_REQUIRE, r#"\brequire\s*\(\s*['"]([^'"\n]+)['"]\s*\)"#);
re!(JS_DYNAMIC, r#"\bimport\s*\(\s*['"]([^'"\n]+)['"]\s*\)"#);
re!(
    GO_SINGLE,
    r#"(?m)^[ \t]*import[ \t]+(?:[\w.]+[ \t]+)?"([^"]+)""#
);
re!(GO_BLOCK, r"(?s)\bimport\s*\(([^)]*)\)");
re!(GO_SPEC, r#"(?m)^[ \t]*(?:[\w.]+[ \t]+)?"([^"]+)""#);
re!(
    JVM_IMPORT,
    r"(?m)^[ \t]*import[ \t]+(?:static[ \t]+)?(\w+(?:\.\w+)*)(\.\*|\._|\.\{[^}]*\})?"
);
re!(
    C_INCLUDE,
    r#"(?m)^[ \t]*#[ \t]*(?:include|import)[ \t]*([<"])([^>"\n]+)[>"]"#
);
re!(
    PATH_IMPORT,
    r#"(?m)^[ \t]*import[ \t]+(?:public[ \t]+|weak[ \t]+)?"([^"]+)"[ \t]*;"#
);

pub fn extract(lang: ImportLang, stripped: &str) -> Vec<Import> {
    let mut out = Vec::new();
    match lang {
        ImportLang::Rust => {
            for c in RUST_USE.captures_iter(stripped) {
                for path in parse_use_tree(&c[1]) {
                    out.push(Import::RustUse(path));
                }
            }
            for c in RUST_EXTERN.captures_iter(stripped) {
                out.push(Import::RustUse(vec![c[1].to_string()]));
            }
            for c in RUST_MOD.captures_iter(stripped) {
                out.push(Import::RustMod(c[1].to_string()));
            }
        }
        ImportLang::Python => {
            for c in PY_IMPORT.captures_iter(stripped) {
                for part in c[2].split(',') {
                    let module = part.split_whitespace().next().unwrap_or("");
                    if !module.is_empty() {
                        out.push(Import::Python {
                            level: 0,
                            module: module.to_string(),
                            names: Vec::new(),
                            nested: !c[1].is_empty(),
                        });
                    }
                }
            }
            for c in PY_FROM.captures_iter(stripped) {
                let names = c[4]
                    .trim_matches(|ch: char| ch == '(' || ch == ')' || ch.is_whitespace())
                    .split(',')
                    .filter_map(|n| n.split_whitespace().next())
                    .filter(|n| *n != "*" && n.chars().all(|ch| ch.is_alphanumeric() || ch == '_'))
                    .map(str::to_string)
                    .collect();
                out.push(Import::Python {
                    level: c[2].len().min(u8::MAX as usize) as u8,
                    module: c[3].to_string(),
                    names,
                    nested: !c[1].is_empty(),
                });
            }
        }
        ImportLang::Js => {
            for c in JS_FROM.captures_iter(stripped) {
                let type_only = c.get(2).is_some() || JS_TYPE_CLAUSE.is_match(c[3].trim());
                let spec = c[4].to_string();
                out.push(if type_only {
                    Import::JsWeak(spec)
                } else {
                    Import::Js(spec)
                });
            }
            for re in [&*JS_SIDE, &*JS_REQUIRE] {
                for c in re.captures_iter(stripped) {
                    out.push(Import::Js(c[1].to_string()));
                }
            }
            for c in JS_DYNAMIC.captures_iter(stripped) {
                out.push(Import::JsWeak(c[1].to_string()));
            }
        }
        ImportLang::Go => {
            for c in GO_SINGLE.captures_iter(stripped) {
                out.push(Import::Go(c[1].to_string()));
            }
            for block in GO_BLOCK.captures_iter(stripped) {
                for c in GO_SPEC.captures_iter(&block[1]) {
                    out.push(Import::Go(c[1].to_string()));
                }
            }
        }
        ImportLang::Jvm => {
            for c in JVM_IMPORT.captures_iter(stripped) {
                let base = c[1].trim_end_matches('.');
                match c.get(2).map(|m| m.as_str()) {
                    Some(group) if group.starts_with(".{") => {
                        for name in group[2..group.len() - 1].split(',') {
                            let name = name.split("=>").next().unwrap_or("").trim();
                            if !name.is_empty() && name != "_" {
                                out.push(Import::Jvm(format!("{base}.{name}")));
                            }
                        }
                    }
                    Some(_) => out.push(Import::Jvm(format!("{base}.*"))),
                    None => out.push(Import::Jvm(base.to_string())),
                }
            }
        }
        ImportLang::CFamily => {
            for c in C_INCLUDE.captures_iter(stripped) {
                out.push(Import::Include {
                    path: c[2].trim().to_string(),
                    quoted: &c[1] == "\"",
                });
            }
        }
        ImportLang::PathImport => {
            for c in PATH_IMPORT.captures_iter(stripped) {
                out.push(Import::Path(c[1].to_string()));
            }
        }
    }
    out.dedup();
    out
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Sep,
    Open,
    Close,
    Comma,
    Star,
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b':' if b.get(i + 1) == Some(&b':') => {
                out.push(Tok::Sep);
                i += 2;
            }
            b'{' => {
                out.push(Tok::Open);
                i += 1;
            }
            b'}' => {
                out.push(Tok::Close);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                // Raw identifiers: `r#type`.
                out.push(Tok::Ident(s[start..i].to_string()));
            }
            _ => i += 1,
        }
    }
    out
}

/// Expand a Rust use-tree (`crate::{a::{b, c as d}, e::*}`) into full paths.
/// `self` leaves and globs resolve to their parent path; aliases are dropped.
pub fn parse_use_tree(s: &str) -> Vec<Vec<String>> {
    let toks = tokenize(s);
    let mut out = Vec::new();
    let mut pos = 0;
    let mut prefix = Vec::new();
    parse_tree(&toks, &mut pos, &mut prefix, &mut out);
    out
}

fn parse_tree(t: &[Tok], pos: &mut usize, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    let base = prefix.len();
    loop {
        match t.get(*pos) {
            Some(Tok::Sep) => *pos += 1,
            Some(Tok::Ident(id)) if id != "as" => {
                prefix.push(id.clone());
                *pos += 1;
                if t.get(*pos) == Some(&Tok::Sep) {
                    *pos += 1;
                    if t.get(*pos) == Some(&Tok::Open) {
                        *pos += 1;
                        loop {
                            parse_tree(t, pos, prefix, out);
                            match t.get(*pos) {
                                Some(Tok::Comma) => {
                                    *pos += 1;
                                    if t.get(*pos) == Some(&Tok::Close) {
                                        *pos += 1;
                                        break;
                                    }
                                }
                                Some(Tok::Close) => {
                                    *pos += 1;
                                    break;
                                }
                                _ => break,
                            }
                        }
                        prefix.truncate(base);
                        return;
                    }
                    continue;
                }
                break;
            }
            Some(Tok::Open) => {
                // Top-level group: `use {a, b};`
                *pos += 1;
                loop {
                    parse_tree(t, pos, prefix, out);
                    match t.get(*pos) {
                        Some(Tok::Comma) => *pos += 1,
                        Some(Tok::Close) => {
                            *pos += 1;
                            break;
                        }
                        _ => break,
                    }
                }
                prefix.truncate(base);
                return;
            }
            Some(Tok::Star) => {
                *pos += 1;
                break;
            }
            _ => break,
        }
    }
    if matches!(t.get(*pos), Some(Tok::Ident(a)) if a == "as") {
        *pos += 1;
        if matches!(t.get(*pos), Some(Tok::Ident(_))) {
            *pos += 1;
        }
    }
    let mut path = prefix.clone();
    if path.last().is_some_and(|s| s == "self") {
        path.pop();
    }
    if !path.is_empty() {
        out.push(path);
    }
    prefix.truncate(base);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(s: &str) -> Vec<String> {
        parse_use_tree(s)
            .into_iter()
            .map(|p| p.join("::"))
            .collect()
    }

    #[test]
    fn rust_use_trees() {
        assert_eq!(paths("crate::scan::scan"), ["crate::scan::scan"]);
        assert_eq!(
            paths("crate::{model::{Tree, Node as N}, scan::*, self}"),
            [
                "crate::model::Tree",
                "crate::model::Node",
                "crate::scan",
                "crate"
            ]
        );
        assert_eq!(paths("std::io::{self, Read}"), ["std::io", "std::io::Read"]);
        assert_eq!(paths("::anyhow::Result"), ["anyhow::Result"]);
        assert_eq!(paths("super::super::x"), ["super::super::x"]);
        assert_eq!(paths("{a::b, c}"), ["a::b", "c"]);
    }

    #[test]
    fn rust_statements() {
        let src = "use crate::a;\npub(crate) use super::b::{C,\n  D};\nextern crate serde;\nlet x = \"use nope\";\nmod inline { }\npub(crate) mod child;\n";
        let got = extract(ImportLang::Rust, src);
        assert_eq!(
            got,
            vec![
                Import::RustUse(vec!["crate".into(), "a".into()]),
                Import::RustUse(vec!["super".into(), "b".into(), "C".into()]),
                Import::RustUse(vec!["super".into(), "b".into(), "D".into()]),
                Import::RustUse(vec!["serde".into()]),
                Import::RustMod("child".into()),
            ]
        );
    }

    #[test]
    fn python() {
        let src = "import os, pkg.sub as s\nfrom . import x\nfrom ..core.util import (a,\n  b as c)\nfrom m import *\ndef f():\n    import lazy\n";
        let got = extract(ImportLang::Python, src);
        assert_eq!(got.len(), 6);
        assert!(got.contains(&Import::Python {
            level: 2,
            module: "core.util".into(),
            names: vec!["a".into(), "b".into()],
            nested: false
        }));
        assert!(got.contains(&Import::Python {
            level: 1,
            module: String::new(),
            names: vec!["x".into()],
            nested: false
        }));
        assert!(got.contains(&Import::Python {
            level: 0,
            module: "lazy".into(),
            names: vec![],
            nested: true
        }));
    }

    #[test]
    fn js() {
        let src = "import a from './a';\nimport {\n b } from \"../b.js\";\nimport './side.css';\nconst c = require('c');\nconst d = await import('./d');\nexport * from './e';\n";
        let got: Vec<_> = extract(ImportLang::Js, src)
            .into_iter()
            .map(|i| match i {
                Import::Js(s) | Import::JsWeak(s) => s,
                _ => unreachable!(),
            })
            .collect();
        for want in ["./a", "../b.js", "./side.css", "c", "./d", "./e"] {
            assert!(
                got.contains(&want.to_string()),
                "{want} missing from {got:?}"
            );
        }
    }

    #[test]
    fn js_type_only_imports_are_weak() {
        let src = "import type { A } from './a'\nimport { type B, type C as D } from './b'\nimport { type E, f } from './c'\nexport type { G } from './g'\nimport './side'\nimport h from './h'\n";
        let got = extract(ImportLang::Js, src);
        let weak: Vec<&str> = got
            .iter()
            .filter_map(|i| match i {
                Import::JsWeak(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let strong: Vec<&str> = got
            .iter()
            .filter_map(|i| match i {
                Import::Js(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(weak, ["./a", "./b", "./g"]);
        assert!(
            strong.contains(&"./c") && strong.contains(&"./side") && strong.contains(&"./h"),
            "{strong:?}"
        );
    }

    #[test]
    fn go_jvm_c_proto() {
        let go = "import \"fmt\"\nimport (\n  \"example.com/m/pkg\"\n  alias \"example.com/m/other\"\n)\n";
        assert_eq!(extract(ImportLang::Go, go).len(), 3);
        let jvm = "import java.util.List;\nimport static a.b.C.m;\nimport a.b.*;\nimport scala.collection.{Map, Set => S}\n";
        let got = extract(ImportLang::Jvm, jvm);
        assert!(got.contains(&Import::Jvm("a.b.C.m".into())));
        assert!(got.contains(&Import::Jvm("a.b.*".into())));
        assert!(got.contains(&Import::Jvm("scala.collection.Set".into())));
        let c = "#include \"base/values.h\"\n#  include <vector>\n#import \"x.h\"\n";
        assert_eq!(
            extract(ImportLang::CFamily, c)[0],
            Import::Include {
                path: "base/values.h".into(),
                quoted: true
            }
        );
        let proto = "import \"a/b.proto\";\nimport public \"c.proto\";\n";
        assert_eq!(extract(ImportLang::PathImport, proto).len(), 2);
    }
}
