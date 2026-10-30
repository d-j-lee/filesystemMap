//! Recognise shell commands that publish local work (`git push`, `gh pr create`), for the
//! pre-push gate. A guardrail, not a shell parser. It follows quotes, command separators,
//! command substitutions (also inside double quotes), reserved words (`if … then`,
//! `{ … }`, `!`), common wrappers with their options (`env`, `sudo -u`, `timeout -k`, …),
//! `sh -c`/`eval`/`env -S` scripts, here-strings, git's and gh's global options,
//! `git -c alias.x=push x` and `git subtree push`. A shell that reads its script from stdin
//! (`… | sh`, a heredoc) is judged by whether the command line mentions a push at all:
//! gating a tree that already passed is cheap. Heredoc bodies fed to anything but a shell
//! are data. Not publishing: `--dry-run`, `--help`, pushes that only delete remote refs,
//! and `gh pr merge` (it merges what was already pushed and checked). Git aliases from the
//! configuration, functions and script files that push are not seen.

/// True when `cmd` would run `git push` or `gh pr create`.
pub fn publishes(cmd: &str) -> bool {
    let cmd = strip_data_heredocs(cmd);
    publishes_at(&cmd, &cmd, 0)
}

/// How deep `sh -c` scripts and command substitutions are followed. Past it, and when the
/// text does not parse to its end (an open quote or substitution), mentioning a push is
/// enough: the parser is unsure, and gating a tree that already passed is cheap.
const MAX_DEPTH: usize = 16;

fn publishes_at(cmd: &str, whole: &str, depth: usize) -> bool {
    if depth > 3 {
        return mentions_publish(cmd);
    }
    let parsed = parse(cmd, 0);
    parsed
        .commands
        .iter()
        .any(|words| simple_publishes(words, whole, depth))
        || (!parsed.complete && mentions_publish(cmd))
}

/// Whether `cmd` (already judged publishing by `publishes`) mentions `gh pr create`: a pull
/// request has no git hook, so only the command can be judged (D-16).
pub fn opens_pr(cmd: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bgh\b[^\n]*\bpr\b[^\n]*\bcreate\b").expect("a valid pattern")
    });
    RE.is_match(cmd)
}

/// Why `cmd` would get a push past the git pre-push hook, if it would (D-16): `--no-verify`
/// or an abbreviation of it, a `core.hooksPath` set by `-c`, `--config-env`, a `GIT_CONFIG_*`
/// variable or `git config`, or a `CLAUDECODE` the shim no longer recognises (unset,
/// changed, or the environment cleared). An early warning before the push; the audit at the
/// end of the turn is what catches the forms this misses.
pub fn hook_bypass(cmd: &str) -> Option<&'static str> {
    let lower = cmd.to_ascii_lowercase();
    if ![
        "hookspath",
        "claudecode",
        "--no-veri",
        "env -i",
        "env -",
        "--ignore-environment",
    ]
    .iter()
    .any(|t| lower.contains(t))
    {
        return None;
    }
    bypass_at(&strip_data_heredocs(cmd), 0)
}

const NO_VERIFY: &str = "it skips the harness's git pre-push check (--no-verify)";
const HOOKS_PATH: &str =
    "it changes core.hooksPath, which turns off the harness's git pre-push check";
const CLAUDECODE: &str =
    "it unsets or changes CLAUDECODE, which the git pre-push check uses to recognise Claude Code";

fn bypass_at(cmd: &str, depth: usize) -> Option<&'static str> {
    if depth > 3 {
        return None;
    }
    parse(cmd, 0)
        .commands
        .iter()
        .find_map(|words| bypass_in(words, depth))
}

