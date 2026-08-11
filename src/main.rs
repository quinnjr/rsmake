//! The `rsmake` binary: argument parsing and the engine both live in the
//! library, so the whole flag surface is reachable from tests. What is left
//! here is the part that cannot be tested in-process — printing and exiting.

use rsmake::{Engine, Request, USAGE, parse_args};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(Request::Run(o)) => o,
        Ok(Request::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Request::Version) => {
            println!("rsmake {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("rsmake: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut engine = Engine::new(opts);
    match engine.make() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("rsmake: {e}");
            // After the error, never before: GNU make prints the failing
            // recipe's `*** ... Error N` line first and the `-k` summary of
            // abandoned goals last.
            for t in &engine.not_remade {
                eprintln!("rsmake: Target '{t}' not remade because of errors.");
            }
            ExitCode::from(2)
        }
    }
}
