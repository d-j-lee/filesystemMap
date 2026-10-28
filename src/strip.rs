//! Comment stripping and line statistics in one pass.
//!
//! Output keeps the byte layout (comments become spaces, newlines stay) so later regexes
//! see real code and line numbers still line up. String literals are kept — import paths
//! live inside them — except Python triple-quoted strings, which are treated as comments
//! (they are almost always docstrings and often contain example `import` lines).

use crate::classify::{Strings, Syntax};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineStats {
    pub lines: u64,
    pub code: u64,
    pub comments: u64,
    pub blank: u64,
    /// Σ logical indentation over code lines (Tornhill's language-neutral complexity proxy).
    pub complexity: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    LineComment,
    Block(u32),
    /// Inside a quoted string closed by `quote`; `multiline` strings may span lines.
    Str {
        quote: u8,
        multiline: bool,
    },
    /// Rust raw string closed by `"` followed by `hashes` `#`s.
    RawStr(u32),
    /// Python triple-quoted string (treated as a comment).
    Triple(u8),
}

pub fn strip(src: &[u8], syn: &Syntax) -> (Vec<u8>, LineStats) {
    let mut out = src.to_vec();
    let mut state = State::Normal;
    let mut i = 0usize;
    let n = src.len();

    // Per-line flags.
    let mut line_has_code = false;
    let mut line_has_comment = false;
    let mut line_start = 0usize;
    let mut stats = LineStats::default();
    let mut code_indents: Vec<(u32, u32)> = Vec::new(); // (tabs, spaces) of code lines

    let block = syn.block;
    let lines = syn.line;

    let finish_line = |start: usize,
                       has_code: bool,
                       has_comment: bool,
                       stats: &mut LineStats,
                       indents: &mut Vec<(u32, u32)>| {
        stats.lines += 1;
        if has_code {
            stats.code += 1;
            let (mut tabs, mut spaces) = (0u32, 0u32);
            for &b in &src[start..] {
                match b {
                    b'\t' => tabs += 1,
                    b' ' => spaces += 1,
                    _ => break,
                }
            }
            indents.push((tabs, spaces));
        } else if has_comment {
            stats.comments += 1;
        } else {
            stats.blank += 1;
        }
    };

    while i < n {
        let b = src[i];
        if b == b'\n' {
            finish_line(
                line_start,
                line_has_code,
                line_has_comment,
                &mut stats,
                &mut code_indents,
            );
            line_has_code = false;
            line_has_comment = false;
            line_start = i + 1;
            match state {
                State::LineComment => state = State::Normal,
                // Unterminated single-line string (no `\` continuation): recover.
                State::Str {
                    multiline: false, ..
                } if i == 0 || src[i - 1] != b'\\' => state = State::Normal,
                _ => {}
            }
            i += 1;
            continue;
        }
        let is_ws = b == b' ' || b == b'\t' || b == b'\r';
        match state {
            State::Normal => {
                // Block comment start (checked before line comments: Lua `--[[` vs `--`).
                if let Some((open, _)) = block
                    && src[i..].starts_with(open.as_bytes())
                {
                    state = State::Block(1);
                    blank(&mut out, i, open.len());
                    line_has_comment = true;
                    i += open.len();
                    continue;
                }
                if let Some(tok) = lines.iter().find(|t| {
                    src[i..].starts_with(t.as_bytes()) && line_comment_ok(src, i, line_start, t)
                }) {
                    state = State::LineComment;
                    blank(&mut out, i, tok.len());
                    line_has_comment = true;
                    i += tok.len();
                    continue;
                }
                if !is_ws {
                    line_has_code = true;
                }
                match (syn.strings, b) {
                    (Strings::None, _) => {}
                    (Strings::Python, b'"' | b'\'') if src[i..].starts_with(&[b, b, b]) => {
                        state = State::Triple(b);
                        line_has_code = line_has_code_before(src, line_start, i);
                        line_has_comment = true;
                        blank(&mut out, i, 3);
                        i += 3;
                        continue;
                    }
                    (Strings::Rust, b'r')
                        if !prev_is_ident(src, i) || prev_is_byte_prefix(src, i) =>
                    {
                        let mut j = i + 1;
                        while j < n && src[j] == b'#' {
                            j += 1;
                        }
                        if j < n && src[j] == b'"' {
                            state = State::RawStr((j - i - 1) as u32);
                            i = j + 1;
                            continue;
                        }
                    }
                    (Strings::Rust | Strings::C | Strings::Go, b'\'') => {
                        if let Some(end) = char_literal_end(src, i) {
                            i = end;
                            continue;
                        }
                    }
                    (_, b'"') => {
                        state = State::Str {
                            quote: b'"',
                            multiline: false,
                        };
                    }
                    (Strings::Js | Strings::Python | Strings::Script, b'\'') => {
                        state = State::Str {
                            quote: b'\'',
                            multiline: false,
                        };
                    }
                    (Strings::Js | Strings::Go, b'`') => {
                        state = State::Str {
                            quote: b'`',
                            multiline: true,
                        };
                    }
                    _ => {}
                }
                i += 1;
            }
            State::LineComment => {
                if !is_ws {
                    line_has_comment = true;
                }
                out[i] = b' ';
                i += 1;
            }
            State::Block(depth) => {
                let (open, close) = block.expect("block state needs block syntax");
                if src[i..].starts_with(close.as_bytes()) {
                    blank(&mut out, i, close.len());
                    i += close.len();
                    state = if depth > 1 {
                        State::Block(depth - 1)
                    } else {
                        State::Normal
                    };
                    continue;
                }
                if syn.nested && src[i..].starts_with(open.as_bytes()) {
                    blank(&mut out, i, open.len());
                    i += open.len();
                    state = State::Block(depth + 1);
                    continue;
                }
                if !is_ws {
                    line_has_comment = true;
                }
                out[i] = b' ';
                i += 1;
            }
            State::Str { quote, .. } => {
                if !is_ws {
                    line_has_code = true;
                }
                let raw_backtick = quote == b'`' && syn.strings == Strings::Go;
                if b == b'\\' && !raw_backtick {
                    // Skip the escaped byte, but never a newline: it must still end the line.
                    i += if src.get(i + 1).is_some_and(|&c| c != b'\n') {
                        2
                    } else {
                        1
                    };
                    continue;
                }
                if b == quote {
                    state = State::Normal;
                }
                i += 1;
            }
            State::RawStr(hashes) => {
                if !is_ws {
                    line_has_code = true;
                }
                if b == b'"' {
                    let h = hashes as usize;
                    if src[i + 1..].iter().take(h).filter(|&&c| c == b'#').count() == h {
                        state = State::Normal;
                        i += 1 + h;
                        continue;
                    }
                }
                i += 1;
            }
            State::Triple(q) => {
                if src[i..].starts_with(&[q, q, q]) {
                    line_has_comment = true;
                    blank(&mut out, i, 3);
                    i += 3;
                    state = State::Normal;
                    continue;
                }
                if b == b'\\' && src.get(i + 1).is_some_and(|&c| c != b'\n') {
                    blank(&mut out, i, 2);
                    i += 2;
                    continue;
                }
                if !is_ws {
                    line_has_comment = true;
                }
                out[i] = b' ';
                i += 1;
            }
        }
    }
    if line_start < n {
        finish_line(
            line_start,
            line_has_code,
            line_has_comment,
            &mut stats,
            &mut code_indents,
        );
    }
    stats.complexity = indentation_complexity(&code_indents);
    (out, stats)
}

