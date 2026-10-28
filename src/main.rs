//! fsmap command-line interface. All logic lives in the library; this file parses flags,
//! shows progress, and maps outcomes to exit codes (0 ok, 1 findings at/above
//! `--fail-on`, 2 error).

use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobSetBuilder};

use fsmap::analysis::{self, Analysis, AuthorMode, Progress};
use fsmap::brief::{self, CodeOwners, Index};
use fsmap::git::history::Window;
use fsmap::insights::{self, Finding, Severity};
use fsmap::report::{context, html, sarif, terminal};
use fsmap::snapshot::{Parts, PruneInfo, Snapshot};
use fsmap::{dupes, prune, util};

#[derive(Parser)]
#[command(
    name = "fsmap",
    version,
    about = "Map a filesystem or codebase: sizes, code, git history, imports — and the findings worth acting on.",
    after_help = "Examples:\n  fsmap report .                     interactive HTML report\n  fsmap context . > REPO_MAP.md      repo map for AI agents\n  fsmap insights . --fail-on high    CI gate\n  fsmap brief --diff                 files you probably forgot to change\n  fsmap report ~ --no-git            where did my disk go?"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the self-contained interactive HTML report and print a summary.
    Report {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short, long, default_value = "fsmap-report.html")]
        output: PathBuf,
        /// Open the report in the default browser.
        #[arg(long)]
        open: bool,
        /// Node budget for the embedded map (larger trees are folded, totals stay exact).
        #[arg(long, default_value_t = 60_000)]
        max_nodes: usize,
        /// Omit the Content-Security-Policy meta tag (for hosts that inject scripts).
        #[arg(long)]
        no_csp: bool,
        #[command(flatten)]
        a: AnalyzeArgs,
    },
    /// Print the versioned JSON snapshot (unpruned unless --max-nodes).
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long)]
        pretty: bool,
        #[command(flatten)]
        a: AnalyzeArgs,
    },
    /// Token-budgeted Markdown map of the repository for AI agents (deterministic).
    Context {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Approximate token budget.
        #[arg(long, default_value_t = 6000)]
        budget: usize,
        #[command(flatten)]
        a: AnalyzeArgs,
    },
    /// Ranked findings for humans or CI.
    Insights {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
        /// Exit 1 if a finding at or above this severity exists (only new ones with --baseline).
        #[arg(long, value_enum)]
        fail_on: Option<Severity>,
        /// Previously written baseline; findings in it are not "new".
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Write the current finding ids as a baseline file.
        #[arg(long)]
        write_baseline: Option<PathBuf>,
        #[command(flatten)]
        a: AnalyzeArgs,
    },
    /// What to know before/after touching files: co-change partners, importers, tests, owners.
    Brief {
        /// Files to brief (relative to the current directory or absolute).
        paths: Vec<PathBuf>,
        /// Repository (or directory) to analyse.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Check the working-tree change set against BASE (default HEAD) for forgotten partners.
        #[arg(long, num_args = 0..=1, default_missing_value = "HEAD", value_name = "BASE")]
        diff: Option<String>,
        /// Claude Code hook mode: read the hook event JSON on stdin, print additionalContext JSON.
        #[arg(long)]
        hook: bool,
        /// Ignore the cached index and re-analyse.
        #[arg(long)]
        refresh: bool,
    },
    /// Compare two `fsmap scan` snapshots.
    Diff {
        old: PathBuf,
        new: PathBuf,
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Suggested read-deny rules so AI agents skip bulky, low-value paths.
    AgentIgnore {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = IgnoreFormat::Claude)]
        format: IgnoreFormat,
        #[command(flatten)]
        a: AnalyzeArgs,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
    Sarif,
    Markdown,
}

#[derive(Clone, Copy, ValueEnum)]
enum IgnoreFormat {
    /// `.claude/settings.json` `permissions.deny` entries.
    Claude,
    /// gitignore syntax (`.cursorignore`, `.aiderignore`, `.geminiignore`…).
    Gitignore,
}

