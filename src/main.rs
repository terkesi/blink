#![forbid(unsafe_code)]

use blink::cli::{self, Cli};
use clap::Parser;
use std::{
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli::run(cli, &mut io::stdout().lock(), &mut io::stderr().lock()) {
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
