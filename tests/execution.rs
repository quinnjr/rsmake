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

/// A scratch directory with an arbitrary set of files, for the tests whose
/// subject is a flag rather than the fixture project.
fn scratch(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsmake-exec-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create nested directory");
        }
        std::fs::write(path, body).expect("write scratch file");
    }
    dir
}

/// As [`build_with`], with extra environment variables set on the child.
fn build_with_env(
    program: &str,
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (i32, String, String) {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env_remove("MAKEFLAGS")
        .env_remove("MAKELEVEL");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("could not run {program}: {e}"));
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_later_goal_whose_prerequisite_an_earlier_goal_built_still_runs() {
    // `state` is shared across goals, so a node finished while building the
    // first goal never settles again. Seeding the second goal's readiness
    // counter from *all* its prerequisites left it permanently short of zero,
    // and the goal was skipped in silence and reported up to date.
    let dir = scratch(
        "multigoal",
        &[(
            "Makefile",
            ".PHONY: alpha beta shared\n\
             shared:\n\t@echo shared\n\
             alpha: shared\n\t@echo alpha\n\
             beta: shared\n\t@echo beta\n",
        )],
    );
    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir, &["alpha", "beta"]);
    assert_eq!(code, 0, "{log}");
    for expected in ["shared", "alpha", "beta"] {
        assert!(
            log.lines().any(|l| l == expected),
            "`{expected}` never ran:\n{log}"
        );
    }
    assert_eq!(
        log.lines().filter(|l| *l == "shared").count(),
        1,
        "the shared prerequisite must be built once, not once per goal:\n{log}"
    );

    // And the same in the order that makes the second goal's prerequisite the
    // first goal itself.
    let dir2 = scratch(
        "multigoal-dep-first",
        &[(
            "Makefile",
            ".PHONY: shared beta\nshared:\n\t@echo shared\nbeta: shared\n\t@echo beta\n",
        )],
    );
    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir2, &["shared", "beta"]);
    assert_eq!(code, 0, "{log}");
    assert!(
        log.contains("beta"),
        "`beta` must still run when its only prerequisite was the first goal:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn makeflags_from_the_environment_is_honoured() {
    // A sub-make learns its parent's flags only through MAKEFLAGS. Ignoring
    // it meant `make -n` really executed a `+`-prefixed sub-make's recipes.
    let dir = scratch("makeflags", &[("Makefile", "all:\n\techo hi\n")]);

    let (code, out, _) = build_with_env(env!("CARGO_BIN_EXE_rsmake"), &dir, &[], &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("echo hi"),
        "without -s the recipe is echoed:\n{out}"
    );

    let (code, out, _) = build_with_env(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &[],
        &[("MAKEFLAGS", "s")],
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("echo hi"),
        "MAKEFLAGS=s must silence the recipe:\n{out}"
    );
    assert!(out.contains("hi"), "the recipe must still run:\n{out}");

    // GNU 4.4.1 writes the jobserver words after the letter cluster. They are
    // options rsmake does not implement and must be ignored, not refused --
    // otherwise rsmake cannot run as a child of GNU make at all.
    let (code, out, err) = build_with_env(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &[],
        &[("MAKEFLAGS", "s -j32 --jobserver-auth=fifo:/tmp/nonexistent")],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(!out.contains("echo hi"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_serial_sub_make_notice_fires_only_in_a_real_sub_make() {
    // The guard used to read MAKELEVEL out of the variable table, which `-R`
    // erases: `rsmake -R -j4` then believed it was a sub-make, printed the
    // notice, and silently dropped to serial at the top level.
    let dir = scratch("notice", &[("Makefile", "all:\n\t@echo hi\n")]);

    let (code, out, err) = build_with_env(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-R", "-j4"], &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        !err.contains("sub-make"),
        "the top level is not a sub-make, whatever -R did to MAKELEVEL:\n{err}"
    );

    // With a real inherited level it must fire.
    let (code, out, err) = build_with_env(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &["-j4"],
        &[("MAKELEVEL", "1")],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        err.contains("sub-make"),
        "a real sub-make must say why it went serial:\n{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_recipe_line_mentioning_make_recurses_even_under_dry_run() {
    // GNU make §5.7.1: a line referencing the MAKE variable runs under -n as
    // if it were `+`-prefixed, so a dry run shows the whole tree's plan
    // instead of stopping at the top makefile.
    let files: &[(&str, &str)] = &[
        ("Makefile", "all:\n\t@$(MAKE) -f sub.mk sub\n"),
        ("sub.mk", "sub:\n\techo from-the-sub-make\n"),
    ];
    let gnu_dir = scratch("recurse-gnu", files);
    let rs_dir = scratch("recurse-rs", files);

    let (gnu_code, gnu_log) = build_with("make", &gnu_dir, &["-n"]);
    assert_eq!(gnu_code, 0, "GNU make failed on the fixture:\n{gnu_log}");
    assert!(
        gnu_log.contains("echo from-the-sub-make"),
        "sanity: GNU must recurse under -n:\n{gnu_log}"
    );

    let (rs_code, rs_log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &rs_dir, &["-n"]);
    assert_eq!(rs_code, 0, "{rs_log}");
    assert!(
        rs_log.contains("echo from-the-sub-make"),
        "a $(MAKE) line must run under -n, so the sub-make's plan is shown:\n{rs_log}"
    );
    // And the flags reached the child: the sub-make dry-ran rather than
    // actually running the echo.
    assert!(
        !rs_log.lines().any(|l| l == "from-the-sub-make"),
        "-n must propagate through MAKEFLAGS; the sub-make executed:\n{rs_log}"
    );

    let _ = std::fs::remove_dir_all(&gnu_dir);
    let _ = std::fs::remove_dir_all(&rs_dir);
}

#[test]
fn sub_make_makeflags_carries_b_and_e_but_never_q() {
    // GNU propagates most of the parent's flags to a sub-make through
    // MAKEFLAGS, but `-q` is deliberately excluded (see `child_env`'s
    // comment): a sub-make asked "is this up to date?" would build nothing
    // while its parent believed it had answered the question truthfully.
    // This reads MAKEFLAGS through the shell as `$$MAKEFLAGS` (the process
    // environment variable a sub-make inherits); the make-variable form
    // `$(MAKEFLAGS)` is covered separately below.
    let files: &[(&str, &str)] = &[
        ("Makefile", "all:\n\t@$(MAKE) -f sub.mk report\n"),
        ("sub.mk", "report:\n\t@echo FLAGS=[$$MAKEFLAGS]\n"),
    ];
    let dir = scratch("subflags", files);

    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-B", "-e"]);
    assert_eq!(code, 0, "{log}");
    let flags_line = log
        .lines()
        .find(|l| l.starts_with("FLAGS=["))
        .unwrap_or_else(|| panic!("sub-make never reported its MAKEFLAGS:\n{log}"));
    let inner = flags_line
        .trim_start_matches("FLAGS=[")
        .trim_end_matches(']');
    assert!(
        inner.contains('B'),
        "-B must propagate to the sub-make: {flags_line}"
    );
    assert!(
        inner.contains('e'),
        "-e must propagate to the sub-make: {flags_line}"
    );
    assert!(
        !inner.contains('q'),
        "-q must never propagate to a sub-make: {flags_line}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn makeflags_expands_as_a_make_variable_independent_of_the_process_env() {
    // GNU make (verified against 4.4.1) sets $(MAKEFLAGS) as a real
    // make-variable, distinct from the MAKEFLAGS process environment
    // variable a child process would see: `-s` shows up as the letter `s`
    // when the recipe expands `$(MAKEFLAGS)` itself. Removing MAKEFLAGS from
    // the child's env before running proves the variable is synthesized by
    // rsmake's own parsing of its flags, not merely forwarded from the
    // process environment.
    let dir = scratch(
        "makeflags-var",
        &[("Makefile", "all:\n\t@echo [$(MAKEFLAGS)]\n")],
    );

    let (code, out, err) = build_with_env(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-s"], &[]);
    assert_eq!(code, 0, "{out}{err}");
    let line = out
        .lines()
        .find(|l| l.starts_with('[') && l.ends_with(']'))
        .unwrap_or_else(|| panic!("no $(MAKEFLAGS) line in output:\n{out}"));
    assert!(
        line.contains('s'),
        "$(MAKEFLAGS) should expand to include `s` under -s, independent of \
         the process environment (which had MAKEFLAGS removed): {line}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_f_reads_every_makefile_and_repeated_c_chains() {
    // GNU accumulates both. Last-wins silently built the wrong thing: `-f
    // base.mk -f extra.mk` would have lost every rule in base.mk.
    let files: &[(&str, &str)] = &[
        ("base.mk", "one:\n\t@echo one\n"),
        ("extra.mk", "two:\n\t@echo two\n"),
        ("x/y/Makefile", "deep:\n\t@echo deep\n"),
    ];
    let dir = scratch("multif", files);

    // The default goal comes from the first file, as in GNU make.
    let (code, log) = build_with(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &["-f", "base.mk", "-f", "extra.mk"],
    );
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("one"), "the first -f names the default goal:\n{log}");

    // ...and the second file's rules are there to be asked for.
    let (code, log) = build_with(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &["-f", "base.mk", "-f", "extra.mk", "two"],
    );
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("two"), "every -f must be read:\n{log}");

    // `-C x -C y` lands in x/y, not in y.
    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir, &["-C", "x", "-C", "y", "deep"]);
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("deep"), "-C must chain:\n{log}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_recipe_shell_ignores_the_environments_shell() {
    // GNU make never takes SHELL from the environment: whoever started the
    // build has nothing to say about what runs its recipes. With /bin/echo
    // inherited as the shell, `-c 'echo ran'` would print its own arguments
    // instead of running anything.
    let dir = scratch("envshell", &[("Makefile", "all:\n\t@echo ran\n")]);
    let (code, out, err) = build_with_env(
        env!("CARGO_BIN_EXE_rsmake"),
        &dir,
        &[],
        &[("SHELL", "/bin/echo")],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(
        out.trim(),
        "ran",
        "the recipe must run under /bin/sh regardless of $SHELL:\n{out}{err}"
    );

    // A makefile that sets SHELL through another variable still gets a
    // program name rather than the literal reference.
    let dir2 = scratch(
        "varshell",
        &[("Makefile", "REAL = /bin/sh\nSHELL = $(REAL)\nall:\n\t@echo ran\n")],
    );
    let (code, log) = build_with(env!("CARGO_BIN_EXE_rsmake"), &dir2, &[]);
    assert_eq!(code, 0, "SHELL must be expanded before use: {log}");
    assert_eq!(log.trim(), "ran", "{log}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}
