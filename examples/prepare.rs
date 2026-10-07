use blink::{
    search::{Options, prepare},
    source::{Control, Source},
};
use std::{env, error::Error, time::Instant};

fn main() -> Result<(), Box<dyn Error>> {
    let root = env::args_os().nth(1).ok_or("usage: prepare ROOT")?;
    let started = Instant::now();
    let source = Source::open(root)?;
    let prepared = prepare(
        &source,
        "where is the retry delay bounded",
        &Options::default(),
        &mut || Control::Continue,
    );
    println!(
        "{}",
        serde_json::json!({
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
            "coverage": prepared.coverage(),
            "windows": prepared.window_count(),
            "candidates": prepared.candidate_count(),
        })
    );
    Ok(())
}
