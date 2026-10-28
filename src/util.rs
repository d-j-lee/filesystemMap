//! Small formatting helpers shared by terminal, Markdown and finding text.

/// `1536` → `1.5 KiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `12345` → `12.3k`.
pub fn count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Seconds → `3 days`, `5 months`.
pub fn duration(secs: i64) -> String {
    let d = secs.max(0) / 86_400;
    if d < 1 {
        "today".into()
    } else if d < 60 {
        format!("{d} days")
    } else if d < 730 {
        format!("{} months", d / 30)
    } else {
        format!("{:.1} years", d as f64 / 365.0)
    }
}

/// Unix seconds → `YYYY-MM-DD` (UTC), civil-from-days (Howard Hinnant).
pub fn date(ts: i64) -> String {
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Replace control characters so hostile file names cannot drive the terminal.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Quote a path for a POSIX shell command line.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@%:,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(300 * 1024 * 1024), "300 MiB");
        assert_eq!(count(12_345), "12k");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_793_194_026), "2026-10-28");
        assert_eq!(sanitize("a\x1b[31mb"), "a?[31mb");
        assert_eq!(shell_quote("src/a.rs"), "src/a.rs");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }
}