fn bypass_in(words: &[String], depth: usize) -> Option<&'static str> {
    let i = match program_index(words) {
        Program::At(i) => i,
        Program::SplitString(at) => return words.get(at).and_then(|s| bypass_at(s, depth + 1)),
        Program::None => words.len(),
    };
    let before = &words[..i];
    for (k, w) in before.iter().enumerate() {
        if let Some((name, value)) = w.split_once('=').filter(|_| is_assignment(w)) {
            if name == "CLAUDECODE" && value != "1" {
                return Some(CLAUDECODE);
            }
            if name.starts_with("GIT_CONFIG") && value.to_ascii_lowercase().contains("hookspath") {
                return Some(HOOKS_PATH);
            }
        }
        let unset_next = matches!(w.as_str(), "-u" | "--unset")
            && before.get(k + 1).is_some_and(|n| n == "CLAUDECODE");
        if unset_next
            || w == "-uCLAUDECODE"
            || w == "--unset=CLAUDECODE"
            || (matches!(w.as_str(), "-i" | "-" | "--ignore-environment")
                && before[..k].iter().any(|p| program(p) == "env"))
        {
            return Some(CLAUDECODE);
        }
    }
    let prog = words.get(i)?;
    let rest = &words[i + 1..];
    match program(prog) {
        "unset" if rest.iter().any(|w| w == "CLAUDECODE") => Some(CLAUDECODE),
        "export"
            if rest
                .iter()
                .any(|w| w.strip_prefix("CLAUDECODE=").is_some_and(|v| v != "1"))
                || (rest.iter().any(|w| w == "-n") && rest.iter().any(|w| w == "CLAUDECODE")) =>
        {
            Some(CLAUDECODE)
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            let at = rest
                .iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))?;
            let script = match rest.get(at + 1).map(String::as_str) {
                Some("--") => rest.get(at + 2),
                _ => rest.get(at + 1),
            }?;
            bypass_at(script, depth + 1)
        }
        "eval" => bypass_at(&rest.join(" "), depth + 1),
        "git" => git_bypass(rest),
        _ => None,
    }
}

fn git_bypass(rest: &[String]) -> Option<&'static str> {
    let sets_hooks_path = |kv: &str| {
        kv.split_once('=')
            .is_some_and(|(k, _)| k.eq_ignore_ascii_case("core.hooksPath"))
    };
    let mut i = 0;
    while let Some(a) = rest.get(i) {
        if a == "-c" || a == "--config-env" {
            if rest.get(i + 1).is_some_and(|kv| sets_hooks_path(kv)) {
                return Some(HOOKS_PATH);
            }
            i += 2;
        } else if let Some(kv) = a.strip_prefix("--config-env=") {
            if sets_hooks_path(kv) {
                return Some(HOOKS_PATH);
            }
            i += 1;
        } else if GIT_VALUE_OPTIONS.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let sub = rest.get(i)?;
    let args = &rest[i + 1..];
    match sub.as_str() {
        "config" => {
            let reading = args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "--get" | "--get-all" | "--get-regexp" | "-l" | "--list" | "get" | "list"
                )
            });
            let names_it = args
                .iter()
                .any(|a| a.eq_ignore_ascii_case("core.hooksPath"));
            (names_it && !reading).then_some(HOOKS_PATH)
        }
        // git accepts any unambiguous abbreviation; `--no-ve…` shorter than this is ambiguous
        // with `--no-verbose`.
        "push" => args
            .iter()
            .any(|a| a.len() >= "--no-veri".len() && "--no-verify".starts_with(a.as_str()))
            .then_some(NO_VERIFY),
        _ => None,
    }
}

/// Whether the text mentions a publishing command anywhere.
pub fn mentions_publish(text: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bgit\b[^\n]*\bpush\b|\bgh\b[^\n]*\bpr\b[^\n]*\bcreate\b")
            .expect("a valid pattern")
    });
    RE.is_match(text)
}

/// Remove the bodies of heredocs whose command is not a shell (`cat > notes.md <<'X'`,
/// `git commit -m "$(cat <<'EOF' … EOF)"`): they are data. A heredoc a shell runs is a
/// script and stays: fed to a shell, piped into one (`cat <<X | sh`), or inside a
/// substitution whose output a shell or `eval` runs (`bash -c "$(cat <<X …)"`). Operators
/// are found with the quoting context: `echo "<<X"` is text, `"$(cat <<X …)"` is a heredoc.
/// A body that never ends means the reading was wrong (or the script is), so nothing is
/// removed.
fn strip_data_heredocs(cmd: &str) -> String {
    let mut out = String::new();
    let mut pending: Vec<(String, bool)> = Vec::new(); // (delimiter, keep body)
    let mut stack: Vec<Ctx> = Vec::new();
    for line in cmd.split_inclusive('\n') {
        if let Some((delim, keep)) = pending.first().cloned() {
            if line.trim() == delim {
                pending.remove(0);
            }
            if keep {
                out.push_str(line);
            }
            continue;
        }
        out.push_str(line);
        for op in heredoc_operators(line, &mut stack) {
            let before = parse(&line[..op.at], 0);
            let own = before.commands.len().saturating_sub(1);
            // Fed to a shell (`timeout 60 bash <<X`), inside a substitution or process
            // substitution that a shell or `eval` around it runs, or piped into a shell.
            let script = before
                .commands
                .last()
                .is_some_and(|w| runs_script(w, false))
                || (op.in_substitution
                    && before.commands[..own].iter().any(|w| runs_script(w, true)))
                || piped_into_shell(&line[op.at..]);
            pending.push((op.delimiter, script));
        }
    }
    if pending.is_empty() {
        out
    } else {
        cmd.to_string()
    }
}

