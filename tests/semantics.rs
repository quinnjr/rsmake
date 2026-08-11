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

#[test]
fn a_function_name_with_no_argument_list_is_a_variable() {
    // GNU make recognises a function only when its name is followed by
    // whitespace. `$(value)` is a reference to a variable called `value`, not
    // a zero-argument call -- which is why the refusal of `$(value ...)` must
    // not fire here.
    assert_eq!(eval("value = hi", "[$(value)]"), "[hi]");
    assert_eq!(eval("words = ww", "[$(words)]"), "[ww]");
    assert_eq!(eval("", "[$(shell)]"), "[]");
    // The real calls are unaffected.
    assert_eq!(eval("", "[$(words a b)]"), "[2]");
    assert_eq!(eval("", "[$(strip  a  b )]"), "[a b]");
}

#[test]
fn subst_empty_search_appends_replacement_once() {
    // GNU make: `$(subst ,X,ab)` is `abX`, not `XaXbX`. Matching Rust's
    // `str::replace` on an empty needle would insert at every boundary.
    assert_eq!(eval("", "$(subst ,X,ab)"), "abX");
    assert_eq!(eval("", "$(subst ,X,)"), "X");
    assert_eq!(eval("", "$(subst ,,ab)"), "ab");
}

#[test]
fn word_indices_after_the_zero_guard_still_work() {
    // The `word 0` refusal added a guard; non-zero indices must be unaffected.
    assert_eq!(eval("", "$(word 1,a b)"), "a");
    assert_eq!(eval("", "$(word 2,a b)"), "b");
}

#[test]
fn a_nested_call_does_not_inherit_the_outer_calls_positionals() {
    // GNU make pushes a fresh scope per `$(call)`: a positional the inner call
    // was not given is empty, not the outer call's. Saving only the arguments
    // this call supplies leaves `$(2)` visible from one frame up, which is a
    // wrong answer that looks like a plausible one.
    assert_eq!(
        eval("f = [$(1)][$(2)]\ng = $(call f,inner)", "$(call g,X,Y)"),
        "[inner][]"
    );
    // The outer frame is restored intact once the inner call returns.
    assert_eq!(
        eval(
            "f = [$(1)][$(2)]\ng = $(call f,inner)$(1)$(2)",
            "$(call g,X,Y)"
        ),
        "[inner][]XY"
    );
}

#[test]
fn a_call_shadows_positionals_but_not_a_makefile_variable_named_with_digits() {
    // Measured against GNU make 4.4.1: with a global `2 = globaltwo`,
    // `$(call f,onearg)` where `f = [$(1)][$(2)]` is `[onearg][globaltwo]`.
    // The fresh scope hides the *caller's positionals*, not every variable
    // whose name happens to be a number -- blanking those is a wrong answer
    // indistinguishable from an unsupplied argument.
    assert_eq!(
        eval("f = [$(1)][$(2)]\n2 = globaltwo", "$(call f,onearg)"),
        "[onearg][globaltwo]"
    );
    // And it is restored, not consumed, when the call returns.
    assert_eq!(
        eval("f = [$(1)][$(2)]\n2 = globaltwo", "$(call f,onearg)$(2)"),
        "[onearg][globaltwo]globaltwo"
    );
}

#[test]
fn the_whole_run_of_space_after_a_function_name_is_skipped() {
    // GNU make 4.4.1: `$(subst  a,b,a a)` (two spaces) is `b b`. Consuming a
    // single byte would leave the extra space inside the first argument, so
    // the search text would be " a" and only the second `a` would match.
    assert_eq!(eval("", "$(subst  a,b,a a)"), "b b");
    assert_eq!(eval("", "$(words   a b  c)"), "3");
    // A multi-byte space is whitespace too. Slicing one byte past it used to
    // panic on a character boundary; the reference is a plain (undefined)
    // variable name, so it expands to nothing.
    assert_eq!(eval("", "[$(subst\u{a0}a,b,a a)]"), "[]");
}

