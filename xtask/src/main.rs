//! Dev tools for the engine's storage/write paths, dispatched on the first
//! argument rather than each getting its own `[[bin]]` target:
//!
//!   cargo run -p xtask -- membench [cells dims]   (see `membench.rs`)
//!   cargo run -p xtask -- writebench [cells dims]  (see `writebench.rs`)
//!   cargo run -p xtask -- examples [out_dir]      (builds the example databases, default `target/examples`)

mod membench;
mod writebench;

// mimalloc, not the platform default: glibc malloc keeps freed memory
// instead of returning it, which would skew membench's resident-memory
// numbers (a fix that frees memory could look like it didn't work).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("usage: cargo run -p xtask -- membench [cells dims]");
        eprintln!("       cargo run -p xtask -- writebench [cells dims]");
        eprintln!("       cargo run -p xtask -- examples [out_dir]");
        return ExitCode::from(2);
    };
    let rest: Vec<String> = args.collect();
    match cmd.as_str() {
        "membench" => membench::run(&rest),
        "writebench" => writebench::run(&rest),
        "examples" => return examples(rest.first().map(String::as_str)),
        other => {
            eprintln!("unknown xtask command: {other}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// Builds the example databases from `examples/sources` into `out_dir`.
fn examples(out_dir: Option<&str>) -> ExitCode {
    let out = std::path::PathBuf::from(out_dir.unwrap_or("target/examples"));
    let started = std::time::Instant::now();
    match fluxolap_examples::build_all(&fluxolap_examples::default_sources_dir(), &out) {
        Ok(files) => {
            for f in &files {
                let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                println!("{:>10} bytes  {}", size, f.display());
            }
            println!("built {} example databases in {:.1}s", files.len(), started.elapsed().as_secs_f64());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("building examples failed: {e}");
            ExitCode::FAILURE
        }
    }
}