#[derive(Args, Clone)]
struct AnalyzeArgs {
    /// Disk usage only: no git, no file reading, no imports, no duplicates (du speed).
    #[arg(long)]
    sizes_only: bool,
    /// Skip all git analysis.
    #[arg(long)]
    no_git: bool,
    /// Skip import extraction and the dependency graph.
    #[arg(long)]
    no_deps: bool,
    /// Skip duplicate-file detection.
    #[arg(long)]
    no_dupes: bool,
    /// History window back from each repo's newest commit: `2y`, `18m`, `90d`, `all`, or a `YYYY-MM-DD` date.
    #[arg(long, default_value = "2y", value_parser = parse_window)]
    since: Window,
    /// Safety cap on commits read per repository.
    #[arg(long, default_value_t = 30_000)]
    git_max_commits: u32,
    /// Maximum repositories analysed in one scan.
    #[arg(long, default_value_t = 64)]
    max_repos: usize,
    /// Descend into other mounted filesystems.
    #[arg(long)]
    cross_fs: bool,
    /// Skip paths matching this glob (relative to the scanned root); repeatable.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Do not read files larger than this many MiB.
    #[arg(long, default_value_t = 4)]
    max_file_mib: u64,
    /// How author identities appear in outputs.
    #[arg(long, value_enum, default_value_t = AuthorMode::Names)]
    authors: AuthorMode,
    /// Worker threads (default: all cores).
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// No progress display.
    #[arg(short, long)]
    quiet: bool,
}

impl AnalyzeArgs {
    fn options(&self) -> Result<analysis::Options> {
        let mut o = analysis::Options {
            git: !self.no_git && !self.sizes_only,
            deps: !self.no_deps && !self.sizes_only,
            read_content: !self.sizes_only,
            dupes: (!self.no_dupes && !self.sizes_only).then(dupes::Options::default),
            max_repos: self.max_repos,
            authors: self.authors,
            ..Default::default()
        };
        o.history.window = self.since.clone();
        o.history.max_commits = self.git_max_commits;
        o.scan.cross_fs = self.cross_fs;
        o.content.max_file_size = self.max_file_mib * 1024 * 1024;
        if !self.exclude.is_empty() {
            let mut b = GlobSetBuilder::new();
            for g in &self.exclude {
                b.add(Glob::new(g).with_context(|| format!("bad --exclude glob {g:?}"))?);
                b.add(Glob::new(&format!("{}/**", g.trim_end_matches('/')))?);
            }
            o.scan.exclude = Some(b.build()?);
        }
        Ok(o)
    }
}

