//! Self-contained HTML report: template + viewer JS/CSS compiled into the binary, snapshot
//! JSON embedded as inert data. A hash-based CSP allows only the bundled script.

use base64::Engine;
use sha2::{Digest, Sha256};

const TEMPLATE: &str = include_str!("../../viewer/index.html");
const CSS: &str = include_str!("../../viewer/style.css");
const JS: &str = include_str!("../../viewer/app.js");

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// JSON is embedded in `<script type="application/json">`; `<` must never appear raw or a
/// hostile file name containing `</script>` would end the element.
fn script_safe(json: &str) -> String {
    json.replace('<', "\\u003c")
}

pub fn render(snapshot_json: &str, title: &str, csp: bool) -> String {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(JS.as_bytes()));
    let policy = if csp {
        format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
             script-src 'sha256-{hash}'; style-src 'unsafe-inline'; img-src data:; \
             base-uri 'none'; form-action 'none'\">"
        )
    } else {
        String::new()
    };
    fill(
        TEMPLATE,
        &[
            ("{{CSP}}", &policy),
            ("{{TITLE}}", &html_escape(title)),
            ("/*{{CSS}}*/", CSS),
            ("{{DATA}}", &script_safe(snapshot_json)),
            ("/*{{JS}}*/", JS),
        ],
    )
}

/// Single pass over the template: each marker is replaced exactly once, in template order,
/// and inserted text is never searched again (a directory named `{{DATA}}` stays a name).
fn fill(template: &str, parts: &[(&str, &str)]) -> String {
    let mut out =
        String::with_capacity(template.len() + parts.iter().map(|p| p.1.len()).sum::<usize>());
    let mut rest = template;
    for (marker, value) in parts {
        let Some(at) = rest.find(marker) else {
            continue;
        };
        out.push_str(&rest[..at]);
        out.push_str(value);
        rest = &rest[at + marker.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_safely() {
        let html = render(
            r#"{"name":"</script><script>alert(1)</script>"}"#,
            "a<b",
            true,
        );
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script>"));
        assert!(html.contains("a&lt;b"));
        assert!(html.contains("Content-Security-Policy"));
        // The bundled script must be byte-identical to what the hash covers.
        assert!(html.contains(JS));
        // Names that look like markers stay names.
        let tricky = render(r#"{"root":"{{DATA}} /*{{JS}}*/"}"#, "{{DATA}}", false);
        assert!(tricky.contains("<title>{{DATA}}</title>"));
        assert!(tricky.contains(r#""root":"{{DATA}} /*{{JS}}*/""#));
        assert!(tricky.contains(JS));
    }
}
