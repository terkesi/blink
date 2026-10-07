#![forbid(unsafe_code)]

use blink::cli::{self, Cli};
use clap::Parser;
use std::{
    io::{self, Write},
    process::ExitCode,
};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if error.use_stderr() && std::env::args_os().any(|arg| arg == "--json") {
                let _ = writeln!(
                    io::stdout().lock(),
                    "{}",
                    serde_json::json!({"schema_version": 1, "error": {"code": "invalid_arguments"}})
                );
            }
            let code = error.exit_code() as u8;
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    match cli::run(cli, &mut io::stdout().lock(), &mut io::stderr().lock()).await {
        Ok(code) => ExitCode::from(code),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(
                io::stderr().lock(),
                "blink: output failed ({:?})",
                error.kind()
            );
            ExitCode::from(3)
        }
    }
}
