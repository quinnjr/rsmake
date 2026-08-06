//! rsmake — a `make` for GNU-free base systems.
//!
//! The dialect is defined in the design spec and is deliberately closed: a
//! construct outside it is an error naming the construct, never a silent skip.
//! A build that quietly does the wrong thing is worse than one that stops.
//!
//! The pipeline is exposed as a library so the differential harness can drive
//! it in-process rather than spawning a binary per corpus entry.

use std::collections::HashMap;
use std::path::PathBuf;

pub mod expand;
pub mod graph;
pub mod parse;
pub mod run;

/// A location in makefile text, used for every diagnostic.
///
/// Carried by value rather than interned: makefiles are small, and an
/// unlocatable error in a generated include is the single most annoying thing
/// a make can do to you.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loc {
    pub file: String,
    pub line: usize,
}

impl std::fmt::Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

#[derive(Debug)]
pub struct Error {
    pub msg: String,
    pub loc: Option<Loc>,
}

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            loc: None,
        }
    }
    pub fn at(loc: &Loc, msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            loc: Some(loc.clone()),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.loc {
            Some(l) => write!(f, "{l}: *** {}.  Stop.", self.msg),
            None => write!(f, "*** {}.  Stop.", self.msg),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a variable's value came from. The ordering is the precedence rule:
/// a lower origin never overwrites a higher one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Default,
    Environment,
    File,
    CommandLine,
    /// `foreach` bindings and `call` positionals. Not overridable and not
    /// visible to `$(origin)` as anything else.
    Automatic,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::Default => "default",
            Origin::Environment => "environment",
            Origin::File => "file",
            Origin::CommandLine => "command line",
            Origin::Automatic => "automatic",
        }
    }
}

/// Recursive (`=`) stores text and expands at every reference; simple (`:=`)
/// expands once at assignment. The visible consequence is that `$(shell ...)`
/// on the right of a `=` runs once per reference, not once per makefile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Recursive,
    Simple,
}

#[derive(Clone, Debug)]
pub struct Var {
    pub value: String,
    pub flavor: Flavor,
    pub origin: Origin,
    pub exported: bool,
}

#[derive(Debug, Default)]
pub struct Vars {
    map: HashMap<String, Var>,
    /// Set by `export` with no arguments. `.EXPORT_ALL_VARIABLES` is outside
    /// the dialect and is rejected by name rather than silently honoured.
    pub export_all: bool,
    /// `-e`: the environment outranks the makefile.
    pub env_overrides: bool,
}

impl Vars {
    pub fn get(&self, name: &str) -> Option<&Var> {
        self.map.get(name)
    }

    pub fn value(&self, name: &str) -> &str {
        self.map.get(name).map(|v| v.value.as_str()).unwrap_or("")
    }

    /// Effective precedence of an origin, accounting for `-e`.
    fn rank(&self, o: Origin) -> u8 {
        match o {
            Origin::Environment if self.env_overrides => Origin::File as u8 + 1,
            other => other as u8,
        }
    }

    /// Assign, honouring precedence. Returns whether the assignment took.
    ///
    /// An equal-ranked assignment wins, so a makefile may reassign its own
    /// variables; a strictly lower-ranked one is dropped, which is what makes
    /// `make CC=clang` immune to `CC = cc` in the makefile.
    pub fn set(&mut self, name: &str, var: Var) -> bool {
        if let Some(old) = self.map.get(name)
            && self.rank(var.origin) < self.rank(old.origin)
        {
            return false;
        }
        let exported = self.map.get(name).is_some_and(|v| v.exported) || var.exported;
        self.map.insert(name.to_string(), Var { exported, ..var });
        true
    }

    /// Unconditional assignment, for the automatic bindings of `foreach` and
    /// `call` which are scoped to one expansion and outrank nothing.
    pub fn force(&mut self, name: &str, value: String, origin: Origin) {
        self.map.insert(
            name.to_string(),
            Var {
                value,
                flavor: Flavor::Simple,
                origin,
                exported: false,
            },
        );
    }

    pub fn remove(&mut self, name: &str) -> Option<Var> {
        self.map.remove(name)
    }

    pub fn restore(&mut self, name: &str, saved: Option<Var>) {
        match saved {
            Some(v) => {
                self.map.insert(name.to_string(), v);
            }
            None => {
                self.map.remove(name);
            }
        }
    }

