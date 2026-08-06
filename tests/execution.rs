//! Execution tests.
//!
//! `-n` proves rsmake *intends* the right commands. This proves it runs them,
//! in an order that works, and produces the same artefacts GNU make does. The
//! two are independent: a scheduler that links before compiling emits a
//! perfect dry run and a broken build.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROJECT: &[(&str, &str)] = &[
    (
        "main.c",
        "int util(int);\nint main(void) { return util(2) - 4; }\n",
    ),
    ("util.c", "int util(int x) { return x * 2; }\n"),
    ("util.h", "int util(int);\n"),
    (
        "Makefile",
        "CC = cc\nCFLAGS = -O1\n\
         all: prog\n\
         prog: main.o util.o\n\
         \t$(CC) $(CFLAGS) $^ -o $@\n\
         main.o: main.c util.h\n\
         \t$(CC) $(CFLAGS) -c main.c -o $@\n\
         util.o: util.c util.h\n\
         \t$(CC) $(CFLAGS) -c util.c -o $@\n\
         .PHONY: all clean\n\
         clean:\n\
         \trm -f *.o prog\n",
    ),
];

fn lay_out(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsmake-exec-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create build directory");
    for (name, body) in PROJECT {
        std::fs::write(dir.join(name), body).expect("write source");
    }
    dir
}

fn build_with(program: &str, dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("could not run {program}: {e}"));
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn require_cc() {
    let ok = Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success());
    assert!(
        ok.unwrap_or(false),
        "`cc` is not on this host, so the execution tests cannot compile anything. \
         That is a broken environment rather than a reason to skip: without it, \
         nothing here verifies that rsmake's recipes actually run."
    );
}

#[test]
fn builds_the_same_artefacts_as_gnu_make() {
    require_cc();
    let gnu_dir = lay_out("gnu");
    let rs_dir = lay_out("rs");

    let (gnu_code, gnu_log) = build_with("make", &gnu_dir, &[]);
    assert_eq!(
        gnu_code, 0,
        "GNU make failed on the fixture project:\n{gnu_log}"
    );
    let (rs_code, rs_log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &rs_dir, &[]);
    assert_eq!(
        rs_code, 0,
        "rsmake failed on the fixture project:\n{rs_log}"
    );

    for artefact in ["main.o", "util.o", "prog"] {
        let a = std::fs::read(gnu_dir.join(artefact)).expect("GNU make produced the artefact");
        let b = std::fs::read(rs_dir.join(artefact)).expect("rsmake produced the artefact");
        assert_eq!(
            a, b,
            "`{artefact}` differs between the two builds; the compiler is \
             deterministic here, so a difference means the recipes were not"
        );
    }

    // The program must actually work, not merely link: util(2)*2 - 4 == 0.
    let status = Command::new(rs_dir.join("prog"))
        .status()
        .expect("run the built program");
    assert!(status.success(), "the linked program returned {status}");

    let _ = std::fs::remove_dir_all(&gnu_dir);
    let _ = std::fs::remove_dir_all(&rs_dir);
}

/// Both programs name themselves in their reports, and that is the one
/// difference this comparison must not flag.
fn strip_program_name(s: &str) -> String {
    s.replace("rsmake: ", "MAKE: ").replace("make: ", "MAKE: ")
}

#[test]
fn incremental_rebuilds_match_gnu_make() {
    require_cc();
    let gnu_dir = lay_out("incr-gnu");
    let rs_dir = lay_out("incr-rs");

    for (dir, prog) in [(&gnu_dir, "make"), (&rs_dir, env!("CARGO_BIN_EXE_rsmake"))] {
        let (code, log) = build_with(prog, dir, &[]);
        assert_eq!(code, 0, "first build failed:\n{log}");
    }

    // Second build: whatever GNU make decides to do, rsmake must decide too.
    // Asserting a literal message here instead would encode this author's
    // guess about GNU's wording rather than GNU's actual behaviour.
    let (_, gnu_second) = build_with("make", &gnu_dir, &[]);
    let (_, rs_second) = build_with(env!("CARGO_BIN_EXE_rsmake"), &rs_dir, &[]);
    assert_eq!(
        strip_program_name(&gnu_second),
        strip_program_name(&rs_second),
        "an up-to-date rebuild must behave the same; equal timestamps mean done"
    );
    assert!(
        !rs_second.contains("-c main.c"),
        "nothing should have been recompiled:\n{rs_second}"
    );

    // Touching a header must reach every object that depends on it. The sleep
    // is deliberate: with sub-second resolution an immediate write can land in
    // the same timestamp as the object and legitimately count as not newer.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for dir in [&gnu_dir, &rs_dir] {
        std::fs::write(dir.join("util.h"), "int util(int); /* touched */\n").expect("touch header");
    }
    let (_, gnu_third) = build_with("make", &gnu_dir, &[]);
    let (_, rs_third) = build_with(env!("CARGO_BIN_EXE_rsmake"), &rs_dir, &[]);
    assert_eq!(
        strip_program_name(&gnu_third),
        strip_program_name(&rs_third),
        "a touched header must trigger the same rebuilds"
    );
    assert!(
        rs_third.contains("-c main.c"),
        "main.o depends on util.h:\n{rs_third}"
    );
    assert!(
        rs_third.contains("-c util.c"),
        "util.o depends on util.h:\n{rs_third}"
    );

    let _ = std::fs::remove_dir_all(&gnu_dir);
    let _ = std::fs::remove_dir_all(&rs_dir);
}