/// Whether a simple command runs a shell script (`bash`, `sudo -u x sh`, `timeout 60
/// bash`, `env -S …`), or, with `eval`, evaluates its arguments.
fn runs_script(words: &[String], eval: bool) -> bool {
    match program_index(words) {
        Program::At(i) => {
            let p = program(&words[i]);
            matches!(p, "sh" | "bash" | "zsh" | "dash" | "ksh") || (eval && p == "eval")
        }
        Program::SplitString(_) => true,
        Program::None => false,
    }
}

/// Whether the rest of a line, from a heredoc operator, pipes into a shell before the
/// command ends (`<<X | timeout 60 bash`; not `<<X > f && bash build.sh`).
fn piped_into_shell(rest: &str) -> bool {
    let end = [";", "&&", "||", "\n"]
        .iter()
        .filter_map(|sep| rest.find(sep))
        .min()
        .unwrap_or(rest.len());
    rest[..end].split('|').skip(1).any(|segment| {
        parse(segment, 0)
            .commands
            .first()
            .is_some_and(|w| runs_script(w, false))
    })
}

/// A heredoc operator found in a line.
struct HeredocOp {
    /// Where the operator starts.
    at: usize,
    /// The delimiter word with its quotes removed.
    delimiter: String,
    /// Inside a command substitution or backticks.
    in_substitution: bool,
}

/// Quoting context while scanning for heredoc operators.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Single,
    /// `$'…'`, where a backslash escapes the next character.
    Ansi,
    Double,
    /// `$( … )`, or a parenthesis inside one.
    Subst,
    Backtick,
    /// `$(( … ))`, where `<<` is a shift.
    Arith,
}

