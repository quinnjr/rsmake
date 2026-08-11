//! Negative direction: what rsmake must refuse.
//!
//! Every test here asserts a *rejection*. A suite that only checks that
//! correct makefiles work would pass against an implementation that accepts
//! everything and quietly does the wrong thing, which is the failure mode the
//! closed dialect exists to prevent.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

struct Run {
    code: i32,
    stderr: String,
    stdout: String,
}

/// Write `makefile` into a fresh directory and run rsmake on it.
fn run(tag: &str, makefile: &str, args: &[&str]) -> Run {
    run_with_files(tag, makefile, &[], args)
}

/// Lay a makefile and any accompanying fixture files out in a fresh scratch
/// directory, and return its path. Shared by every helper that needs a
/// directory on disk before running rsmake in it.
fn lay_out_scratch(tag: &str, makefile: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsmake-refuse-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    let mut f = std::fs::File::create(dir.join("Makefile")).expect("write makefile");
    f.write_all(makefile.as_bytes())
        .expect("write makefile body");
    drop(f);
    for (name, body) in files {
        std::fs::write(dir.join(name), body).expect("write fixture file");
    }
    dir
}

/// As [`run`], plus extra files laid down beside the makefile — a refusal
/// often depends on what is or is not on disk.
fn run_with_files(tag: &str, makefile: &str, files: &[(&str, &str)], args: &[&str]) -> Run {
    let dir = lay_out_scratch(tag, makefile, files);
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_rsmake")))
        .args(args)
        .current_dir(&dir)
        .output()
        .expect("rsmake runs");
    let _ = std::fs::remove_dir_all(&dir);
    Run {
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

/// As [`run_with_files`], but run under a `timeout` wrapper so a hang fails
/// the assertion instead of the test never returning. `secs` is the timeout
/// in seconds; a run that hits it reports exit code 124.
fn run_with_files_timed(
    tag: &str,
    makefile: &str,
    files: &[(&str, &str)],
    args: &[&str],
    secs: &str,
) -> Run {
    let dir = lay_out_scratch(tag, makefile, files);
    let out = Command::new("timeout")
        .arg(secs)
        .arg(PathBuf::from(env!("CARGO_BIN_EXE_rsmake")))
        .args(args)
        .current_dir(&dir)
        .output()
        .expect("timeout runs rsmake");
    let _ = std::fs::remove_dir_all(&dir);
    Run {
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

#[track_caller]
fn assert_refused(r: &Run, needle: &str) {
    assert_eq!(
        r.code, 2,
        "expected exit 2\n--- stderr ---\n{}\n--- stdout ---\n{}",
        r.stderr, r.stdout
    );
    assert!(
        r.stderr.contains(needle),
        "expected the diagnostic to mention {needle:?}\n--- stderr ---\n{}",
        r.stderr
    );
}

#[test]
fn dependency_cycle_is_refused_and_named() {
    let r = run(
        "cycle",
        "a: b\n\techo a\nb: c\n\techo b\nc: a\n\techo c\n",
        &["-n"],
    );
    assert_refused(&r, "dependency cycle");
    // Naming the cycle is the point: "circular dependency" without the path
    // leaves the reader grepping a thousand-line makefile.
    for t in ["a", "b", "c"] {
        assert!(r.stderr.contains(t), "cycle must name `{t}`: {}", r.stderr);
    }
}

#[test]
fn space_indented_recipe_is_refused() {
    // The tab is load-bearing. Accepting spaces here means a makefile that
    // looks right runs nothing.
    let r = run("spaces", "all:\n    echo hi\n", &["-n"]);
    assert_refused(&r, "missing separator");
}

#[test]
fn recipe_before_any_target_is_refused() {
    let r = run("orphan", "\techo hi\nall:\n\techo all\n", &["-n"]);
    assert_refused(&r, "recipe commences before first target");
}

#[test]
fn excluded_special_targets_are_refused_by_name() {
    for (target, needle) in [
        (".ONESHELL", ".ONESHELL"),
        (".SECONDARY", ".SECONDARY"),
        (".SECONDEXPANSION", ".SECONDEXPANSION"),
        (".EXPORT_ALL_VARIABLES", ".EXPORT_ALL_VARIABLES"),
        (".DEFAULT_GOAL", ".DEFAULT_GOAL"),
    ] {
        let mk = format!("{target}:\nall:\n\techo hi\n");
        let r = run(&format!("special{}", &target[1..]), &mk, &["-n"]);
        assert_refused(&r, needle);
        assert!(
            r.stderr.contains("outside the rsmake dialect"),
            "the refusal must say why: {}",
            r.stderr
        );
    }
}

#[test]
fn excluded_directives_are_refused_by_name() {
    for d in [
        "override X = 1",
        "undefine X",
        "private X = 1",
        "load foo.so",
    ] {
        let mk = format!("{d}\nall:\n\techo hi\n");
        let tag = d.split_whitespace().next().expect("directive word");
        let r = run(&format!("dir{tag}"), &mk, &["-n"]);
        assert_refused(&r, tag);
    }
}

#[test]
fn shell_assignment_operator_is_refused() {
    let r = run("bang", "X != echo hi\nall:\n\techo $(X)\n", &["-n"]);
    assert_refused(&r, "!=");
}

#[test]
fn static_pattern_rules_are_refused() {
    let r = run(
        "static",
        "OBJ = a.o\nall: $(OBJ)\n$(OBJ): %.o: %.c\n\techo $<\n",
        &["-n"],
    );
    assert_refused(&r, "static pattern rules");
}

#[test]
fn archive_member_syntax_is_refused() {
    let r = run("archive", "lib.a(x.o): x.o\n\techo ar\n", &["-n"]);
    assert_refused(&r, "archive member");
}

#[test]
fn a_suffix_rule_with_prerequisites_warns_that_they_are_dropped() {
    // A suffix rule's name is the two suffixes; there is nowhere in that
    // syntax for extra prerequisites to live, so GNU (and rsmake) drop them
    // and warn rather than silently keeping a dependency nobody can see.
    let r = run(
        "suffixdrop",
        ".SUFFIXES: .c .o\nall:\n\t@echo hi\n.c.o: extra.h\n\t@echo compile\n",
        &["-n"],
    );
    assert_eq!(r.code, 0, "the build itself is not refused: {}", r.stderr);
    assert!(
        r.stderr
            .contains("ignoring prerequisites on suffix rule definition"),
        "the dropped prerequisite must be warned about: {}",
        r.stderr
    );
}

#[test]
fn a_second_recipe_for_one_target_is_refused() {
    // GNU make warns and silently discards the first recipe. A discarded
    // recipe is invisible, so rsmake stops instead.
    let r = run("dup", "all:\n\techo one\nall:\n\techo two\n", &["-n"]);
    assert_refused(&r, "already has a recipe");
}

#[test]
fn unbalanced_conditionals_are_refused() {
    assert_refused(
        &run("noendif", "ifeq (a,a)\nall:\n\techo hi\n", &["-n"]),
        "endif",
    );
    assert_refused(
        &run("noif", "else\nall:\n\techo hi\n", &["-n"]),
        "`else` without `if`",
    );
    assert_refused(
        &run(
            "twoelse",
            "ifeq (a,b)\nelse\nelse\nendif\nall:\n\techo hi\n",
            &["-n"],
        ),
        "`else` after `else`",
    );
}

#[test]
fn missing_endef_is_refused() {
    let r = run(
        "noendef",
        "define BODY\necho hi\nall:\n\techo all\n",
        &["-n"],
    );
    assert_refused(&r, "endef");
}

#[test]
fn a_missing_include_is_refused_but_a_dash_include_is_not() {
    assert_refused(
        &run("inc", "include nope.mk\nall:\n\techo hi\n", &["-n"]),
        "no such file or directory",
    );
    let r = run("dashinc", "-include nope.mk\nall:\n\techo hi\n", &["-n"]);
    assert_eq!(
        r.code, 0,
        "`-include` must tolerate a missing file: {}",
        r.stderr
    );
}

#[test]
fn a_target_with_no_rule_and_no_file_is_refused() {
    let r = run("norule", "all: ghost.o\n\techo link\n", &["-n"]);
    assert_refused(&r, "no rule to make target");
    assert!(
        r.stderr.contains("needed by"),
        "the refusal must say who wanted it: {}",
        r.stderr
    );
}

#[test]
fn error_function_stops_the_build() {
    let r = run("errfn", "$(error deliberate)\nall:\n\techo hi\n", &["-n"]);
    assert_refused(&r, "deliberate");
    assert!(
        !r.stdout.contains("echo hi"),
        "$(error) must stop before any recipe is emitted: {}",
        r.stdout
    );
}

#[test]
fn a_failing_recipe_fails_the_build_unless_it_is_marked_ignorable() {
    let hard = run("fail", "all:\n\tfalse\n", &[]);
    assert_eq!(
        hard.code, 2,
        "a failing recipe must fail the build: {}",
        hard.stderr
    );
    assert!(
        hard.stderr.contains("Error 1"),
        "report the exit status: {}",
        hard.stderr
    );

    // Ignored is not invisible. GNU reports the failure and carries on, under
    // every job count. Serial (`-j1`) prints it straight to stderr as it
    // happens; under `-j` (capture mode) each job's output -- including this
    // notice -- is buffered and flushed whole with the job's stdout, in
    // order, or it would tear against other jobs' interleaved output.
    {
        let soft = run("failsoft", "all:\n\t-false\n\techo after\n", &["-j1"]);
        assert_eq!(
            soft.code, 0,
            "a `-` prefixed failure is ignored: {}",
            soft.stderr
        );
        assert!(
            soft.stdout.contains("after"),
            "the recipe must continue under -j1: {}",
            soft.stdout
        );
        assert!(
            soft.stderr.contains("(ignored)"),
            "the ignored failure must be reported on stderr under -j1: \
             --- stderr ---\n{}\n--- stdout ---\n{}",
            soft.stderr,
            soft.stdout
        );
        assert!(
            !soft.stdout.contains("(ignored)"),
            "under -j1 the diagnostic belongs on stderr, not the recipe stream: {}",
            soft.stdout
        );
    }
    {
        let soft = run("failsoft4", "all:\n\t-false\n\techo after\n", &["-j4"]);
        assert_eq!(
            soft.code, 0,
            "a `-` prefixed failure is ignored: {}",
            soft.stderr
        );
        assert!(
            soft.stdout.contains("after"),
            "the recipe must continue under -j4: {}",
            soft.stdout
        );
        assert!(
            soft.stdout.contains("(ignored)"),
            "under -j4 the notice is flushed with the job's captured stdout, \
             in order: --- stderr ---\n{}\n--- stdout ---\n{}",
            soft.stderr,
            soft.stdout
        );
        assert!(
            !soft.stderr.contains("(ignored)"),
            "under -j4 the notice belongs in the captured stdout stream, not stderr: {}",
            soft.stderr
        );
    }
}

#[test]
fn a_failure_with_undispatched_siblings_exits_promptly() {
    // The scheduler used to spin at 100% CPU here: once a recipe failed
    // without `-k`, dispatch was blocked but the "nothing in flight" arm kept
    // `continue`ing because sibling targets were still queued. GNU exits 2.
    for jobs in ["-j1", "-j4"] {
        let r = run_with_files_timed(
            &format!("spin{jobs}"),
            ".PHONY: all a b c\nall: a b c\na:\n\t@false\nb:\n\t@echo b\nc:\n\t@echo c\n",
            &[],
            &[jobs],
            "20",
        );
        assert_ne!(
            r.code, 124,
            "rsmake hung under {jobs} instead of stopping at the failure"
        );
        assert_eq!(
            r.code, 2,
            "a failed target must exit 2 under {jobs}: {}",
            r.stderr
        );
    }
}

#[test]
fn keep_going_reports_which_goal_was_abandoned() {
    // Under `-k` the build does not stop at the failure, so without this
    // summary the reader has to work out which goal was given up on from a
    // scroll of interleaved errors. GNU prints it to stderr before exiting 2.
    let r = run("knotremade", "all: a b\na:\n\t@false\nb:\n\t@echo b\n", &["-k"]);
    assert_eq!(r.code, 2, "a failure still fails the build under -k");
    assert!(
        r.stderr
            .contains("Target 'all' not remade because of errors."),
        "-k must name the abandoned goal: {}",
        r.stderr
    );
    assert!(
        r.stdout.contains('b'),
        "-k must have built the sibling anyway: {}",
        r.stdout
    );
}

#[test]
fn keep_going_with_multiple_goals_names_only_the_blocked_one_after_the_error() {
    // Verified against GNU make 4.4.1: with two goals under -k, the goal
    // blocked by a failed prerequisite gets the "not remade" summary and it
    // is printed after the `*** [` error line; a sibling goal that succeeded
    // still runs and is reported.
    let r = run_with_files(
        "kmultigoal",
        "broken: failing-dep\n\t@echo broken-ran\nfailing-dep:\n\t@echo dep-ran; false\nok:\n\t@echo ok-goal-ran\n",
        &[],
        &["-k", "broken", "ok"],
    );
    assert_eq!(r.code, 2, "a failure still fails the build under -k: {}", r.stderr);
    assert!(
        r.stdout.contains("ok-goal-ran"),
        "the sibling goal must still run under -k: {}",
        r.stdout
    );
    let summary = "Target 'broken' not remade because of errors.";
    assert!(
        r.stderr.contains(summary),
        "the summary must name the goal blocked by its failed prerequisite: {}",
        r.stderr
    );
    let error_idx = r
        .stderr
        .find("*** [")
        .unwrap_or_else(|| panic!("no `*** [` error line: {}", r.stderr));
    let summary_idx = r.stderr.find(summary).unwrap();
    assert!(
        summary_idx > error_idx,
        "GNU prints the summary after the error line: {}",
        r.stderr
    );
}

#[test]
fn keep_going_prints_no_summary_for_a_goal_whose_own_recipe_fails() {
    // Verified against GNU make 4.4.1: the "not remade because of errors"
    // summary only fires for a goal blocked by a *prerequisite's* failure,
    // never for a goal whose own recipe is what failed.
    let r = run_with_files(
        "kownfail",
        "broken:\n\t@echo broken-ran; false\nok:\n\t@echo ok-goal-ran\n",
        &[],
        &["-k", "broken", "ok"],
    );
    assert_eq!(r.code, 2, "a failure still fails the build under -k: {}", r.stderr);
    assert!(
        r.stdout.contains("ok-goal-ran"),
        "the sibling goal must still run under -k: {}",
        r.stdout
    );
    assert!(
        !r.stderr.contains("not remade because of errors"),
        "a goal whose own recipe failed must not also get the summary line: {}",
        r.stderr
    );
}

#[test]
fn no_builtin_variables_also_disables_builtin_rules() {
    // GNU: --no-builtin-variables implies --no-builtin-rules. Without that,
    // the built-in `%.o: %.c` rule survives while `COMPILE.c` is empty, the
    // recipe collapses to the bare source file name, and rsmake hands `foo.c`
    // to the shell as a command.
    let r = run_with_files(
        "capitalr",
        "all: foo.o\n",
        &[("foo.c", "int main(void) { return 0; }\n")],
        &["-R", "-n"],
    );
    assert_refused(&r, "no rule to make target");
    assert!(
        !r.stdout.contains("foo.c"),
        "-R must not emit a recipe built from an empty COMPILE.c: {}",
        r.stdout
    );
}

#[test]
fn a_recipe_killed_by_a_signal_reports_the_signal_by_name() {
    // A child that dies by signal has no exit code. GNU make names the
    // terminating signal; a bare `-1` would hide it.
    let r = run("signal", "all:\n\t@sh -c 'kill -TERM $$$$'\n", &[]);
    assert_eq!(r.code, 2, "a signalled recipe must fail: {}", r.stderr);
    assert!(
        r.stderr.contains("Terminated"),
        "name the terminating signal: {}",
        r.stderr
    );
}

#[test]
fn unbounded_parallelism_is_refused() {
    // `-j` with no count is how a makefile takes a build host down.
    let r = run("jbare", "all:\n\techo hi\n", &["-j"]);
    assert_eq!(r.code, 2, "bare -j must be refused: {}", r.stderr);
    assert!(r.stderr.contains("job count"), "{}", r.stderr);
}

#[test]
fn question_mode_reports_staleness_without_building() {
    let r = run("question", "all:\n\techo hi\n", &["-q"]);
    assert_eq!(r.code, 1, "a phony target is always out of date under -q");
    assert!(
        !r.stdout.contains("hi"),
        "-q must not run anything: {}",
        r.stdout
    );
}

#[test]
fn word_index_zero_is_refused() {
    // GNU make stops on `$(word 0,...)`; silently returning the empty string
    // would let a makefile that means to pick a word build nothing instead.
    let r = run("word0", "all:\n\t@echo '$(word 0,a b)'\n", &["-n"]);
    assert_refused(&r, "word");
    assert!(r.stderr.contains("greater than 0"), "{}", r.stderr);
}

#[test]
fn wordlist_zero_start_is_refused() {
    let r = run("wordlist0", "all:\n\t@echo '$(wordlist 0,2,a b c)'\n", &["-n"]);
    assert_refused(&r, "wordlist");
    assert!(r.stderr.contains("'0'"), "{}", r.stderr);
}

#[test]
fn a_bad_index_names_the_argument_it_came_from() {
    // GNU make: "invalid second argument to 'wordlist' function: 'x'". Naming
    // it as the first sends the reader to the wrong end of the call.
    let r = run(
        "wordlist2nd",
        "all:\n\t@echo '$(wordlist 1,x,a b c)'\n",
        &["-n"],
    );
    assert_refused(&r, "invalid second argument to 'wordlist' function: 'x'");

    let r = run(
        "wordlist1st",
        "all:\n\t@echo '$(wordlist x,2,a b c)'\n",
        &["-n"],
    );
    assert_refused(&r, "invalid first argument to 'wordlist' function: 'x'");

    let r = run("word1st", "all:\n\t@echo '$(word x,a b)'\n", &["-n"]);
    assert_refused(&r, "invalid first argument to 'word' function: 'x'");
}

#[test]
fn real_but_unimplemented_functions_are_refused_by_name() {
    // These are functions GNU make evaluates. Falling through to a variable
    // lookup would expand them to nothing, which is the silent wrong answer
    // the closed dialect exists to prevent -- and unlike a genuinely unknown
    // name, there is a real construct here to name.
    for (call, needle) in [
        ("$(value V)", "value"),
        ("$(file > out.txt,x)", "file"),
        ("$(intcmp 1,2,a,b,c)", "intcmp"),
        ("$(let x,1,$(x))", "let"),
        ("$(guile (+ 1 2))", "guile"),
    ] {
        let mk = format!("V = x\nall:\n\t@echo '{call}'\n");
        let r = run(&format!("fn{needle}"), &mk, &["-n"]);
        assert_refused(&r, needle);
        assert!(
            r.stderr.contains("outside the rsmake dialect"),
            "the refusal must say why: {}",
            r.stderr
        );
    }
}

#[test]
fn a_target_specific_conditional_assignment_is_refused() {
    // GNU keeps the existing value for `target: V ?= x`. rsmake's overlay is
    // applied when the target is built, far from where "only if unset" would
    // have to be decided, so behaving like `=` would overwrite what GNU
    // preserves. Refuse by name rather than diverge silently.
    let r = run(
        "tvarcond",
        "V = orig\nall: V ?= over\nall:\n\t@echo $(V)\n",
        &["-n"],
    );
    assert_refused(&r, "?=");
    assert!(
        r.stderr.contains("outside the rsmake dialect"),
        "the refusal must say why: {}",
        r.stderr
    );
}

#[test]
fn a_recipe_on_a_settings_target_is_refused() {
    // `.PHONY` and friends carry settings, not commands. GNU make drops a
    // recipe written on one without a word; a dropped recipe is invisible.
    for t in [
        ".PHONY",
        ".PRECIOUS",
        ".SILENT",
        ".IGNORE",
        ".DELETE_ON_ERROR",
        ".NOTPARALLEL",
        ".POSIX",
        ".SUFFIXES",
    ] {
        let mk = format!("{t}: all\n\techo dropped\nall:\n\t@echo hi\n");
        let r = run(&format!("special-recipe{}", &t[1..]), &mk, &["-n"]);
        assert_refused(&r, t);
        assert!(
            r.stderr.contains("takes no recipe"),
            "the refusal must say why: {}",
            r.stderr
        );
    }
    // `.DEFAULT` is the exception: a recipe is the whole content of that rule.
    let r = run(
        "defaultrecipe",
        ".DEFAULT:\n\t@echo made $@\nall: ghost\n",
        &["-n"],
    );
    assert_eq!(r.code, 0, "`.DEFAULT` takes a recipe: {}", r.stderr);
}

#[test]
fn an_include_cycle_is_refused_rather_than_overflowing_the_stack() {
    // A makefile that includes itself recurses until the stack runs out, and a
    // stack overflow names nothing. (Real GNU make dumps core on this.)
    let r = run(
        "selfinclude",
        "include Makefile\nall:\n\t@echo hi\n",
        &["-n"],
    );
    assert_refused(&r, "include cycle");
    assert!(
        r.stderr.contains("Makefile"),
        "the cycle must name the file: {}",
        r.stderr
    );
}

#[test]
fn a_long_ring_of_includes_is_refused_cleanly() {
    // A cycle detector that only handles the trivial two- or three-file case
    // (e.g. a fixed-depth check) can crash or hang once the ring is longer
    // than whatever it was tuned for. Fifty files is enough to expose that
    // without makefiles that hang or blow the stack.
    const N: usize = 50;
    let mut files: Vec<(String, String)> = Vec::new();
    for i in 1..=N {
        let next = if i == N { 1 } else { i + 1 };
        files.push((format!("a{i}.mk"), format!("include a{next}.mk\n")));
    }
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();

    let r = run_with_files_timed(
        "longring",
        "include a1.mk\nall:\n\t@echo hi\n",
        &file_refs,
        &["-n"],
        "20",
    );
    assert_ne!(
        r.code, 124,
        "rsmake hung on a long include ring instead of detecting the cycle"
    );
    assert_eq!(
        r.code, 2,
        "expected a clean refusal (exit 2), not a crash: {}",
        r.stderr
    );
    assert!(r.stderr.contains("include cycle"), "{}", r.stderr);
}

#[test]
fn a_ring_of_includes_is_refused() {
    let r = run_with_files(
        "incring",
        "include a.mk\nall:\n\t@echo hi\n",
        &[("a.mk", "include b.mk\n"), ("b.mk", "include a.mk\n")],
        &["-n"],
    );
    assert_eq!(r.code, 2, "expected exit 2: {}", r.stderr);
    assert!(r.stderr.contains("include cycle"), "{}", r.stderr);
    assert!(
        r.stderr.contains("a.mk") && r.stderr.contains("b.mk"),
        "the cycle must name both files: {}",
        r.stderr
    );
}