fn parse_window(s: &str) -> Result<Window, String> {
    Window::parse(s)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "fsmap: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Spinner on stderr while the analysis runs (only on a terminal).
fn with_progress<T>(quiet: bool, f: impl FnOnce(&Progress) -> T) -> T {
    let progress = Arc::new(Progress::default());
    let done = Arc::new(AtomicBool::new(false));
    let show = !quiet && std::io::stderr().is_terminal();
    let handle = show.then(|| {
        let (p, d) = (progress.clone(), done.clone());
        std::thread::spawn(move || {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(
                indicatif::ProgressStyle::with_template("{spinner} {msg} {elapsed}")
                    .expect("template"),
            );
            while !d.load(Ordering::Relaxed) {
                let (c, t) = (
                    p.count.load(Ordering::Relaxed),
                    p.total.load(Ordering::Relaxed),
                );
                let msg = if t > 0 {
                    format!("{} {}/{}", p.phase(), util::count(c), util::count(t))
                } else if c > 0 {
                    format!("{} {}", p.phase(), util::count(c))
                } else {
                    p.phase().to_string()
                };
                bar.set_message(msg);
                bar.tick();
                std::thread::sleep(Duration::from_millis(120));
            }
            bar.finish_and_clear();
        })
    });
    let out = f(&progress);
    done.store(true, Ordering::Relaxed);
    if let Some(h) = handle {
        let _ = h.join();
    }
    out
}

fn analyse(path: &Path, a: &AnalyzeArgs) -> Result<(Analysis, Vec<Finding>)> {
    let opts = a.options()?;
    if let Some(n) = a.threads {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }
    let analysis = with_progress(a.quiet, |p| analysis::run(path, &opts, p))?;
    let findings = insights::run(&analysis);
    Ok((analysis, findings))
}

fn write_out(output: Option<&Path>, text: &[u8]) -> Result<()> {
    match output {
        Some(p) => std::fs::write(p, text).with_context(|| format!("cannot write {}", p.display())),
        None => {
            let mut out = std::io::stdout().lock();
            out.write_all(text)?;
            out.flush()?;
            Ok(())
        }
    }
}

fn snapshot_json(
    a: &Analysis,
    findings: &[Finding],
    max_nodes: Option<usize>,
    pretty: bool,
) -> Result<String> {
    let mut parts = Parts::new(a, findings);
    let pruned = max_nodes.and_then(|budget| prune::prune(&a.tree, budget, &parts.pinned()));
    let (tree, info) = match &pruned {
        Some(p) => {
            parts.remap(&p.map);
            (
                &p.tree,
                Some(PruneInfo {
                    original_nodes: a.tree.len(),
                    kept_nodes: p.tree.len(),
                }),
            )
        }
        None => (&a.tree, None),
    };
    let snap = Snapshot::new(a, tree, &parts, info);
    Ok(if pretty {
        serde_json::to_string_pretty(&snap)?
    } else {
        serde_json::to_string(&snap)?
    })
}

fn open_in_browser(path: &Path) {
    let target = path.to_string_lossy().into_owned();
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&target).status()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &target])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(&target).status()
    };
    if status.is_err() {
        eprintln!("fsmap: could not open a browser; open {target} manually");
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Report {
            path,
            output,
            open,
            max_nodes,
            no_csp,
            a,
        } => {
            let (analysis, findings) = analyse(&path, &a)?;
            let json = snapshot_json(&analysis, &findings, Some(max_nodes), false)?;
            let title = format!(
                "fsmap · {}",
                analysis
                    .root
                    .file_name()
                    .map_or_else(|| analysis.root.to_string_lossy(), |n| n.to_string_lossy())
            );
            let page = html::render(&json, &title, !no_csp);
            std::fs::write(&output, page)
                .with_context(|| format!("cannot write {}", output.display()))?;
            let mut out = anstream::stdout();
            terminal::summary(&mut out, &analysis, &findings, 8)?;
            writeln!(
                out,
                "\nReport: {} ({})",
                output.display(),
                util::bytes(std::fs::metadata(&output)?.len())
            )?;
            if open {
                open_in_browser(&output);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Scan {
            path,
            output,
            max_nodes,
            pretty,
            a,
        } => {
            let (analysis, findings) = analyse(&path, &a)?;
            let json = snapshot_json(&analysis, &findings, max_nodes, pretty)?;
            write_out(output.as_deref(), json.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Context {
            path,
            output,
            budget,
            a,
        } => {
            let (analysis, findings) = analyse(&path, &a)?;
            let md = context::render(&analysis, &findings, &context::Options { budget });
            write_out(output.as_deref(), md.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Insights {
            path,
            format,
            fail_on,
            baseline,
            write_baseline,
            a,
        } => {
            let (analysis, mut findings) = analyse(&path, &a)?;
            if let Some(b) = &baseline {
                let text = std::fs::read_to_string(b)
                    .with_context(|| format!("cannot read baseline {}", b.display()))?;
                let base: insights::Baseline =
                    serde_json::from_str(&text).context("invalid baseline file")?;
                let ids: HashSet<String> = base.findings.into_iter().collect();
                insights::mark_new(&mut findings, &ids);
            }
            if let Some(w) = &write_baseline {
                let json = serde_json::to_string_pretty(&insights::baseline_of(&findings))?;
                std::fs::write(w, json + "\n")
                    .with_context(|| format!("cannot write {}", w.display()))?;
            }
            match format {
                Format::Text => {
                    let mut out = anstream::stdout();
                    terminal::summary(&mut out, &analysis, &findings, usize::MAX)?;
                }
                Format::Json => {
                    write_out(None, serde_json::to_string_pretty(&findings)?.as_bytes())?
                }
                Format::Sarif => write_out(
                    None,
                    serde_json::to_string_pretty(&sarif::render(&findings))?.as_bytes(),
                )?,
                Format::Markdown => {
                    let mut s = String::from("| severity | finding | action |\n|---|---|---|\n");
                    for f in &findings {
                        s.push_str(&format!(
                            "| {}{} | **{}** — {} | {} |\n",
                            f.severity.as_str(),
                            if f.new { " (new)" } else { "" },
                            f.title.replace('|', "\\|"),
                            f.detail.replace('|', "\\|"),
                            f.action.replace('|', "\\|")
                        ));
                    }
                    write_out(None, s.as_bytes())?;
                }
            }
            let failing = fail_on.is_some_and(|min| {
                findings
                    .iter()
                    .any(|f| f.severity >= min && (baseline.is_none() || f.new))
            });
            Ok(if failing {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Cmd::Brief {
            paths,
            root,
            diff,
            hook,
            refresh,
        } => run_brief(paths, root, diff, hook, refresh),
        Cmd::Diff { old, new, top } => {
            let text = fsmap::diff::render(&old.to_string_lossy(), &new.to_string_lossy(), top)?;
            write_out(None, text.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::AgentIgnore { path, format, a } => {
            let (analysis, _) = analyse(&path, &a)?;
            let skip = context::skip_list(&analysis);
            let dirs: Vec<String> = skip
                .iter()
                .filter(|(p, _, _)| p.ends_with('/'))
                .map(|(p, _, _)| p.trim_end_matches('/').to_string())
                .collect();
            let text = match format {
                IgnoreFormat::Claude => {
                    let deny: Vec<String> = dirs.iter().map(|d| format!("Read(/{d}/**)")).collect();
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "permissions": { "deny": deny } }),
                    )? + "\n"
                }
                IgnoreFormat::Gitignore => {
                    let mut s = String::from(
                        "# Generated by `fsmap agent-ignore`: bulky, low-value paths for AI agents\n",
                    );
                    for (p, why, tok) in &skip {
                        if p.ends_with('/') {
                            s.push_str(&format!("/{p}  # {why}, ~{} tokens\n", util::count(*tok)));
                        }
                    }
                    s
                }
            };
            write_out(None, text.as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn repo_root(dir: &Path) -> PathBuf {
    brief::current_head(dir)
        .and_then(|_| {
            fsmap::git::run({
                let mut c = fsmap::git::git(dir);
                c.args(["rev-parse", "--show-toplevel"]);
                c
            })
            .ok()
        })
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

fn brief_index(root: &Path, refresh: bool, quiet: bool) -> Result<Index> {
    if !refresh && let Some(idx) = brief::load_cached(root) {
        return Ok(idx);
    }
    let opts = analysis::Options {
        dupes: None,
        ..Default::default()
    };
    let a = with_progress(quiet, |p| analysis::run(root, &opts, p))?;
    let idx = Index::build(&a);
    brief::store_cached(root, &idx);
    Ok(idx)
}

fn run_brief(
    paths: Vec<PathBuf>,
    root: PathBuf,
    diff: Option<String>,
    hook: bool,
    refresh: bool,
) -> Result<ExitCode> {
    let root = repo_root(&root);
    if hook {
        // Hooks must never break the agent: any problem means "say nothing".
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        // Outside a git work tree there is no history to brief on and nothing to key the
        // cache on: stay silent instead of re-analysing on every edit.
        if brief::current_head(&root).is_none() {
            return Ok(ExitCode::SUCCESS);
        }
        if let Ok(idx) = brief_index(&root, refresh, true) {
            let owners = CodeOwners::load(&root);
            if let Some(json) = brief::hook_response(&idx, owners.as_ref(), &root, &input) {
                println!("{json}");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let idx = brief_index(&root, refresh, false)?;
    let owners = CodeOwners::load(&root);
    let mut out = String::new();
    if let Some(base) = diff {
        let changed = brief::changed_files(&root, &base)?;
        out.push_str(&format!(
            "## fsmap brief --diff {base}: {} changed file(s)\n\n",
            changed.len()
        ));
        let missing = brief::forgotten(&idx, &changed, 0.5);
        if missing.is_empty() {
            out.push_str("No usual co-change partner is missing from this change set.\n");
        } else {
            out.push_str("### Possibly forgotten (usually change together)\n");
            for (from, other, conf, shared) in missing.iter().take(15) {
                out.push_str(&format!(
                    "- `{}` — changed in {:.0}% of `{}`'s commits ({shared} shared)\n",
                    util::md(other),
                    conf * 100.0,
                    util::md(from)
                ));
            }
        }
        out.push('\n');
        let set: HashSet<&str> = changed.iter().map(String::as_str).collect();
        for c in changed
            .iter()
            .filter(|c| idx.partners.contains_key(*c) || idx.importers.contains_key(*c))
            .take(8)
        {
            out.push_str(&brief::brief_file(&idx, owners.as_ref(), c, &set));
            out.push('\n');
        }
    } else {
        if paths.is_empty() {
            bail!("give file paths to brief, or --diff");
        }
        let cwd = std::env::current_dir()?;
        for p in paths {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(&p)
            };
            let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
            let rel = abs
                .strip_prefix(&root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.to_string_lossy().into_owned());
            out.push_str(&brief::brief_file(
                &idx,
                owners.as_ref(),
                &rel,
                &HashSet::new(),
            ));
            out.push('\n');
        }
    }
    write_out(None, out.as_bytes())?;
    Ok(ExitCode::SUCCESS)
}