#[test]
fn conditions_are_stripped_but_branches_are_not() {
    // GNU make strips the condition of `$(if)` and every argument of
    // `$(and)`/`$(or)` before testing it, so a space after a variable that
    // expanded to nothing does not make the condition true. The branches keep
    // their whitespace -- see `whitespace_inside_function_arguments_is_significant`.
    assert_eq!(eval("S =", "[$(if $(S) ,yes,no)]"), "[no]");
    assert_eq!(eval("S = x", "[$(if $(S) ,yes,no)]"), "[yes]");
    assert_eq!(eval("", "[$(or , x)]"), "[x]");
    assert_eq!(eval("", "[$(and a , b )]"), "[b]");
    assert_eq!(eval("S =", "[$(and $(S) ,b)]"), "[]");
    // Stripping the condition must not reach the branches.
    assert_eq!(eval("S =", "[$(if $(S) , yes , no )]"), "[ no ]");
}

#[test]
fn an_unmatched_closer_does_not_swallow_the_argument_list() {
    // The depth counter tracks only the delimiter pair that opened the
    // reference, and never goes negative. A `}` inside `$(...)` is ordinary
    // text; letting it drive the count below zero suppressed every later
    // comma, so `$(subst },X,a}b)` returned nothing at all.
    assert_eq!(eval("", "$(subst },X,a}b)"), "aXb");
    assert_eq!(eval("", "$(subst {,X,a{b)"), "aXb");
    // A `}` that arrives after the split point is still ordinary text.
    assert_eq!(eval("", "$(subst a,},a}a)"), "}}}");
    // Nesting of the *matching* pair still protects its commas.
    assert_eq!(eval("V = a,b", "$(words $(subst a,x,$(V)))"), "1");
}

#[test]
fn a_deep_but_acyclic_chain_expands() {
    // 100 links, no repeats. Rejecting this as a self-reference would be a
    // false accusation, and the makefile that hits it is correct.
    let mut mk = String::new();
    for i in 0..100 {
        mk.push_str(&format!("A{i} = $(A{})\n", i + 1));
    }
    mk.push_str("A100 = deep\n");
    assert_eq!(eval(&mk, "$(A0)"), "deep");
}

#[test]
fn exceeding_the_depth_cap_without_a_repeat_is_reported_as_depth() {
    let mut mk = String::new();
    for i in 0..300 {
        mk.push_str(&format!("A{i} = $(A{})\n", i + 1));
    }
    mk.push_str("A300 = deep\n");
    let mut e = parse(&mk);
    let msg = e
        .expand("$(A0)", &AutoVars::default())
        .expect_err("beyond the cap")
        .to_string();
    assert!(
        msg.contains("nested deeper") && !msg.contains("references itself"),
        "a chain with no repeat is depth, not a cycle: {msg}"
    );
}

#[test]
fn a_comment_hash_is_escaped_by_an_odd_run_of_backslashes() {
    // GNU make counts the backslashes: an even run leaves the `#` starting a
    // comment and still halves the run, so `a\\#` is `a\`. Looking only at the
    // character before the `#` keeps the whole line instead.
    assert_eq!(eval("V = a\\\\# c", "[$(V)]"), "[a\\]");
    assert_eq!(eval("V = a\\# c", "[$(V)]"), "[a# c]");
    assert_eq!(eval("V = a\\\\\\# c", "[$(V)]"), "[a\\# c]");
    // A backslash that quotes nothing is left alone, and a trailing one still
    // continues the line.
    assert_eq!(eval("V = a\\b", "[$(V)]"), "[a\\b]");
    assert_eq!(eval("V = a \\\n b", "[$(V)]"), "[a b]");
}

#[test]
fn a_backslash_escapes_a_literal_percent_in_a_pattern() {
    // GNU make: `\%` is a literal percent, so the wildcard is the first `%`
    // preceded by an even number of backslashes.
    assert_eq!(eval("", "$(patsubst \\%.c,X,%.c a.c)"), "X a.c");
    assert_eq!(eval("", "$(patsubst %.c,X,a.c b.c)"), "X X");
    assert_eq!(eval("", "$(filter \\%,% a)"), "%");
    assert_eq!(eval("", "$(filter-out \\%,% a)"), "a");
    // The escape collapses in the replacement too.
    assert_eq!(eval("", "$(patsubst %.c,\\%-%,a.c)"), "%-a");
}