    pub fn set_exported(&mut self, name: &str, exported: bool) {
        if let Some(v) = self.map.get_mut(name) {
            v.exported = exported;
        } else {
            self.map.insert(
                name.to_string(),
                Var {
                    value: String::new(),
                    flavor: Flavor::Recursive,
                    origin: Origin::File,
                    exported,
                },
            );
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Var)> {
        self.map.iter()
    }
}

/// The automatic variables, bound per recipe rather than stored in [`Vars`]
/// because they are scoped to one rule instance.
#[derive(Clone, Debug, Default)]
pub struct AutoVars {
    /// `$@`
    pub target: String,
    /// `$*`
    pub stem: String,
    /// `$%` — only meaningful for archive members, which are excluded, so it
    /// is always empty. Present so `$%` expands to nothing rather than erroring.
    pub member: String,
    /// `$<`
    pub first_prereq: String,
    /// `$^`, deduplicated
    pub prereqs: Vec<String>,
    /// `$+`, not deduplicated
    pub prereqs_all: Vec<String>,
    /// `$?`
    pub newer: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub targets: Vec<String>,
    pub prereqs: Vec<String>,
    pub order_only: Vec<String>,
    /// Unexpanded. Recipe lines expand when the recipe runs, which is why `$$`
    /// in a recipe reaches the shell as `$`.
    pub recipe: Vec<String>,
    pub double_colon: bool,
    pub loc: Loc,
}

/// Target-specific variable assignments, applied as an overlay while a rule's
/// prerequisites and recipe are expanded.
#[derive(Clone, Debug)]
pub struct TargetVar {
    pub name: String,
    pub value: String,
    pub flavor: Flavor,
    pub append: bool,
}

#[derive(Debug, Default)]
pub struct Rules {
    /// Explicit rules by target. A single-colon target may appear more than
    /// once (prerequisites accumulate); double-colon entries each keep their
    /// own recipe.
    pub explicit: HashMap<String, Vec<Rule>>,
    /// Pattern rules in declaration order; first match with satisfiable
    /// prerequisites wins.
    pub patterns: Vec<Rule>,
    /// Insertion order of explicit targets, so the default goal is the first
    /// non-special target in the file rather than a hash-map accident.
    pub order: Vec<String>,
    pub target_vars: HashMap<String, Vec<TargetVar>>,
    pub phony: Vec<String>,
    pub precious: Vec<String>,
    pub silent_targets: Vec<String>,
    pub ignore_targets: Vec<String>,
    pub suffixes: Vec<String>,
    pub default_rule: Option<Rule>,
    pub delete_on_error: bool,
    pub not_parallel: bool,
    pub posix: bool,
    /// `VPATH` is a plain variable; `vpath` directives are pattern-scoped.
    pub vpath: Vec<(String, Vec<String>)>,
}

impl Rules {
    pub fn is_phony(&self, t: &str) -> bool {
        self.phony.iter().any(|p| p == t)
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub makefile: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub jobs: usize,
    pub keep_going: bool,
    pub dry_run: bool,
    pub silent: bool,
    pub ignore_errors: bool,
    pub always_make: bool,
    pub env_overrides: bool,
    pub question: bool,
    pub no_builtin_rules: bool,
    pub no_builtin_vars: bool,
    pub goals: Vec<String>,
    pub overrides: Vec<(String, String)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            makefile: None,
            directory: None,
            jobs: 1,
            keep_going: false,
            dry_run: false,
            silent: false,
            ignore_errors: false,
            always_make: false,
            env_overrides: false,
            question: false,
            no_builtin_rules: false,
            no_builtin_vars: false,
            goals: Vec::new(),
            overrides: Vec::new(),
        }
    }
}

/// Parsing, expansion, and rule storage share one owner because they are not
/// separable: conditionals and `include` are evaluated during the parse, so
/// the variable table must be live while the file is being read, and
/// `$(eval)` re-enters the parser from inside an expansion.
pub struct Engine {
    pub vars: Vars,
    pub rules: Rules,
    pub opts: Opts,
    /// Location of the construct currently being expanded, for `$(error)`.
    pub loc: Loc,
    /// Guards self-referential variables. A cycle is reported with its chain
    /// rather than recursing to a stack overflow.
    pub depth: usize,
    pub expanding: Vec<String>,
    /// Diagnostics from `$(warning)` and `$(info)` are written immediately;
    /// this counts them so tests can assert they happened.
    pub warnings: usize,
}