#[test]
fn parallel_and_serial_builds_agree() {
    require_cc();
    let serial = lay_out("serial");
    let parallel = lay_out("parallel");

    let (c1, l1) = build_with(env!("CARGO_BIN_EXE_rsmake"), &serial, &[]);
    assert_eq!(c1, 0, "{l1}");
    let (c2, l2) = build_with(env!("CARGO_BIN_EXE_rsmake"), &parallel, &["-j4"]);
    assert_eq!(c2, 0, "{l2}");

    for artefact in ["main.o", "util.o", "prog"] {
        let a = std::fs::read(serial.join(artefact)).expect("serial artefact");
        let b = std::fs::read(parallel.join(artefact)).expect("parallel artefact");
        assert_eq!(
            a, b,
            "`{artefact}` differs between a serial and a -j4 build"
        );
    }

    let _ = std::fs::remove_dir_all(&serial);
    let _ = std::fs::remove_dir_all(&parallel);
}

#[test]
fn parallel_output_is_not_interleaved() {
    // Under -j each job's output is captured and flushed whole. Without that,
    // two concurrent recipes tear each other's lines in half and the log
    // becomes unreadable exactly when a build is failing.
    let dir = std::env::temp_dir().join(format!("rsmake-interleave-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create directory");
    let mut mk = String::from(".PHONY: all\nall:");
    for i in 0..8 {
        mk.push_str(&format!(" t{i}"));
    }
    mk.push('\n');
    for i in 0..8 {
        // Each job writes its marker in several bursts with pauses between,
        // which is what tears if output is not captured per job.
        mk.push_str(&format!(
            ".PHONY: t{i}\nt{i}:\n\t@for n in 1 2 3 4 5; do printf 'job{i} line %s\\n' $$n; done\n"
        ));
    }
    std::fs::write(dir.join("Makefile"), mk).expect("write makefile");

    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-j8"]);
    assert_eq!(code, 0, "{log}");

    for i in 0..8 {
        let lines: Vec<usize> = log
            .lines()
            .enumerate()
            .filter(|(_, l)| l.starts_with(&format!("job{i} line ")))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(lines.len(), 5, "job{i} lost output:\n{log}");
        let contiguous = lines.windows(2).all(|w| w[1] == w[0] + 1);
        assert!(
            contiguous,
            "job{i}'s output was interleaved with another job's:\n{log}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn realpath_canonicalises_existing_abspath_resolves_rest() {
    // `realpath` resolves only what exists (so it is dry-run compatible with
    // the differential corpus's private cwds), while `abspath` is textual and
    // works on names that do not exist yet. Run on a real tree so the paths
    // are absolute and deterministic.
    let dir = std::env::temp_dir().join(format!("rsmake-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create path dir");
    std::fs::write(dir.join("real.f"), "x").expect("write fixture");
    std::fs::write(
        dir.join("Makefile"),
        "all:\n\techo '$(realpath ./real.f)'\n\techo '$(abspath ./missing.o)'\n",
    )
    .expect("write makefile");

    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-n"]);
    assert_eq!(code, 0, "{log}");
    // Expanded `-n` output is only the recipe lines, so realpath's absolute
    // path is embedded in the emitted echo. It must be absolute (start /).
    assert!(
        log.contains("real.f"),
        "realpath must resolve an existing file:\n{log}"
    );
    assert!(
        !log.contains("echo ''"),
        "realpath of an existing file must not be empty:\n{log}"
    );
    assert!(
        log.lines().any(|l| {
            l.contains("missing.o") && l.starts_with("echo '/")
        }),
        "abspath must resolve a non-existent name to an absolute path:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