/// Line counts for text without a known comment syntax.
pub fn plain_stats(src: &[u8]) -> LineStats {
    let mut stats = LineStats::default();
    for line in src.split(|&b| b == b'\n') {
        stats.lines += 1;
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            stats.blank += 1;
        }
    }
    if src.last() == Some(&b'\n') {
        // `split` yields a trailing empty slice after the final newline.
        stats.lines -= 1;
        stats.blank -= 1;
    }
    if src.is_empty() {
        stats = LineStats::default();
    }
    stats
}

fn blank(out: &mut [u8], at: usize, len: usize) {
    let end = (at + len).min(out.len());
    for b in &mut out[at..end] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn line_has_code_before(src: &[u8], line_start: usize, i: usize) -> bool {
    src[line_start..i].iter().any(|b| !b.is_ascii_whitespace())
}

/// Alphabetic tokens (`REM`) only start a comment at line start; `#` only at a word start
/// (so shell `${#arr}` and `$#` are code).
fn line_comment_ok(src: &[u8], i: usize, line_start: usize, tok: &str) -> bool {
    let first = tok.as_bytes()[0];
    if first.is_ascii_alphabetic() || tok == "::" {
        return src[line_start..i].iter().all(|b| b.is_ascii_whitespace());
    }
    if first == b'#' {
        return i == line_start || matches!(src[i - 1], b' ' | b'\t' | b';' | b'\r');
    }
    true
}

fn prev_is_ident(src: &[u8], i: usize) -> bool {
    i > 0 && (src[i - 1].is_ascii_alphanumeric() || src[i - 1] == b'_')
}

/// `br"…"` — the `r` follows a lone `b`.
fn prev_is_byte_prefix(src: &[u8], i: usize) -> bool {
    i > 0 && src[i - 1] == b'b' && !prev_is_ident(src, i - 1)
}

/// If `'` at `i` opens a char literal (`'x'`, `'\n'`, `'\u{1F600}'`, multi-byte UTF-8),
/// return the index just past its closing quote. Lifetimes (`'a`) return `None`.
fn char_literal_end(src: &[u8], i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    if rest.first() == Some(&b'\\') {
        let close = rest.iter().take(12).skip(2).position(|&b| b == b'\'')?;
        return Some(i + 1 + 2 + close + 1);
    }
    // One (possibly multi-byte) character then a quote.
    let first = *rest.first()?;
    let width = match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    };
    (rest.get(width) == Some(&b'\'') && first != b'\n').then_some(i + 1 + width + 1)
}

