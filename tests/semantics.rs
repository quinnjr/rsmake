//! Expansion semantics, tested by observable difference.
//!
//! Each of these asserts that two things *disagree* in the way the design
//! requires. A test that merely exercises the expander would pass against an
//! implementation that expanded everything eagerly, which is the exact bug
//! these rules exist to prevent.

use rsmake::{AutoVars, Engine, Opts};

fn parse(mk: &str) -> Engine {
    let mut e = Engine::new(Opts::default());
    e.parse_text(mk, "<test>").expect("makefile parses");
    e
}

/// Expand one makefile expression. Not a language `eval`: it runs rsmake's own
/// expander over makefile text, with no interpreter and no dynamic code path.
fn eval(mk: &str, expr: &str) -> String {
    let mut e = parse(mk);
    e.expand(expr, &AutoVars::default())
        .expect("expression expands")
}

/// A path no other test will collide on.
fn scratch(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("rsmake-sem-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn recursive_assignment_reruns_shell_at_every_reference() {
    let counter = scratch("recursive");
    let mk = format!(
        "LAZY = $(shell echo tick >> {} ; echo v)\n",
        counter.display()
    );
    let mut e = parse(&mk);
    for _ in 0..3 {
        assert_eq!(
            e.expand("$(LAZY)", &AutoVars::default()).expect("expand"),
            "v"
        );
    }
    let ticks = std::fs::read_to_string(&counter)
        .expect("counter written")
        .lines()
        .count();
    let _ = std::fs::remove_file(&counter);
    assert_eq!(
        ticks, 3,
        "a `=` variable must re-run its $(shell) at every reference; \
         running it once would make it indistinguishable from `:=`"
    );
}

#[test]
fn simple_assignment_runs_shell_exactly_once() {
    let counter = scratch("simple");
    let mk = format!(
        "NOW := $(shell echo tick >> {} ; echo v)\n",
        counter.display()
    );
    let mut e = parse(&mk);
    for _ in 0..3 {
        assert_eq!(
            e.expand("$(NOW)", &AutoVars::default()).expect("expand"),
            "v"
        );
    }
    let ticks = std::fs::read_to_string(&counter)
        .expect("counter written")
        .lines()
        .count();
    let _ = std::fs::remove_file(&counter);
    assert_eq!(ticks, 1, "a `:=` variable expands once, at assignment");
}

#[test]
fn dollar_dollar_survives_one_expansion() {
    assert_eq!(eval("", "$$"), "$");
    assert_eq!(eval("", "$$HOME"), "$HOME");
    assert_eq!(eval("V = x", "$$(echo $(V))"), "$(echo x)");
}

#[test]
fn whitespace_inside_function_arguments_is_significant() {
    // GNU make does not strip the space after the comma, and neither may
    // rsmake: stripping it changes the text a recipe passes to the shell.
    assert_eq!(eval("X = set", "[$(if $(X), yes)]"), "[ yes]");
    assert_eq!(eval("", "[$(if $(NOPE), yes, no )]"), "[ no ]");
    assert_eq!(eval("", "[$(strip  a   b  )]"), "[a b]");
}

#[test]
fn append_expands_now_for_simple_and_later_for_recursive() {
    let counter = scratch("append");
    let mk = format!(
        "S := base\nS += $(shell echo tick >> {c} ; echo s)\n\
         R = base\nR += $(shell echo tick >> {c} ; echo r)\n",
        c = counter.display()
    );
    let mut e = parse(&mk);
    // Parsing alone runs the shell once, for the `+=` onto the simple variable.
    let after_parse = std::fs::read_to_string(&counter)
        .map(|s| s.lines().count())
        .unwrap_or(0);
    assert_eq!(
        e.expand("$(S)", &AutoVars::default()).expect("expand"),
        "base s"
    );
    assert_eq!(
        e.expand("$(R)", &AutoVars::default()).expect("expand"),
        "base r"
    );
    let after_use = std::fs::read_to_string(&counter)
        .expect("counter")
        .lines()
        .count();
    let _ = std::fs::remove_file(&counter);
    assert_eq!(
        after_parse, 1,
        "`+=` onto a simple variable expands immediately"
    );
    assert_eq!(
        after_use, 2,
        "`+=` onto a recursive variable must keep the text unexpanded, so the \
         $(shell) runs when the variable is referenced"
    );
}

#[test]
fn foreach_restores_the_loop_variable() {
    // A leaked loop binding is invisible until some later expansion reads the
    // same name and silently gets the last iteration's value.
    assert_eq!(
        eval("v = outer", "$(foreach v,a b,<$(v)>) $(v)"),
        "<a> <b> outer"
    );
    assert_eq!(eval("", "$(foreach u,a b,x) [$(u)]"), "x x []");
}

#[test]
fn computed_variable_names_resolve() {
    assert_eq!(eval("A = B\nB = hit", "$($(A))"), "hit");
    assert_eq!(eval("PRE_x = hit\nN = x", "$(PRE_$(N))"), "hit");
}

#[test]
fn substitution_references_distinguish_suffix_from_pattern() {
    let mk = "SRC = a.c b.c sub/c.c";
    assert_eq!(eval(mk, "$(SRC:.c=.o)"), "a.o b.o sub/c.o");
    assert_eq!(eval(mk, "$(SRC:%.c=%.o)"), "a.o b.o sub/c.o");
    assert_eq!(eval(mk, "$(SRC:=.bak)"), "a.c.bak b.c.bak sub/c.c.bak");
}

#[test]
fn suffix_is_taken_from_the_final_path_component() {
    // `dir.d/x` has no suffix; treating the directory's dot as one makes
    // $(basename) truncate the directory name.
    assert_eq!(
        eval("L = dir.d/x", "[$(suffix $(L))][$(basename $(L))]"),
        "[][dir.d/x]"
    );
}

#[test]
fn conditional_branches_that_are_not_taken_are_not_expanded() {
    let counter = scratch("branch");
    let mk = format!(
        "X =\nRESULT = $(if $(X),$(shell echo tick >> {} ; echo taken),untaken)\n",
        counter.display()
    );
    let mut e = parse(&mk);
    assert_eq!(
        e.expand("$(RESULT)", &AutoVars::default()).expect("expand"),
        "untaken"
    );
    let ran = counter.exists();
    let _ = std::fs::remove_file(&counter);
    assert!(!ran, "the untaken branch of $(if) must not be expanded");
}

#[test]
fn self_referential_variable_is_reported_with_its_chain() {
    let mut e = parse("A = $(B)\nB = $(A)\n");
    let err = e
        .expand("$(A)", &AutoVars::default())
        .expect_err("must not silently return empty");
    let msg = err.to_string();
    assert!(
        msg.contains("references itself"),
        "a variable cycle must be named, not truncated into an empty string: {msg}"
    );
    assert!(
        msg.contains('A') && msg.contains('B'),
        "the chain must name both links: {msg}"
    );
}

#[test]
fn fixed_arity_functions_absorb_trailing_commas() {
    assert_eq!(eval("", "$(subst a,b,c,d)"), "c,d");
    assert_eq!(eval("", "$(subst a,b,xaxax)"), "xbxbx");
}

#[test]
fn unknown_function_names_are_ordinary_variable_references() {
    // GNU make treats `$(nosuchfn a,b)` as a reference to a variable of that
    // name, which is undefined, so it expands to nothing. rsmake matches:
    // erroring here would break conformance on every makefile that has ever
    // referenced a variable whose name contains a space.
    assert_eq!(eval("", "[$(nosuchfn a,b)]"), "[]");
}
