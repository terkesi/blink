use blink::{
    provider::Provider,
    search::{self, Operation, Options},
    source::Source,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    io,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    root: PathBuf,
    query: String,
    mode: String,
    threshold: f64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let case: Case = serde_json::from_reader(io::stdin().lock())?;
    let mut options = match case.mode.as_str() {
        "default" => Options::default(),
        "thorough" => Options::thorough(),
        _ => return Err("mode must be default or thorough".into()),
    };
    options.threshold = case.threshold;
    options.validate(&case.query)?;
    let key = env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required")?;
    let provider = Provider::new(&key)?;
    let source = Source::open(case.root)?;
    let mut report = search::execute(
        source,
        case.query,
        options,
        provider,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    report.encode_json()?;
    let status = match report.operation {
        Operation::Completed => "ok",
        _ if report.budgets.stops.contains(&"deadline") => "timeout",
        _ => "error",
    };
    let results: Vec<_> = report
        .results
        .iter()
        .map(|result| {
            serde_json::json!({
                "path": result.path,
                "start_byte": result.start_byte,
                "end_byte": result.end_byte,
                "start_line": result.start_line,
                "end_line": result.end_line,
                "snippet": result.excerpt,
                "sha256": format!("{:x}", Sha256::digest(result.excerpt.as_bytes())),
                "file_sha256": result.sha256,
            })
        })
        .collect();
    let judgments: Vec<_> = report
        .raw_judgments
        .iter()
        .filter_map(|judgment| {
            judgment.probability.map(|probability| {
                serde_json::json!({
                    "path": judgment.path,
                    "start_byte": judgment.start_byte,
                    "end_byte": judgment.end_byte,
                    "start_line": judgment.start_line,
                    "end_line": judgment.end_line,
                    "score": probability,
                })
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "id": case.id,
            "status": status,
            "available_candidates": report.coverage.windows_planned,
            "discovered_candidates": report.coverage.windows_selected,
            "judgments": judgments,
            "returned_count": results.len(),
            "results": results,
            "output_truncated": report.output_truncated,
            "changed_files": report.changed_files,
            "errors": report.errors,
            "budgets": report.budgets,
            "coverage": report.coverage,
        })
    );
    Ok(())
}