/// Σ logical indent of code lines. Indent unit is inferred per file (2, 3, 4 or 8
/// spaces — the largest that divides ≥ 80 % of indents); a tab is one unit.
fn indentation_complexity(indents: &[(u32, u32)]) -> u64 {
    let positive: Vec<u32> = indents.iter().map(|&(_, s)| s).filter(|&s| s > 0).collect();
    let unit = if positive.is_empty() {
        4
    } else {
        [8u32, 4, 3, 2]
            .into_iter()
            .find(|&u| positive.iter().filter(|&&s| s % u == 0).count() * 10 >= positive.len() * 8)
            .unwrap_or(1)
    };
    indents
        .iter()
        .map(|&(tabs, spaces)| u64::from(tabs + spaces / unit))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{detect_lang, lang};

    fn run(file: &str, src: &str) -> (String, LineStats) {
        let syn = lang(detect_lang(file).unwrap()).syntax;
        let (out, stats) = strip(src.as_bytes(), &syn);
        (String::from_utf8(out).unwrap(), stats)
    }

    #[test]
    fn c_like_comments_and_strings() {
        let src = "int a; // trailing\n/* block\n still */ int b = \"// not\";\n\n";
        let (out, s) = run("x.c", src);
        assert!(!out.contains("trailing"));
        assert!(!out.contains("still"));
        assert!(out.contains("\"// not\""));
        assert_eq!(s.lines, 4);
        assert_eq!(s.code, 2);
        assert_eq!(s.comments, 1);
        assert_eq!(s.blank, 1);
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn rust_lifetimes_raw_strings_nested_comments() {
        let src = "fn f<'a>(x: &'a str) -> char { 'x' }\n/* outer /* inner */ still */\nlet s = r#\"// keep \"# ;\nlet c = '\\'';\n";
        let (out, s) = run("x.rs", src);
        assert!(out.contains("fn f<'a>(x: &'a str)"));
        assert!(!out.contains("still"));
        assert!(out.contains("r#\"// keep \"#"));
        assert_eq!(s.code, 3);
        assert_eq!(s.comments, 1);
    }

    #[test]
    fn python_docstrings_are_comments() {
        let src = "\"\"\"Doc.\n\n>>> import os\n\"\"\"\nimport sys  # why\nx = '#not'\n";
        let (out, s) = run("m.py", src);
        assert!(!out.contains("import os"));
        assert!(out.contains("import sys"));
        assert!(out.contains("'#not'"));
        assert_eq!(s.code, 2);
        assert_eq!(s.comments, 3);
        assert_eq!(s.blank, 1);
    }

    #[test]
    fn shell_hash_needs_word_start() {
        let (out, s) = run("x.sh", "echo ${#arr} $# # real\n");
        assert!(out.contains("${#arr} $#"));
        assert!(!out.contains("real"));
        assert_eq!(s.code, 1);
    }

    #[test]
    fn js_templates_and_urls() {
        let src = "const u = `http://x/${a}`; // c\nconst v = 'http://y';\n";
        let (out, _) = run("a.ts", src);
        assert!(out.contains("`http://x/${a}`"));
        assert!(out.contains("'http://y'"));
        assert!(!out.contains("// c"));
    }

    #[test]
    fn complexity_infers_indent_unit() {
        let two = "a\n  b\n    c\n";
        let four = "a\n    b\n        c\n";
        let (_, s2) = run("x.js", two);
        let (_, s4) = run("x.js", four);
        assert_eq!(s2.complexity, 3);
        assert_eq!(s4.complexity, 3);
    }

    #[test]
    fn plain() {
        assert_eq!(plain_stats(b"a\n\nb\n").lines, 3);
        assert_eq!(plain_stats(b"a\n\nb\n").blank, 1);
        assert_eq!(plain_stats(b"a").lines, 1);
        assert_eq!(plain_stats(b"").lines, 0);
    }
}
