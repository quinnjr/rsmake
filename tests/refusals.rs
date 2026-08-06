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
    let dir = std::env::temp_dir().join(format!("rsmake-refuse-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    let mut f = std::fs::File::create(dir.join("Makefile")).expect("write makefile");
    f.write_all(makefile.as_bytes())
        .expect("write makefile body");
    drop(f);

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

    let soft = run("failsoft", "all:\n\t-false\n\techo after\n", &[]);
    assert_eq!(
        soft.code, 0,
        "a `-` prefixed failure is ignored: {}",
        soft.stderr
    );
    assert!(
        soft.stdout.contains("after"),
        "the recipe must continue: {}",
        soft.stdout
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
fn subst_empty_appends_once() {
    let r = run("subst-empty", "all:\n\t@echo '$(subst ,X,ab)'\n", &["-n"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("abX"),
        "{}",
        r.stdout
    );
}

#[test]
fn realpath_resolves_and_requires_existence() {
    // `realpath` canonicalises only paths that exist; `abspath` is textual so
    // it works on names that do not yet exist. The scratch dir is deterministic
    // for this helper, unlike the differential corpus's two private dirs.
    let r = run(
        "realpath",
        "all:\n\t@echo '$(realpath ./Makefile)'\n\t@echo '$(abspath ./missing.o)'\n",
        &["-n"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("Makefile"),
        "realpath keeps existing paths: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("missing.o"),
        "abspath resolves non-existent names textually: {}",
        r.stdout
    );
}
