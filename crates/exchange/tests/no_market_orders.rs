//! Guards the spec's central rule: no code path may emit a market order.
//!
//! This is a source-level test rather than a behavioural one because the rule
//! is about what the codebase *cannot express*, not about what one function
//! happens to do at runtime.

use std::fs;
use std::path::Path;

fn rust_sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).expect("readable directory") {
        let entry = entry.expect("readable entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).expect("readable source");
            out.push((path.display().to_string(), text));
        }
    }
}

fn workspace_sources() -> Vec<(String, String)> {
    // CARGO_MANIFEST_DIR is crates/exchange; walk up to the workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf();

    let mut out = Vec::new();
    for sub in ["crates", "bot"] {
        let dir = root.join(sub);
        if dir.exists() {
            rust_sources(&dir, &mut out);
        }
    }
    out
}

#[test]
fn no_source_file_constructs_a_market_order() {
    let offenders: Vec<String> = workspace_sources()
        .into_iter()
        .filter(|(path, text)| {
            !path.ends_with("no_market_orders.rs") && text.contains("\"Market\"")
        })
        .map(|(path, _)| path)
        .collect();

    assert!(
        offenders.is_empty(),
        "market order literal found in: {offenders:?}\n\
         The spec forbids market orders anywhere, including stop fallbacks."
    );
}

#[test]
fn every_order_type_literal_is_limit() {
    // Any orderType we send must be Limit. Catches slOrderType/tpOrderType
    // regressions as well as the entry order itself.
    for (path, text) in workspace_sources() {
        if path.ends_with("no_market_orders.rs") {
            continue;
        }
        for (lineno, line) in text.lines().enumerate() {
            if line.contains("OrderType")
                && line.contains('"')
                && !line.trim_start().starts_with("//")
            {
                assert!(
                    !line.contains("\"Market\""),
                    "{path}:{} sets an order type to Market: {line}",
                    lineno + 1
                );
            }
        }
    }
}
