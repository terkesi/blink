use crate::source::{Control, Coverage, Limits, Source};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::PathBuf,
};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Parser)]
#[command(
    name = "blink",
    about = "Inspect eligible working-tree source",
    version
)]
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

pub fn run(cli: Cli, out: &mut dyn Write, err: &mut dyn Write) -> io::Result<u8> {
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