/// The heredoc operators (`<<X`, `<<-'X'`, `<<\X`, not `<<<`) in `line` that the shell
/// would see as operators. `stack` carries the quoting context across lines.
fn heredoc_operators(line: &str, stack: &mut Vec<Ctx>) -> Vec<HeredocOp> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match stack.last().copied() {
            Some(Ctx::Single) => {
                if c == b'\'' {
                    stack.pop();
                }
            }
            Some(Ctx::Ansi) => match c {
                b'\\' => i += 1,
                b'\'' => {
                    stack.pop();
                }
                _ => {}
            },
            Some(Ctx::Double) => match c {
                b'\\' => i += 1,
                b'"' => {
                    stack.pop();
                }
                b'$' if next == Some(b'(') && b.get(i + 2) == Some(&b'(') => {
                    stack.push(Ctx::Arith);
                    i += 2;
                }
                b'$' if next == Some(b'(') => {
                    stack.push(Ctx::Subst);
                    i += 1;
                }
                b'`' => stack.push(Ctx::Backtick),
                _ => {}
            },
            Some(Ctx::Arith) => {
                if c == b')' && next == Some(b')') {
                    stack.pop();
                    i += 1;
                }
            }
            top @ (None | Some(Ctx::Subst) | Some(Ctx::Backtick)) => match c {
                b'\\' => i += 1,
                b'\'' => stack.push(Ctx::Single),
                b'"' => stack.push(Ctx::Double),
                b'$' if next == Some(b'\'') => {
                    stack.push(Ctx::Ansi);
                    i += 1;
                }
                b'$' if next == Some(b'(') && b.get(i + 2) == Some(&b'(') => {
                    stack.push(Ctx::Arith);
                    i += 2;
                }
                b'$' if next == Some(b'(') => {
                    stack.push(Ctx::Subst);
                    i += 1;
                }
                // Process substitution: a command whose output another one reads.
                b'<' | b'>' if next == Some(b'(') => {
                    stack.push(Ctx::Subst);
                    i += 1;
                }
                // `(( … ))` as a command is arithmetic too.
                b'(' if next == Some(b'(')
                    && (i == 0 || matches!(b[i - 1], b' ' | b'\t' | b';' | b'&' | b'|' | b'(')) =>
                {
                    stack.push(Ctx::Arith);
                    i += 1;
                }
                b'(' if top == Some(Ctx::Subst) => stack.push(Ctx::Subst),
                b')' if top == Some(Ctx::Subst) => {
                    stack.pop();
                }
                b'`' if top == Some(Ctx::Backtick) => {
                    stack.pop();
                }
                b'`' => stack.push(Ctx::Backtick),
                b'#' if i == 0 || matches!(b[i - 1], b' ' | b'\t' | b';' | b'&' | b'|' | b'(') => {
                    break;
                }
                b'<' if next == Some(b'<') => {
                    let start = i;
                    let mut j = i + 2;
                    if b.get(j) == Some(&b'-') {
                        j += 1;
                    }
                    while matches!(b.get(j), Some(b' ' | b'\t')) {
                        j += 1;
                    }
                    // A number is a shift (`x<<2`), not a delimiter; `<<<` is a here-string.
                    let word = b
                        .get(j)
                        .is_some_and(|c| !c.is_ascii_digit() && !b" \t\r\n;&|<>()".contains(c));
                    // The delimiter word runs to the next unquoted metacharacter; its quotes
                    // and backslashes are removed (`'END-MSG'`, `\EOF`, `E"O"F`).
                    let mut delimiter = Vec::new();
                    let mut quote: Option<u8> = None;
                    while word && let Some(&d) = b.get(j) {
                        match quote {
                            Some(q) if d == q => quote = None,
                            Some(_) => delimiter.push(d),
                            None => match d {
                                b'\'' | b'"' => quote = Some(d),
                                b'\\' => {
                                    j += 1;
                                    if let Some(&n) = b.get(j) {
                                        delimiter.push(n);
                                    }
                                }
                                b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>'
                                | b'(' | b')' => break,
                                _ => delimiter.push(d),
                            },
                        }
                        j += 1;
                    }
                    if !delimiter.is_empty() {
                        out.push(HeredocOp {
                            at: start,
                            delimiter: String::from_utf8_lossy(&delimiter).into_owned(),
                            in_substitution: stack
                                .iter()
                                .any(|c| matches!(c, Ctx::Subst | Ctx::Backtick)),
                        });
                    }
                    i = j.max(i + 2);
                    continue;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out
}

/// Simple commands (lists of words) and whether the text parsed to its end.
struct Parsed {
    /// Top-level commands first, then those inside double-quoted substitutions.
    commands: Vec<Vec<String>>,
    /// How many of `commands` are top-level.
    outer: usize,
    /// No quote or substitution was left open, and nesting stayed within `MAX_DEPTH`.
    complete: bool,
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Split a command line into simple commands. Quotes (`'…'`, `"…"`, `$'…'`), backslashes
/// and comments are honoured; `;`, `&`, `|`, newlines, parentheses and backticks end a
/// command; command substitutions inside double quotes are parsed as commands of their own.
fn parse(s: &str, depth: usize) -> Parsed {
    let mut p = Parsed {
        commands: Vec::new(),
        outer: 0,
        complete: depth <= MAX_DEPTH,
    };
    if !p.complete {
        return p;
    }
    let mut nested: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        let before = prev.replace(c);
        match c {
            '\'' => {
                in_word = true;
                p.complete &= read_until(&mut chars, &mut word, '\'', false);
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                in_word = true;
                p.complete &= read_until(&mut chars, &mut word, '\'', true);
            }
            '"' => {
                in_word = true;
                let mut closed = false;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => {
                            if let Some(n) = chars.next() {
                                if !matches!(n, '"' | '\\' | '$' | '`') {
                                    word.push('\\');
                                }
                                word.push(n);
                            }
                        }
                        // Command substitutions run inside double quotes too.
                        '$' if chars.peek() == Some(&'(') => {
                            chars.next();
                            let (inner, done) = substitution(&mut chars, ')');
                            let sub = parse(&inner, depth + 1);
                            p.complete &= done && sub.complete;
                            word.push_str(&inner);
                            nested.extend(sub.commands);
                        }
                        '`' => {
                            let (inner, done) = substitution(&mut chars, '`');
                            let sub = parse(&inner, depth + 1);
                            p.complete &= done && sub.complete;
                            word.push_str(&inner);
                            nested.extend(sub.commands);
                        }
                        _ => word.push(q),
                    }
                }
                p.complete &= closed;
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    word.push(n);
                }
            }
            // A comment starts a word: `$(date)#` and `a#b` are not comments.
            '#' if before
                .is_none_or(|p| p.is_whitespace() || matches!(p, ';' | '&' | '|' | '(')) =>
            {
                while chars.next_if(|&c| c != '\n').is_some() {}
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    p.commands.push(std::mem::take(&mut words));
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    p.commands.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut word, &mut in_word, &mut words),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut words);
    if !words.is_empty() {
        p.commands.push(words);
    }
    p.outer = p.commands.len();
    p.commands.extend(nested);
    p
}

