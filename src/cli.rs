use crate::{
    provider::Provider,
    search::{self, Options},
    source::{Control, Coverage, Limits, Source},
};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Parser)]
#[command(name = "blink", about = "Find source by behavior", version)]
pub struct Cli {
    #[arg(long, global = true, help = "Write versioned JSON to stdout")]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List eligible current working-tree text files.
    Files {
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Find source that implements behavior described by a query.
    Search {
        query: String,
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        thorough: bool,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_parser = timeout_seconds, help = "Whole-operation timeout in seconds, up to 300")]
        timeout: Option<Duration>,
    },
    /// Inspect local configuration without contacting a provider.
    Doctor,
    /// Print the compiled package version.
    Version,
}

#[derive(Serialize)]
struct FileRecord<'a> {
    path: &'a str,
    bytes: usize,
    sha256: &'a str,
}
#[derive(Serialize)]
struct Inventory<'a> {
    schema_version: u32,
    command: &'static str,
    files: Vec<FileRecord<'a>>,
    coverage: &'a Coverage,
}

pub async fn run(cli: Cli, out: &mut dyn Write, err: &mut dyn Write) -> io::Result<u8> {
    match cli.command {
        Command::Files { root } => {
            let source = match Source::open(&root) {
                Ok(source) => source,
                Err(error) => {
                    if cli.json {
                        json(
                            out,
                            &serde_json::json!({"schema_version": SCHEMA_VERSION, "command": "files", "error": {"kind": format!("{:?}", error.kind()), "operation": "root_open"}}),
                        )?;
                    }
                    writeln!(
                        err,
                        "blink: cannot open root {} ({:?})",
                        root.display(),
                        error.kind()
                    )?;
                    return Ok(2);
                }
            };
            let snapshot = source.snapshot(Limits::default(), &mut || Control::Continue);
            let coverage = snapshot.coverage();
            if cli.json {
                json(
                    out,
                    &Inventory {
                        schema_version: SCHEMA_VERSION,
                        command: "files",
                        files: snapshot
                            .files()
                            .iter()
                            .map(|file| FileRecord {
                                path: file.path(),
                                bytes: file.bytes().len(),
                                sha256: file.sha256(),
                            })
                            .collect(),
                        coverage,
                    },
                )?;
            } else {
                for file in snapshot.files() {
                    writeln!(out, "{}", serde_json::to_string(file.path())?)?;
                }
                writeln!(
                    err,
                    "blink: {} files, {} bytes, {} visited entries, coverage {}",
                    coverage.files_included,
                    coverage.bytes_included,
                    coverage.visited_entries,
                    if coverage.complete {
                        "complete"
                    } else {
                        "incomplete"
                    }
                )?;
            }
            if !coverage.complete {
                writeln!(
                    err,
                    "blink: inventory incomplete ({} read or policy errors, stops {:?})",
                    coverage.issues.len(),
                    coverage.stops
                )?;
            }
            Ok(if coverage.complete { 0 } else { 3 })
        }
        Command::Search {
            query,
            root,
            thorough,
            limit,
            timeout,
        } => {
            let mut options = if thorough {
                Options::thorough()
            } else {
                Options::default()
            };
            options.limit = limit;
            if let Some(timeout) = timeout {
                options.timeout = timeout;
            }
            if let Err(message) = options.validate(&query) {
                return configuration_error(cli.json, out, err, "invalid_search", message);
            }
            let key = match std::env::var("OPENAI_API_KEY") {
                Ok(key) if !key.trim().is_empty() => key,
                _ => {
                    return configuration_error(
                        cli.json,
                        out,
                        err,
                        "missing_api_key",
                        "OPENAI_API_KEY is required for search",
                    );
                }
            };
            let provider = match Provider::new(&key) {
                Ok(provider) => provider,
                Err(_) => {
                    return configuration_error(
                        cli.json,
                        out,
                        err,
                        "invalid_api_key",
                        "OPENAI_API_KEY is invalid",
                    );
                }
            };
            let source = match Source::open(root) {
                Ok(source) => source,
                Err(_) => {
                    return configuration_error(
                        cli.json,
                        out,
                        err,
                        "root_open",
                        "cannot open search root",
                    );
                }
            };
            let cancelled = Arc::new(AtomicBool::new(false));
            let signal_cancelled = Arc::clone(&cancelled);
            let signal = tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_cancelled.store(true, Ordering::Relaxed);
                }
            });
            let mut report = search::execute(source, query, options, provider, cancelled).await;
            signal.abort();
            let bytes = if cli.json {
                report.encode_json()?
            } else {
                report.encode_text()
            };
            out.write_all(&bytes)?;
            writeln!(
                err,
                "blink: {:?}, {} of {} windows judged, {} requests, {} request bytes, {} results omitted{}",
                report.operation,
                report.coverage.windows_judged,
                report.coverage.windows_planned,
                report.budgets.attempts,
                report.budgets.encoded_request_bytes,
                report.omitted_results,
                if report.output_truncated {
                    ", output truncated"
                } else {
                    ""
                }
            )?;
            Ok(report.exit_code())
        }
        Command::Doctor => {
            let present = std::env::var_os("OPENAI_API_KEY").is_some_and(|value| !value.is_empty());
            if cli.json {
                json(
                    out,
                    &serde_json::json!({"schema_version": SCHEMA_VERSION, "command": "doctor", "version": env!("CARGO_PKG_VERSION"), "provider_contacted": false, "configuration": {"OPENAI_API_KEY": {"present": present}}, "inventory_ready": true}),
                )?;
            } else {
                writeln!(out, "blink {}", env!("CARGO_PKG_VERSION"))?;
                writeln!(out, "Inventory ready")?;
                writeln!(
                    out,
                    "OPENAI_API_KEY: {}",
                    if present { "present" } else { "absent" }
                )?;
                writeln!(out, "Provider contacted: no")?;
            }
            Ok(0)
        }
        Command::Version => {
            if cli.json {
                json(
                    out,
                    &serde_json::json!({"schema_version": SCHEMA_VERSION, "command": "version", "version": env!("CARGO_PKG_VERSION")}),
                )?;
            } else {
                writeln!(out, "blink {}", env!("CARGO_PKG_VERSION"))?;
            }
            Ok(0)
        }
    }
}
fn json(out: &mut dyn Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    writeln!(out)
}

fn timeout_seconds(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| "timeout must be a number of seconds")?;
    let duration =
        Duration::try_from_secs_f64(seconds).map_err(|_| "timeout must be finite and positive")?;
    if duration.is_zero() || duration > Duration::from_secs(300) {
        return Err("timeout must be greater than zero and at most 300 seconds".into());
    }
    Ok(duration)
}

fn configuration_error(
    json_mode: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
    code: &str,
    message: &str,
) -> io::Result<u8> {
    if json_mode {
        json(
            out,
            &serde_json::json!({"schema_version": SCHEMA_VERSION, "command": "search", "error": {"code": code}}),
        )?;
    }
    writeln!(err, "blink: {message}")?;
    Ok(2)
}
