mod cell;
mod markdown;
mod runner;

use cell::{Cell, Kind, Verdict};
use std::time::Duration;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const ACTOR_TIMEOUT: Duration = Duration::from_secs(30);

struct Opts {
    file: String,
    edge: String,
}

fn parse_args() -> Result<Opts, String> {
    let mut file = None;
    let mut edge = "edge".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edge" => {
                edge = args.next().ok_or("--edge needs a value")?;
            }
            "-h" | "--help" => {
                return Err("usage: skill <file.md> [--edge <path>]".to_string());
            }
            other if other.starts_with('-') => return Err(format!("unknown flag '{other}'")),
            other => {
                if file.is_some() {
                    return Err("only one file may be given".to_string());
                }
                file = Some(other.to_string());
            }
        }
    }
    let file = file.ok_or("usage: skill <file.md> [--edge <path>]")?;
    Ok(Opts { file, edge })
}

fn run_cell(opts: &Opts, c: &Cell) -> Result<(), String> {
    let outcome = match c.kind {
        Kind::Mini => runner::run_script(&opts.edge, &c.body, SCRIPT_TIMEOUT),
        Kind::Actor | Kind::Untrusted => runner::run_actor(&opts.edge, &c.body, ACTOR_TIMEOUT),
        Kind::MiniSkip => unreachable!("skip cells never become cells"),
    };
    outcome.and_then(|o| runner::check(&c.expect, &o, c.verdict == Verdict::Error))
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(if e.starts_with("usage:") { 0 } else { 2 });
        }
    };
    let src = match std::fs::read_to_string(&opts.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {e}", opts.file);
            std::process::exit(2);
        }
    };
    let cells = markdown::scan(&src)
        .and_then(|blocks| cell::collect(&blocks))
        .unwrap_or_else(|e| {
            eprintln!("{}: {e}", opts.file);
            std::process::exit(2);
        });

    let mut passed = 0;
    let mut failed = 0;
    for c in &cells {
        match run_cell(&opts, c) {
            Ok(()) => passed += 1,
            Err(e) => {
                failed += 1;
                eprintln!("FAIL {}:{}\n{e}", opts.file, c.line);
            }
        }
    }
    println!("{passed} passed, {failed} failed");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