/// Append the text up to the closing `close` to `word`; `escapes`: a backslash escapes the
/// next character (`$'…'`). Returns whether the closing character was found.
fn read_until(chars: &mut Chars<'_>, word: &mut String, close: char, escapes: bool) -> bool {
    while let Some(c) = chars.next() {
        if c == close {
            return true;
        }
        if escapes && c == '\\' {
            if let Some(n) = chars.next() {
                word.push(n);
            }
            continue;
        }
        word.push(c);
    }
    false
}

/// The text of a command substitution up to its closing `close` (`)` with nesting, or a
/// backtick), consumed from `chars`, and whether it was closed. Quotes inside are kept as
/// they are; in single quotes a backslash is an ordinary character.
fn substitution(chars: &mut Chars<'_>, close: char) -> (String, bool) {
    let mut inner = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None; // '\'', '"', or 'a' for `$'…'`
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') | (Some('a'), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                inner.push(c);
                if let Some(n) = chars.next() {
                    inner.push(n);
                }
                continue;
            }
            (Some(_), _) => {}
            (None, '$') if close == ')' && chars.peek() == Some(&'\'') => {
                inner.push(c);
                if let Some(n) = chars.next() {
                    inner.push(n);
                }
                quote = Some('a');
                continue;
            }
            (None, '\'' | '"') if close == ')' => quote = Some(c),
            (None, '(') if close == ')' => depth += 1,
            (None, c) if c == close => {
                if depth == 0 {
                    return (inner, true);
                }
                depth -= 1;
            }
            _ => {}
        }
        inner.push(c);
    }
    (inner, false)
}

/// Shell words that can precede a command without being it.
const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "case", "esac", "{", "}",
    "!", "[[", "]]",
];
/// Programs that run the rest of the line as a command, with their options that take a
/// separate value.
const WRAPPERS: &[(&str, &[&str])] = &[
    ("env", &["-u", "--unset", "-C", "--chdir"]),
    ("command", &[]),
    ("exec", &["-a"]),
    ("nohup", &[]),
    ("time", &["-f", "-o"]),
    (
        "sudo",
        &["-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U"],
    ),
    ("nice", &["-n", "--adjustment"]),
    ("timeout", &["-k", "-s", "--kill-after", "--signal"]),
    ("xargs", &["-I", "-n", "-P", "-L", "-s", "-d", "-E", "-a"]),
    ("stdbuf", &["-i", "-o", "-e"]),
];
/// git options that take their value as the next word.
const GIT_VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
];
/// gh options that take their value as the next word.
const GH_VALUE_OPTIONS: &[&str] = &["-R", "--repo", "--hostname"];

