use rsmake::{Engine, Opts};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: rsmake [options] [VAR=value ...] [target ...]

  -f FILE     read FILE as the makefile
  -C DIR      change to DIR before reading the makefile
  -j N        run up to N recipes concurrently (a bare -j is refused; an
              unbounded job count is not supported)
  -k          keep going after a failed target
  -n          print recipes without executing them
  -s          do not echo recipes
  -i          ignore errors from recipes
  -B          consider every target out of date
  -e          let the environment override the makefile
  -q          exit 1 if any target is out of date, without building
  -r          no built-in rules
  -R          no built-in variables
  --version   print version
  --help      print this message
";

/// Hand-written because make's command line mixes clustered short flags with
/// positional `VAR=value` assignments and bare goals, and `-j`'s argument is
/// optional-but-numeric. Every argument-parsing crate models one of those
/// three badly.
fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--version" {
            println!("rsmake {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        if a == "--help" {
            print!("{USAGE}");
            std::process::exit(0);
        }
        if let Some(eq) = a.find('=')
            && !a.starts_with('-')
        {
            o.overrides
                .push((a[..eq].to_string(), a[eq + 1..].to_string()));
            i += 1;
            continue;
        }
        if !a.starts_with('-') || a == "-" {
            o.goals.push(a.clone());
            i += 1;
            continue;
        }

        // A cluster like `-knf Makefile`: flags consume the rest of the
        // cluster as their argument when they take one, else the next word.
        let mut chars = a[1..].chars().peekable();
        while let Some(c) = chars.next() {
            let rest: String = chars.clone().collect();
            let mut take_arg = |need: bool| -> Result<Option<String>, String> {
                if !rest.is_empty() {
                    for _ in 0..rest.chars().count() {
                        chars.next();
                    }
                    return Ok(Some(rest.clone()));
                }
                if need {
                    i += 1;
                    return args
                        .get(i)
                        .cloned()
                        .ok_or_else(|| format!("option -{c} requires an argument"))
                        .map(Some);
                }
                Ok(None)
            };
            match c {
                'f' => o.makefile = take_arg(true)?.map(PathBuf::from),
                'C' => o.directory = take_arg(true)?.map(PathBuf::from),
                'j' => {
                    // `-j4` and `-j 4` are 4; a bare `-j` is refused. An
                    // unbounded job count is refused rather than guessed: on a
                    // build host it is how a makefile takes the machine down.
                    let inline = take_arg(false)?;
                    let n = match inline {
                        Some(s) => s
                            .parse::<usize>()
                            .map_err(|_| format!("invalid job count `{s}`"))?,
                        None => match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                            Some(n) => {
                                i += 1;
                                n
                            }
                            None => {
                                return Err("-j requires a job count; unbounded parallelism is \
                                     not supported"
                                    .to_string());
                            }
                        },
                    };
                    if n == 0 {
                        return Err("job count must be at least 1".to_string());
                    }
                    o.jobs = n;
                }
                'k' => o.keep_going = true,
                'n' => o.dry_run = true,
                's' => o.silent = true,
                'i' => o.ignore_errors = true,
                'B' => o.always_make = true,
                'e' => o.env_overrides = true,
                'q' => o.question = true,
                'r' => o.no_builtin_rules = true,
                'R' => o.no_builtin_vars = true,
                other => return Err(format!("unknown option -{other}")),
            }
        }
        i += 1;
    }

    // `-n` output is the differential oracle's input, so it must be
    // reproducible; concurrent workers would interleave it.
    if o.dry_run {
        o.jobs = 1;
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
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
            ExitCode::from(2)
        }
    }
}
