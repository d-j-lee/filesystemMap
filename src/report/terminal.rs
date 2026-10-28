//! Human summary on the terminal: headline numbers, zones, languages, git, findings.

use std::io::Write;

use anstyle::{AnsiColor, Style};

use crate::analysis::Analysis;
use crate::classify;
use crate::insights::{Finding, Severity};
use crate::model::Zone;
use crate::snapshot::totals;
use crate::util::{bytes, count, date, sanitize};

const BOLD: Style = Style::new().bold();
const DIM: Style = Style::new().dimmed();

fn sev_style(s: Severity) -> Style {
    match s {
        Severity::High => AnsiColor::Red.on_default().bold(),
        Severity::Medium => AnsiColor::Yellow.on_default().bold(),
        Severity::Low => AnsiColor::Cyan.on_default(),
        Severity::Info => DIM,
    }
}

fn bar(frac: f64, width: usize) -> String {
    let full = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

pub fn summary(
    w: &mut impl Write,
    a: &Analysis,
    findings: &[Finding],
    max_findings: usize,
) -> std::io::Result<()> {
    let t = &a.tree;
    let tot = totals(t);
    writeln!(
        w,
        "{BOLD}fsmap{BOLD:#} {} {DIM}·{DIM:#} {} on disk {DIM}·{DIM:#} {} files {DIM}·{DIM:#} {} code lines{}",
        sanitize(&a.root.to_string_lossy()),
        bytes(tot.disk),
        count(tot.files),
        count(tot.code),
        if a.repos.is_empty() {
            String::new()
        } else {
            format!(
                " {DIM}·{DIM:#} {} repo{}",
                a.repos.len(),
                if a.repos.len() == 1 { "" } else { "s" }
            )
        }
    )?;

    // Zones.
    for z in Zone::ALL {
        if let Some(b) = tot.by_zone.get(z.as_str()) {
            let frac = b.disk as f64 / tot.disk.max(1) as f64;
            if frac < 0.001 && b.files < 10 {
                continue;
            }
            writeln!(
                w,
                "  {:<10} {:>9}  {DIM}{}{DIM:#} {:>4.0}%  {:>7} files",
                z.as_str(),
                bytes(b.disk),
                bar(frac, 16),
                frac * 100.0,
                count(b.files)
            )?;
        }
    }

    // Languages by code lines.
    let mut langs: Vec<(&str, u64)> = tot
        .by_language
        .iter()
        .filter(|(n, b)| {
            b.code > 0
                && classify::lang_by_name(n)
                    .is_some_and(|i| classify::lang(i).category == crate::model::Category::Code)
        })
        .map(|(n, b)| (*n, b.code))
        .collect();
    langs.sort_by_key(|l| std::cmp::Reverse(l.1));
    if !langs.is_empty() {
        let s: Vec<String> = langs
            .iter()
            .take(6)
            .map(|(n, c)| format!("{n} {}", count(*c)))
            .collect();
        writeln!(
            w,
            "  {DIM}code{DIM:#}       {}",
            s.join(&format!(" {DIM}·{DIM:#} "))
        )?;
    }

    // Git.
    for r in a.repos.iter().take(5) {
        let label = if r.path == "." {
            String::new()
        } else {
            format!("{} ", sanitize(&r.path))
        };
        match (&r.history, &r.error) {
            (_, Some(e)) => writeln!(
                w,
                "  {DIM}git{DIM:#}        {label}{DIM}error: {}{DIM:#}",
                sanitize(e)
            )?,
            (Some(h), None) if h.commits > 0 => writeln!(
                w,
                "  {DIM}git{DIM:#}        {label}{} commits {} → {}, {} authors ({} active){}{}",
                count(u64::from(h.commits)),
                date(h.first),
                date(h.last),
                h.authors,
                h.active_authors,
                h.truck_factor
                    .map(|f| format!(", truck factor {f}"))
                    .unwrap_or_default(),
                if r.shallow { " (shallow)" } else { "" }
            )?,
            _ => writeln!(
                w,
                "  {DIM}git{DIM:#}        {label}{} tracked files, no history in window",
                count(u64::from(r.tracked))
            )?,
        }
    }
    if a.repos.len() > 5 {
        writeln!(
            w,
            "  {DIM}git{DIM:#}        … and {} more repositories",
            a.repos.len() - 5
        )?;
    }
    if let Some(g) = &a.graph {
        let total: u32 = g.stats.iter().map(|s| s.imports).sum();
        if total > 0 {
            writeln!(
                w,
                "  {DIM}imports{DIM:#}    {} internal edges from {} import statements, {} cycle{}",
                count(g.edges.len() as u64),
                count(u64::from(total)),
                g.cycles.len(),
                if g.cycles.len() == 1 { "" } else { "s" }
            )?;
        }
    }

    // Findings.
    let by = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    writeln!(
        w,
        "\n{BOLD}Findings{BOLD:#} {DIM}({} high · {} medium · {} low · {} info){DIM:#}",
        by(Severity::High),
        by(Severity::Medium),
        by(Severity::Low),
        by(Severity::Info)
    )?;
    for f in findings.iter().take(max_findings) {
        let st = sev_style(f.severity);
        let new = if f.new { " NEW" } else { "" };
        writeln!(
            w,
            "  {st}{:<6}{st:#} {}{}",
            f.severity.as_str().to_uppercase(),
            sanitize(&f.title),
            new
        )?;
        writeln!(w, "         {DIM}{}{DIM:#}", sanitize(&f.detail))?;
        if let Some(cmd) = &f.command {
            writeln!(w, "         {DIM}${DIM:#} {}", sanitize(cmd))?;
        }
    }
    if findings.len() > max_findings {
        writeln!(
            w,
            "  {DIM}… {} more (see the report or `fsmap insights`){DIM:#}",
            findings.len() - max_findings
        )?;
    }
    Ok(())
}