pub const MAX_EXPAND_DEPTH: usize = 256;

impl Engine {
    pub fn new(opts: Opts) -> Self {
        let mut vars = Vars {
            env_overrides: opts.env_overrides,
            ..Vars::default()
        };
        for (k, v) in std::env::vars() {
            // MAKEFLAGS and MAKELEVEL are recomputed rather than inherited as
            // ordinary variables; inheriting them would let a parent's `-j`
            // silently re-enter through the back door after §8 stripped it.
            if k == "MAKEFLAGS" || k == "MAKELEVEL" {
                continue;
            }
            vars.set(
                &k,
                Var {
                    value: v,
                    flavor: Flavor::Recursive,
                    origin: Origin::Environment,
                    exported: true,
                },
            );
        }
        if !opts.no_builtin_vars {
            for (k, v) in DEFAULT_VARS {
                vars.set(
                    k,
                    Var {
                        value: (*v).to_string(),
                        flavor: Flavor::Recursive,
                        origin: Origin::Default,
                        exported: false,
                    },
                );
            }
        }
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "rsmake".to_string());
        vars.set(
            "MAKE",
            Var {
                value: exe,
                flavor: Flavor::Recursive,
                origin: Origin::Default,
                exported: false,
            },
        );
        for (k, v) in &opts.overrides {
            vars.set(
                k,
                Var {
                    value: v.clone(),
                    flavor: Flavor::Recursive,
                    origin: Origin::CommandLine,
                    exported: true,
                },
            );
        }
        let mut rules = Rules {
            suffixes: DEFAULT_SUFFIXES.iter().map(|s| (*s).to_string()).collect(),
            ..Rules::default()
        };
        if opts.no_builtin_rules {
            rules.suffixes.clear();
        }
        Engine {
            vars,
            rules,
            opts,
            loc: Loc::default(),
            depth: 0,
            expanding: Vec::new(),
            warnings: 0,
        }
    }
}

/// `cc` and `c++` rather than `clang` and `clang++`: a makefile that overrides
/// `CC` expects the conventional default, and the base system provides `cc` as
/// clang. Baking the real compiler name in would make rsmake's output differ
/// from GNU make's for no gain, which corrupts the differential oracle.
pub const DEFAULT_VARS: &[(&str, &str)] = &[
    ("CC", "cc"),
    ("CXX", "c++"),
    ("AR", "ar"),
    ("AS", "as"),
    ("ARFLAGS", "rv"),
    ("CFLAGS", ""),
    ("CXXFLAGS", ""),
    ("CPPFLAGS", ""),
    ("ASFLAGS", ""),
    ("LDFLAGS", ""),
    ("LDLIBS", ""),
    ("SHELL", "/bin/sh"),
    ("MAKELEVEL", "0"),
    ("OUTPUT_OPTION", "-o $@"),
    ("COMPILE.c", "$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c"),
    (
        "COMPILE.cc",
        "$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c",
    ),
    (
        "COMPILE.S",
        "$(CC) $(ASFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c",
    ),
    (
        "LINK.c",
        "$(CC) $(CFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)",
    ),
    ("LINK.o", "$(CC) $(LDFLAGS) $(TARGET_ARCH)"),
];

pub const DEFAULT_SUFFIXES: &[&str] = &[".o", ".c", ".cc", ".cpp", ".S", ".s", ".h"];

/// The built-in rule set is listed rather than inherited from GNU make's
/// catalogue. Six rules cover what a hand-written Makefile in a GNU-free base
/// actually leans on; the rest of GNU's catalogue exists for languages and
/// archive formats this base does not ship.
pub const BUILTIN_RULES: &[(&str, &str, &str)] = &[
    ("%.o", "%.c", "$(COMPILE.c) $(OUTPUT_OPTION) $<"),
    ("%.o", "%.cc", "$(COMPILE.cc) $(OUTPUT_OPTION) $<"),
    ("%.o", "%.cpp", "$(COMPILE.cc) $(OUTPUT_OPTION) $<"),
    ("%.o", "%.S", "$(COMPILE.S) $(OUTPUT_OPTION) $<"),
    ("%", "%.o", "$(LINK.o) $^ $(LDLIBS) -o $@"),
    ("%", "%.c", "$(LINK.c) $^ $(LDLIBS) -o $@"),
];
