//! Recognise shell commands that publish work (`git push`, `gh pr create|merge`), for the
//! pre-push gate. A guardrail, not a shell parser: it follows quotes, command separators,
//! common wrappers (`env`, `sudo`, `timeout`, …) and `sh -c`/`eval` scripts, and reads
//! git's and gh's global options, which covers how agents actually push. Aliases, functions
//! and scripts that push are not seen (review finding M6 of the final review).

/// True when `cmd` would run `git push`, `gh pr create` or `gh pr merge`.
pub fn publishes(cmd: &str) -> bool {
    publishes_at(cmd, 0)
}

fn publishes_at(cmd: &str, depth: usize) -> bool {
    depth <= 3
        && commands(cmd)
            .iter()
            .any(|words| simple_publishes(words, depth))
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

/// Programs that run the rest of the line as a command; their own options are skipped.
const WRAPPERS: &[&str] = &[
    "env", "command", "exec", "nohup", "time", "sudo", "nice", "timeout", "xargs", "stdbuf",
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

fn simple_publishes(words: &[String], depth: usize) -> bool {
    let mut i = 0;
    // Leading assignments and wrappers (with their options) before the real program.
    loop {
        while words.get(i).is_some_and(|w| is_assignment(w)) {
            i += 1;
        }
        let Some(w) = words.get(i) else {
            return false;
        };
        let p = program(w);
        if !WRAPPERS.contains(&p) {
            break;
        }
        i += 1;
        while words
            .get(i)
            .is_some_and(|w| w.starts_with('-') || is_assignment(w))
        {
            i += 1;
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
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            let script = rest
                .iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
                .and_then(|at| rest.get(at + 1));
            script.is_some_and(|s| publishes_at(s, depth + 1))
        }
        "eval" => publishes_at(&rest.join(" "), depth + 1),
        "git" => subcommand(rest, GIT_VALUE_OPTIONS).first() == Some(&"push"),
        "gh" => {
            let args = subcommand(rest, GH_VALUE_OPTIONS);
            args.first() == Some(&"pr") && matches!(args.get(1), Some(&"create") | Some(&"merge"))
        }
        _ => false,
    }
}

/// The non-option words of `args`, skipping options and the values of `value_options`.
fn subcommand<'a>(args: &'a [String], value_options: &[&str]) -> Vec<&'a str> {
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
        ] {
            assert!(!publishes(cmd), "{cmd}");
        }
    }
}
