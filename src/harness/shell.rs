//! Recognise shell commands that publish work (`git push`, `gh pr create|merge`), for the
//! pre-push gate. A guardrail, not a shell parser. It follows quotes, command separators,
//! reserved words (`if … then`, `{ … }`, `!`), common wrappers with their options (`env`,
//! `sudo -u`, `timeout -k`, …), `sh -c`/`eval`/`env -S` scripts, here-strings, git's and
//! gh's global options, `git -c alias.x=push x` and `git subtree push`. A shell that reads
//! its script from stdin (`… | sh`, a heredoc) is judged by whether the command line
//! mentions a push at all: gating a tree that already passed is cheap. Heredoc bodies fed
//! to anything but a shell are data, and `--dry-run` does not publish. Git aliases from the
//! configuration, functions and script files that push are not seen.

/// True when `cmd` would run `git push`, `gh pr create` or `gh pr merge`.
pub fn publishes(cmd: &str) -> bool {
    let cmd = strip_data_heredocs(cmd);
    publishes_at(&cmd, &cmd, 0)
}

fn publishes_at(cmd: &str, whole: &str, depth: usize) -> bool {
    depth <= 3
        && commands(cmd)
            .iter()
            .any(|words| simple_publishes(words, whole, depth))
}

/// Whether the text mentions a publishing command anywhere.
fn mentions_publish(text: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bgit\b[^\n]*\bpush\b|\bgh\b[^\n]*\bpr\b[^\n]*\b(create|merge)\b")
            .expect("a valid pattern")
    });
    RE.is_match(text)
}

/// Remove the bodies of heredocs whose command is not a shell (`cat > notes.md <<'X'`):
/// they are data. A heredoc fed to a shell is a script and stays.
fn strip_data_heredocs(cmd: &str) -> String {
    static OP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(^|[^<])<<-?[ \t]*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?"#)
            .expect("a valid pattern")
    });
    let mut out = String::new();
    let mut pending: Vec<(String, bool)> = Vec::new(); // (delimiter, keep body)
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
        for c in OP.captures_iter(line) {
            let before = &line[..c.get(0).map_or(0, |m| m.start())];
            let shell = commands(before).last().is_some_and(|w| {
                w.first()
                    .is_some_and(|p| matches!(program(p), "sh" | "bash" | "zsh" | "dash" | "ksh"))
            });
            pending.push((c[2].to_string(), shell));
        }
    }
    out
}

/// Split a command line into simple commands (lists of words). Quotes and backslashes are
/// honoured; `;`, `&`, `|`, newlines, parentheses and backticks end a command.
fn commands(s: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
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
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                if !matches!(n, '"' | '\\' | '$' | '`') {
                                    word.push('\\');
                                }
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    word.push(n);
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
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
        out.push(words);
    }
    out
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

fn simple_publishes(words: &[String], whole: &str, depth: usize) -> bool {
    let mut i = 0;
    // Reserved words, assignments and wrappers (with their options) before the program.
    loop {
        while words
            .get(i)
            .is_some_and(|w| is_assignment(w) || RESERVED.contains(&w.as_str()))
        {
            i += 1;
        }
        let Some(w) = words.get(i) else {
            return false;
        };
        let p = program(w);
        let Some((_, value_options)) = WRAPPERS.iter().find(|(name, _)| *name == p) else {
            break;
        };
        i += 1;
        while let Some(w) = words.get(i) {
            if p == "env" && (w == "-S" || w == "--split-string") {
                // `env -S 'git push'` runs its value as a command line.
                return words
                    .get(i + 1)
                    .is_some_and(|s| publishes_at(s, whole, depth + 1));
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
    let rest = &words[i + 1..];
    match program(&words[i]) {
        "sh" | "bash" | "zsh" | "dash" | "ksh" => shell_publishes(rest, whole, depth),
        "eval" => publishes_at(&rest.join(" "), whole, depth + 1),
        "git" => git_publishes(rest, whole, depth),
        "gh" => {
            let args = non_options(rest, GH_VALUE_OPTIONS);
            args.first() == Some(&"pr") && matches!(args.get(1), Some(&"create") | Some(&"merge"))
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
            None => value.split_whitespace().next() == Some("push") && !dry_run(args),
        };
    }
    match sub.as_str() {
        "push" => !dry_run(args),
        "subtree" => non_options(args, &[]).first() == Some(&"push"),
        _ => false,
    }
}

fn dry_run(args: &[String]) -> bool {
    args.iter().any(|a| a == "--dry-run" || a == "-n")
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
            "gh --repo o/r pr merge 1",
            "gh pr -R o/r merge 1",
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
        ] {
            assert!(publishes(cmd), "{cmd}");
        }
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
        ] {
            assert!(!publishes(cmd), "{cmd}");
        }
    }
}