fn program(word: &str) -> &str {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    base.strip_suffix(".exe").unwrap_or(base)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

/// Where the program of a simple command is, past reserved words, assignments and
/// wrappers with their options.
enum Program {
    At(usize),
    /// `env -S <line>`: the word at this index is run as a command line.
    SplitString(usize),
    None,
}

fn program_index(words: &[String]) -> Program {
    let mut i = 0;
    loop {
        while words
            .get(i)
            .is_some_and(|w| is_assignment(w) || RESERVED.contains(&w.as_str()))
        {
            i += 1;
        }
        let Some(w) = words.get(i) else {
            return Program::None;
        };
        let p = program(w);
        let Some((_, value_options)) = WRAPPERS.iter().find(|(name, _)| *name == p) else {
            return Program::At(i);
        };
        i += 1;
        while let Some(w) = words.get(i) {
            if p == "env" && (w == "-S" || w == "--split-string") {
                return Program::SplitString(i + 1);
            }
            if value_options.contains(&w.as_str()) {
                i += 2;
            } else if w.starts_with('-') || is_assignment(w) {
                i += 1;
            } else {
                break;
            }
        }
        if p == "timeout"
            && words
                .get(i)
                .is_some_and(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        {
            i += 1;
        }
    }
}

fn simple_publishes(words: &[String], whole: &str, depth: usize) -> bool {
    let i = match program_index(words) {
        Program::At(i) => i,
        // `env -S 'git push'` runs its value as a command line.
        Program::SplitString(at) => {
            return words
                .get(at)
                .is_some_and(|s| publishes_at(s, whole, depth + 1));
        }
        Program::None => return false,
    };
    let rest = &words[i + 1..];
    match program(&words[i]) {
        "sh" | "bash" | "zsh" | "dash" | "ksh" => shell_publishes(rest, whole, depth),
        "eval" => publishes_at(&rest.join(" "), whole, depth + 1),
        "git" => git_publishes(rest, whole, depth),
        "gh" => {
            let args = non_options(rest, GH_VALUE_OPTIONS);
            args.first() == Some(&"pr") && args.get(1) == Some(&"create") && !help(rest)
        }
        _ => false,
    }
}

/// A shell's arguments: a `-c` script, a here-string, or no script operand at all (the
/// script comes from stdin, which only the whole command line can tell about).
fn shell_publishes(rest: &[String], whole: &str, depth: usize) -> bool {
    if let Some(at) = rest
        .iter()
        .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
    {
        let script = match rest.get(at + 1).map(String::as_str) {
            Some("--") => rest.get(at + 2),
            _ => rest.get(at + 1),
        };
        return script.is_some_and(|s| publishes_at(s, whole, depth + 1));
    }
    if let Some(at) = rest.iter().position(|w| w.starts_with("<<<")) {
        let inline = &rest[at][3..];
        let script = if inline.is_empty() {
            rest.get(at + 1).map_or("", String::as_str)
        } else {
            inline
        };
        return publishes_at(script, whole, depth + 1);
    }
    let operands = rest
        .iter()
        .filter(|w| !w.starts_with(['-', '<', '>']))
        .count();
    operands == 0 && mentions_publish(whole)
}

fn git_publishes(rest: &[String], whole: &str, depth: usize) -> bool {
    let mut aliases: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while let Some(a) = rest.get(i) {
        if a == "-c" {
            if let Some((k, v)) = rest.get(i + 1).and_then(|kv| kv.split_once('='))
                && let Some(name) = k.strip_prefix("alias.")
            {
                aliases.push((name.to_string(), v.to_string()));
            }
            i += 2;
        } else if GIT_VALUE_OPTIONS.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let Some(sub) = rest.get(i) else {
        return false;
    };
    let args = &rest[i + 1..];
    if let Some((_, value)) = aliases.iter().find(|(name, _)| name == sub) {
        return match value.strip_prefix('!') {
            Some(shell) => publishes_at(shell, whole, depth + 1),
            None => {
                let mut words: Vec<String> = value.split_whitespace().map(String::from).collect();
                words.first().is_some_and(|w| w == "push") && {
                    words.extend(args.iter().cloned());
                    push_publishes(&words[1..])
                }
            }
        };
    }
    match sub.as_str() {
        "push" => push_publishes(args),
        "subtree" => non_options(args, &[]).first() == Some(&"push") && !help(args),
        _ => false,
    }
}

/// `git push` options that take their value as the next word.
const PUSH_VALUE_OPTIONS: &[&str] = &["-o", "--push-option", "--repo", "--receive-pack", "--exec"];

/// Whether `git push <args>` sends commits: not with `--dry-run` or `--help`, and not when
/// it only deletes remote refs (`--delete`, `:branch`; third review, finding 3).
fn push_publishes(args: &[String]) -> bool {
    if help(args) || args.iter().any(|a| a == "--dry-run" || a == "-n") {
        return false;
    }
    let mut skip = false;
    for a in args {
        if std::mem::take(&mut skip) {
            continue;
        }
        if PUSH_VALUE_OPTIONS.contains(&a.as_str()) {
            skip = true;
        } else if a == "--delete" || a == "-d" {
            return false;
        }
    }
    let operands = non_options(args, PUSH_VALUE_OPTIONS);
    let refspecs = operands.get(1..).unwrap_or_default();
    refspecs.is_empty() || !refspecs.iter().all(|r| r.starts_with(':') && r.len() > 1)
}

fn help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// The non-option words of `args`, skipping options and the values of `value_options`.
fn non_options<'a>(args: &'a [String], value_options: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if value_options.contains(&a.as_str()) {
            skip = true;
        } else if !a.starts_with('-') {
            out.push(a.as_str());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_commands_are_found() {
        for cmd in [
            "git push origin main",
            "cargo test && git push -u origin x",
            "/usr/bin/git -C . push",
            "git -c core.hooksPath=/dev/null push",
            "git --git-dir=.git push",
            "GIT_TRACE=1 git push",
            "env -i git push",
            "timeout 60 git push",
            "sh -c 'git push origin main'",
            "bash -lc \"cd x && git push\"",
            "eval git push",
            "echo $(git push)",
            "gh pr create --draft",
            "gh -R o/r pr create",
            "gh pr -R o/r create --fill",
            // The second implementation review's misses.
            "if cargo test; then git push origin main; fi",
            "{ git push origin main; }",
            "for b in a; do git push origin $b; done",
            "! git push",
            "nice -n 10 git push",
            "sudo -u root git push",
            "timeout -k 5 60 git push",
            "env -u FOO git push",
            "env -S 'git push origin main'",
            "echo 'git push' | sh",
            "bash <<< 'git push'",
            "bash -c -- 'git push'",
            "git -c alias.p=push p",
            "git -c 'alias.p=!git push' p",
            "git subtree push --prefix x origin main",
            "bash <<'X'\ngit push origin main\nX",
            // The third review's misses.
            "echo \"$(git push origin main)\"",
            "echo \"result: `git push`\"",
            "x=\"$(cd sub && git push)\"",
            "echo \"<<X\"\ngit push origin main",
            "echo '<<X'\ngit push origin main",
            "git push origin main :old-branch",
            "git push -o ci.skip origin main",
            "git -c alias.p=push p origin main",
            "echo $((1<<2))\ngit push origin main",
            // The fourth review: Claude Code's own commit idiom with an apostrophe in the
            // message, comments, and quoting the parser got wrong.
            "git commit -m \"$(cat <<'EOF'\nFix the parser\n\nIt's quote-aware now.\nEOF\n)\" && git push origin main",
            "gh pr create --title x --body \"$(cat <<'EOF'\nIt's done\nEOF\n)\"",
            "# Push so it's on the remote\ngit push origin main",
            "cargo test # make sure it's green\ngit push",
            "echo \"$(echo 'a\\')\"; git push origin main",
            "x=\"$(echo \"$p\" | tr '\\' '/')\"; git push origin main",
            "git push -o --delete origin main",
            "echo $'it\\'s <<EOF'\ngit push origin main",
            "echo $((x<<y))\ngit push origin main",
            "echo 'never closed\ngit push origin main",
            // A `<<` that is not a heredoc must not swallow a push before a line that
            // looks like its delimiter; an unterminated heredoc keeps everything.
            "echo $((x<<y))\ngit push origin main\ny",
            "echo $'it\\'s <<EOF'\ngit push origin main\nEOF",
            "cat <<EOF\ngit push origin main",
            "true;# a comment <<EOF\ngit push origin main\nEOF",
            "echo \"<<X\"\ngit push origin main\nX",
            "(( a = 1<<2 ))\ngit push origin main\n2",
            // The fifth review: a heredoc whose output a shell runs is a script; `#` inside a
            // word; shifts in double quotes and in `(( ))`; `<<<` is not `<<`.
            "bash -c \"$(cat <<'EOF'\ngit push origin main\nEOF\n)\"",
            "eval \"$(cat <<EOF\ngit push origin main\nEOF\n)\"",
            "cat <<'EOF' | bash\ngit push origin main\nEOF",
            "echo $(date)#; git push origin main",
            "echo \"$((x<<y))\"\ngit push origin main\ny",
            "(( a = x<<y ))\ngit push origin main\ny",
            "cat <<<word\ngit push origin main\nword",
            "let x=1<<2\ngit push origin main\n2",
            "echo \"a <<X b\"\ngit push origin main\nX",
            "echo $'it\\'s <<EOF here'\ngit push origin main\nEOF",
            // The sixth review: shells behind wrappers, in pipes and process substitutions.
            "timeout 60 bash <<'EOF'\ngit push origin main\nEOF",
            "GIT_TERMINAL_PROMPT=0 bash <<'EOF'\ngit push origin main\nEOF",
            "env bash <<'EOF'\ngit push origin main\nEOF",
            "sudo bash <<'EOF'\ngit push origin main\nEOF",
            "cat <<'EOF' | timeout 60 bash\ngit push origin main\nEOF",
            "cat <<'EOF' | sudo -u dev bash\ngit push origin main\nEOF",
            "eval $(cat <<'EOF'\ngit push origin main\nEOF\n)",
            "bash <(cat <<'EOF'\ngit push origin main\nEOF\n)",
        ] {
            assert!(publishes(cmd), "{cmd}");
        }
    }

    /// Deep nesting must not overflow the stack (the hook would die and the wrapper would
    /// let the command through); past the limit, mentioning a push is enough.
    #[test]
    fn ways_around_the_git_hook_are_found() {
        for cmd in [
            "git push --no-verify origin main",
            "git push --no-veri origin main",
            "git push origin main --no-verif",
            "git -c core.hooksPath=/dev/null push",
            "git -c CORE.HOOKSPATH=x push",
            "git --config-env=core.hooksPath=H push",
            "git --config-env core.hooksPath=H push",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=x git push",
            "GIT_CONFIG_PARAMETERS=\"'core.hooksPath'='x'\" git push",
            "git config core.hooksPath x",
            "git config --global core.hookspath x",
            "CLAUDECODE= git push",
            "CLAUDECODE=0 git push",
            "env -u CLAUDECODE git push",
            "env --unset=CLAUDECODE git push",
            "env -i PATH=/usr/bin git push",
            "unset CLAUDECODE; git push",
            "export CLAUDECODE=; git push",
            "bash -c 'git push --no-verify'",
            "sh -c \"env -u CLAUDECODE git push\"",
        ] {
            assert!(hook_bypass(cmd).is_some(), "{cmd}");
        }
        for cmd in [
            "git push origin main",
            "git push --verbose origin main",
            "git config --get core.hooksPath",
            "git config --list",
            "git commit -m 'about --no-verify and core.hooksPath'",
            "CLAUDECODE=1 git push",
            "echo $CLAUDECODE",
            "env FOO=1 git push",
            "grep -r hooksPath docs/",
        ] {
            assert!(hook_bypass(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let cmd = format!("{}git push origin main", "echo \"$(".repeat(20_000));
        assert!(publishes(&cmd));
        let quiet = format!("{}true", "echo \"$(".repeat(20_000));
        assert!(!publishes(&quiet));
    }

    #[test]
    fn other_commands_are_not() {
        for cmd in [
            "git status; git log --oneline",
            "git stash push -m wip",
            "git log --grep push",
            "git commit -m \"fix push handling\"",
            "git commit -m 'git push later'",
            "echo push",
            "echo 'git push'",
            "gh pr view 1",
            "gh issue create",
            "cargo test -- push",
            "git push --dry-run origin main",
            "cat > CONTRIBUTING.md <<'X'\nRun tests, then:\ngit push origin main\nX",
            "sh scripts/check.sh",
            // The third review's false positives: nothing local is published.
            "git push --delete origin old-branch",
            "git push origin --delete a b",
            "git push -d origin old",
            "git push origin :old-branch",
            "git push --help",
            "git push -h",
            "gh pr merge 1 --squash",
            "gh --repo o/r pr merge 1",
            "echo \"$(git log -1)\"",
            "git -c alias.p=push p --delete origin x",
            "git commit -m \"$(cat <<'EOF'\nIt's ready for git push later\nEOF\n)\"",
            "echo 'git push' # it's only text",
            "true;# it's a comment <<EOF\ngit status",
            "git commit -F- <<'END-MSG'\nIt's done; next git push\nEND-MSG",
            "cat <<\\EOF\nIt's about git push\nEOF",
            "cat <<E\"O\"F\nIt's about git push\nEOF",
            "cat <<'EOF' > notes.md && bash build.sh\nremember to git push\nEOF",
            "cat <<'EOF' > notes.md || bash fallback.sh\nremember to git push\nEOF",
            "cat <<'EOF' > notes.md; echo ok | bash\nremember to git push\nEOF",
            "python3 - <<'EOF'\nprint('git push')\nEOF",
            "timeout 60 cat <<'EOF'\ngit push origin main\nEOF",
            "echo \"$(echo 'a\\')\" && echo 'remember: git push'",
        ] {
            assert!(!publishes(cmd), "{cmd}");
        }
    }
}
