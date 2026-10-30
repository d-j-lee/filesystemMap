//! SARIF 2.1.0 output for code-scanning UIs (GitHub, Azure DevOps, IDEs).

use serde_json::{Value, json};

use crate::insights::{Finding, RULES, Severity};

pub fn render(findings: &[Finding]) -> Value {
    let rules: Vec<Value> = RULES
        .iter()
        .map(|(id, desc)| json!({ "id": id, "shortDescription": { "text": desc } }))
        .collect();
    let results: Vec<Value> = findings
        .iter()
        .map(|f| {
            let level = match f.severity {
                Severity::High => "error",
                Severity::Medium => "warning",
                Severity::Low | Severity::Info => "note",
            };
            let locations: Vec<Value> = f
                .paths
                .iter()
                .filter(|p| p.as_str() != ".")
                .take(10)
                .map(|p| json!({ "physicalLocation": { "artifactLocation": { "uri": p } } }))
                .collect();
            json!({
                "ruleId": f.rule,
                "level": level,
                "message": { "text": format!("{}\n{}\n{}", f.title, f.detail, f.action) },
                "locations": locations,
                "partialFingerprints": { "fsmapId": f.id },
            })
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "fsmap",
                "version": env!("CARGO_PKG_VERSION"),
                "informationUri": "https://github.com/d-j-lee/filesystemMap",
                "rules": rules,
            }},
            "results": results,
        }]
    })
}