#[test]
fn a_deep_glob_with_several_stars_still_matches() {
    // The `*` arm recurses two ways rather than trying every split, so a
    // pattern with several stars stays polynomial instead of quadratic per
    // star. This asserts the answer; the bound is in the comment on `go`.
    assert_eq!(eval("", "$(filter a%c,abbbbbbbbbbc x)"), "abbbbbbbbbbc");
    let dir = scratch("glob");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for n in ["a1b2c3.c", "abc.c", "nope.c"] {
        std::fs::write(dir.join(n), "").expect("fixture");
    }
    let got = eval("", &format!("$(wildcard {}/a*b*c*.c)", dir.display()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut names: Vec<&str> = got
        .split_whitespace()
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["a1b2c3.c", "abc.c"]);
}

#[test]
fn warning_and_info_are_both_counted() {
    // The counters exist so a test can assert the diagnostic happened, and
    // they are separate: `$(warning)` goes to stderr and `$(info)` to stdout,
    // so a single field could not say which of them fired.
    let mut e = parse("");
    e.expand("$(warning w)", &AutoVars::default())
        .expect("warning expands");
    assert_eq!(e.warnings, 1, "$(warning) is counted");
    assert_eq!(e.infos, 0, "$(warning) is not an $(info)");
    e.expand("$(info i)", &AutoVars::default())
        .expect("info expands");
    assert_eq!(e.infos, 1, "$(info) is counted separately");
    assert_eq!(e.warnings, 1, "$(info) must not bump the warning count");
}

#[test]
fn the_shell_variable_is_expanded_before_it_is_run() {
    // `SHELL = $(BASH)` names a program. Handing the unexpanded text to exec
    // looks for a file literally called `$(BASH)`.
    assert_eq!(
        eval("SH = /bin/sh\nSHELL = $(SH)", "$(shell echo ok)"),
        "ok"
    );
}

#[test]
fn the_shell_function_captures_stdout_and_lets_stderr_through() {
    // GNU make captures only stdout; the child's stderr goes to make's, so a
    // failing probe says why instead of vanishing into the expansion.
    assert_eq!(eval("", "$(shell echo boom 1>&2; echo ok)"), "ok");

    // Passthrough cannot be observed from inside this process -- the child
    // inherits the test harness's own stderr -- so it is asserted against the
    // binary, which is where the behaviour matters.
    let dir = scratch("stderr-dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(
        dir.join("Makefile"),
        "V := $(shell echo boom 1>&2; echo ok)\nall:\n\t@echo [$(V)]\n",
    )
    .expect("write makefile");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rsmake"))
        .arg("-n")
        .current_dir(&dir)
        .output()
        .expect("rsmake runs");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[ok]"),
        "stdout is captured: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("boom"),
        "the child's stderr must reach ours, not be dropped: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_define_in_a_dead_branch_is_captured_and_discarded() {
    // The body of a `define` is arbitrary text. A dead branch that does not
    // enter capture mode lets a body line reading `endif` close the enclosing
    // conditional, and the failure surfaces much later as a bogus "missing
    // separator" on an unrelated line.
    let mut e = parse("ifeq (a,b)\ndefine BODY\nendif\nendef\nendif\nall:\n\techo hi\n");
    assert_eq!(
        e.expand("[$(BODY)]", &AutoVars::default())
            .expect("expands"),
        "[]",
        "a define in a dead branch must not be assigned"
    );
    // The live case still assigns.
    let mut e = parse("ifeq (a,a)\ndefine BODY\nbody line\nendef\nendif\n");
    assert_eq!(
        e.expand("[$(BODY)]", &AutoVars::default())
            .expect("expands"),
        "[body line]"
    );
}

#[test]
fn a_call_restores_a_global_it_shadowed() {
    // `1` and `2` are legal ordinary variable names. A `call` that binds `$(1)`
    // must put the old `1` back when it returns, not leave it undefined:
    // saving `None` for a positional the call happens to bind destroys it.
    // Measured against GNU make 4.4.1, which prints
    // `in=[arg][globaltwo] after=[globalone][globaltwo]`.
    let mk = "1 = globalone\n2 = globaltwo\nf = [$(1)][$(2)]\n";
    let mut e = parse(mk);
    let auto = AutoVars::default();
    assert_eq!(
        e.expand("$(call f,arg)", &auto).expect("expands"),
        "[arg][globaltwo]",
        "an unbound positional must show the global, a bound one the argument"
    );
    assert_eq!(
        e.expand("[$(1)][$(2)]", &auto).expect("expands"),
        "[globalone][globaltwo]",
        "the call must restore the global `1` it shadowed"
    );
}

#[test]
fn an_inline_recipe_is_not_scanned_for_assignments() {
    // The rule's grammar ends at the `;`. Probing the whole line for a
    // target-specific assignment made `all: ; @echo "FOO?=bar"` look like a
    // target-specific `?=` (refused) and `all: ; @echo V=x` look like an
    // assignment with no targets. GNU runs both.
    for recipe in ["@echo \"FOO?=bar\"", "@echo V=x", "@echo A += B"] {
        let mk = format!("all: ; {recipe}\n");
        let e = parse(&mk);
        let rules = e.rules.explicit.get("all").expect("`all` is a rule");
        assert_eq!(
            rules[0].recipe,
            vec![format!(" {recipe}")],
            "the text after `;` is a recipe line, not part of the rule head"
        );
    }
}

#[test]
fn a_semicolon_inside_a_target_specific_value_is_kept() {
    // The other half of the same split: once the line *is* an assignment there
    // is no inline recipe on it, and GNU keeps the `;` in the value —
    // `all: V = a;b` gives `V` the value `a;b`.
    let e = parse("all: V = a;b\nall:\n\t@echo hi\n");
    let tvars = e.rules.target_vars.get("all").expect("target var recorded");
    assert_eq!(tvars[0].name, "V");
    assert_eq!(tvars[0].value, "a;b");
}

#[test]
fn makeflags_picks_up_directives_the_makefile_set() {
    // `$(MAKEFLAGS)` is defined while the makefile is being read, so a
    // makefile may inspect it during the parse, and recomputed once the parse
    // is over, so a `.IGNORE:` seen along the way is in it. GNU 4.4.1 does
    // both: `$(info)` above and below `.IGNORE:` print the pre-`.IGNORE`
    // value, and the recipe environment carries the `i`.
    let mk = scratch("makeflags");
    std::fs::write(&mk, ".IGNORE:\n.PHONY: all\nall:\n").expect("makefile written");
    let mut e = Engine::new(Opts {
        makefile: vec![mk.clone()],
        goals: vec!["all".to_string()],
        always_make: true,
        env_overrides: true,
        ..Opts::default()
    });
    let auto = AutoVars::default();
    assert_eq!(
        e.expand("[$(MAKEFLAGS)]", &auto).expect("expands"),
        "[Be]",
        "at parse time the flags are the invocation's"
    );
    e.make().expect("the build succeeds");
    let _ = std::fs::remove_file(&mk);
    assert_eq!(
        e.expand("[$(MAKEFLAGS)]", &auto).expect("expands"),
        "[Bei]",
        "`.IGNORE:` must reach $(MAKEFLAGS) once the makefile has been read"
    );
}

#[test]
fn silent_from_a_makefile_is_not_a_makeflag() {
    // `.SILENT:` silences this makefile's recipes; `-s` silences and is
    // inherited. GNU 4.4.1 puts no `s` in a child's MAKEFLAGS for `.SILENT:`,
    // so the two cannot share one field.
    let mk = scratch("silent");
    std::fs::write(&mk, ".SILENT:\n.PHONY: all\nall:\n").expect("makefile written");
    let mut e = Engine::new(Opts {
        makefile: vec![mk.clone()],
        goals: vec!["all".to_string()],
        ..Opts::default()
    });
    e.make().expect("the build succeeds");
    let _ = std::fs::remove_file(&mk);
    assert!(e.rules.silent_all, "`.SILENT:` still silences recipes");
    assert_eq!(
        e.expand("[$(MAKEFLAGS)]", &AutoVars::default())
            .expect("expands"),
        "[]",
        "`.SILENT:` must not propagate `s` to sub-makes"
    );
}
