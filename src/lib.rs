//! rsmake — a `make` for GNU-free base systems.
//!
//! The dialect is defined in the design spec and is deliberately closed: a
//! construct outside it is an error naming the construct, never a silent skip.
//! A build that quietly does the wrong thing is worse than one that stops.
//!
//! The pipeline is exposed as a library so the differential harness can drive
//! it in-process rather than spawning a binary per corpus entry.

use std::collections::{HashMap, HashSet};
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

/// Where a variable's value came from. Precedence is *not* this enum's
/// declaration order: it is [`Vars::rank`], which is hand-written because `-e`
/// moves the environment above the makefile. Compare origins with `rank`, never
/// with `<`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Default,
    Environment,
    /// An environment variable that actually *displaced* a makefile
    /// assignment under `-e`. Measured against GNU make 4.4.1: `$(origin V)`
    /// says `environment override` only once a `V = ...` in the makefile has
    /// been dropped in its favour; an environment variable the makefile never
    /// assigns stays plain `environment`.
    EnvironmentOverride,
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
            Origin::EnvironmentOverride => "environment override",
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
    ///
    /// Spelled out rather than derived from the discriminant so that adding
    /// [`Origin::EnvironmentOverride`] between `Environment` and `File` cannot
    /// silently shift anything else's rank.
    fn rank(&self, o: Origin) -> u8 {
        match o {
            Origin::Default => 0,
            Origin::Environment | Origin::EnvironmentOverride if self.env_overrides => 3,
            Origin::Environment | Origin::EnvironmentOverride => 1,
            Origin::File => 2,
            Origin::CommandLine => 4,
            Origin::Automatic => 5,
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
            // A makefile assignment that lost to the environment because of
            // `-e` is exactly what promotes that variable's origin from
            // `environment` to `environment override`; nothing else does.
            if var.origin == Origin::File && old.origin == Origin::Environment {
                if let Some(old) = self.map.get_mut(name) {
                    old.origin = Origin::EnvironmentOverride;
                }
            }
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
    /// Membership sets, not lists: every one of these is only ever asked
    /// "is this target in it?", once per node or per prepared recipe, and a
    /// linear scan of `.PHONY` in a makefile that declares a few hundred
    /// phony targets is quadratic for no reason. Nothing iterates them, so
    /// there is no declaration order to preserve.
    pub phony: HashSet<String>,
    pub precious: HashSet<String>,
    pub silent_targets: HashSet<String>,
    pub ignore_targets: HashSet<String>,
    pub suffixes: Vec<String>,
    pub default_rule: Option<Rule>,
    pub delete_on_error: bool,
    pub not_parallel: bool,
    /// A bare `.SILENT:` silences every recipe, but — unlike `-s` — it is not
    /// propagated to sub-makes: GNU puts no `s` in the child's `MAKEFLAGS` for
    /// it, because it is a property of *this* makefile, not of the invocation.
    /// Kept apart from [`Opts::silent`] for exactly that reason; folding the
    /// two would leak `s` down the tree.
    pub silent_all: bool,
    pub posix: bool,
    /// `VPATH` is a plain variable; `vpath` directives are pattern-scoped.
    pub vpath: Vec<(String, Vec<String>)>,
}

impl Rules {
    pub fn is_phony(&self, t: &str) -> bool {
        self.phony.contains(t)
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    /// Every `-f`, in order. GNU make reads them all, and the first file's
    /// first target is the default goal.
    pub makefile: Vec<PathBuf>,
    /// Every `-C`, chained: each is entered relative to the previous one.
    pub directory: Vec<PathBuf>,
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
            makefile: Vec::new(),
            directory: Vec::new(),
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
    /// `$(warning)` calls. The diagnostic is written immediately; this counts
    /// them so tests can assert it happened.
    pub warnings: usize,
    /// `$(info)` calls, counted separately from [`Engine::warnings`]: the two
    /// go to different streams, and one field could not say which fired.
    pub infos: usize,
    /// Files already named in a "modification time in the future" warning.
    /// Staleness is asked once per dependent edge, so without this a skewed
    /// file with ten dependents is reported ten times; GNU warns once per
    /// file. Interior-mutable because `staleness` takes `&self`.
    pub future_warned: std::cell::RefCell<HashSet<String>>,
    /// Files whose parse is in progress, canonicalised — the `include` cycle
    /// guard. A makefile that includes itself, directly or through a ring,
    /// would otherwise recurse until the stack runs out, and a stack overflow
    /// names nothing.
    pub including: Vec<PathBuf>,
    /// The `MAKELEVEL` inherited from the environment: 0 at the top, N in a
    /// sub-make N levels down. Kept as a field rather than read back out of
    /// `vars` because `-R` erases the `MAKELEVEL` default and a lookup would
    /// then report "" at the top level, which reads as "this is a sub-make".
    pub make_level: u32,
    /// Goals abandoned because a prerequisite failed, under `-k`. Filled in by
    /// [`Engine::make`] and printed by the caller *after* the error `make`
    /// returns, because that is the order GNU emits the two in.
    pub not_remade: Vec<String>,
}

pub const MAX_EXPAND_DEPTH: usize = 256;

impl Engine {
    pub fn new(opts: Opts) -> Self {
        let mut vars = Vars {
            env_overrides: opts.env_overrides,
            ..Vars::default()
        };
        let make_level: u32 = std::env::var("MAKELEVEL")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        for (k, v) in std::env::vars() {
            // MAKEFLAGS and MAKELEVEL are recomputed rather than inherited as
            // ordinary variables; inheriting them would let a parent's `-j`
            // silently re-enter through the back door after §8 stripped it.
            // (The level itself is still carried, in `make_level` below.)
            //
            // SHELL is excluded outright: GNU make never takes SHELL from the
            // environment, because the user's interactive shell has nothing to
            // do with the shell recipes must run under. Importing it would
            // make a build's behaviour depend on who started it.
            if k == "MAKEFLAGS" || k == "MAKELEVEL" || k == "SHELL" {
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
        // Set unconditionally, and after the defaults so it wins over the
        // `MAKELEVEL` in `DEFAULT_VARS`: a sub-make must see its real depth,
        // and `-R` must not leave the variable undefined the way it does the
        // rest of the built-ins. GNU make always defines it.
        vars.set(
            "MAKELEVEL",
            Var {
                value: make_level.to_string(),
                flavor: Flavor::Recursive,
                origin: Origin::Default,
                exported: false,
            },
        );
        // GNU make defines `$(MAKEFLAGS)` inside the makefile, not just in the
        // recipe environment: `make -n` expands it to `n`. Built from the same
        // assembly the child environment uses, so a makefile that inspects it
        // sees exactly what a sub-make would be told.
        //
        // This is the *parse-time* value, which is why it is set here: a
        // makefile that reads `$(MAKEFLAGS)` while being read must not see an
        // empty string. Directives parsed later can still change the flags
        // (`.IGNORE:` turns on `i`), so `make()` recomputes it once the whole
        // file has been read — matching GNU, where a `$(info)` above `.IGNORE:`
        // and one below it both print the pre-`.IGNORE` value and only the
        // recipe environment carries the `i`.
        vars.set(
            "MAKEFLAGS",
            Var {
                value: run::makeflags_string(&opts),
                flavor: Flavor::Recursive,
                origin: Origin::File,
                exported: false,
            },
        );
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
            infos: 0,
            future_warned: std::cell::RefCell::new(HashSet::new()),
            including: Vec::new(),
            make_level,
            not_remade: Vec::new(),
        }
    }
}

pub const USAGE: &str = "\
usage: rsmake [options] [VAR=value ...] [target ...]

  -f FILE     read FILE as the makefile (repeatable; all are read in order)
  -C DIR      change to DIR before reading the makefile (repeatable; chained)
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
  -R          no built-in variables (implies -r)
  --version   print version
  --help      print this message
";

/// What the command line asked for. `--help` and `--version` are requests
/// rather than side effects so the parser is a pure function: it used to call
/// `process::exit` from inside a `Result`-returning routine, which made the
/// whole flag surface untestable.
#[derive(Debug)]
pub enum Request {
    Run(Opts),
    Help,
    Version,
}

/// The flag letters that mean the same thing on the command line and in
/// `MAKEFLAGS`. Returns whether `c` was one of them, so each caller can decide
/// what an unrecognised letter means: the command line refuses it, `MAKEFLAGS`
/// is more forgiving (see [`apply_makeflags`]).
///
/// Shared so the two tables cannot drift: a letter added to the CLI but not to
/// `MAKEFLAGS` would work at the top level and silently vanish in a sub-make.
fn apply_flag_letter(c: char, o: &mut Opts) -> bool {
    match c {
        'k' => o.keep_going = true,
        'n' => o.dry_run = true,
        's' => o.silent = true,
        'i' => o.ignore_errors = true,
        'B' => o.always_make = true,
        'e' => o.env_overrides = true,
        'r' => o.no_builtin_rules = true,
        // GNU: --no-builtin-variables implies --no-builtin-rules.
        // Without that, the built-in `%.o: %.c` recipe survives while
        // `COMPILE.c` is empty, and what reaches the shell is the bare
        // source file name -- i.e. rsmake tries to execute `foo.c`.
        'R' => {
            o.no_builtin_vars = true;
            o.no_builtin_rules = true;
        }
        _ => return false,
    }
    true
}

/// GNU flag letters rsmake knows are real but does not model. Silently ignored
/// in an inherited `MAKEFLAGS` rather than warned about, because a GNU make
/// parent invoked with any of them puts them in the environment of every child
/// and a warning per sub-make would be noise, not information.
///
/// `q` is in the list even though rsmake implements `-q`: it is deliberately
/// *not* propagated (see [`crate::run::makeflags_string`]), so an inherited `q`
/// must be swallowed rather than obeyed — but it is a real GNU flag and warning
/// "unknown" about it would be a lie.
const UNMODELLED_FLAG_LETTERS: &[char] = &['q', 't', 'o', 'w', 'v', 'd', 'p'];

/// Fold the single-letter flags of an inherited `MAKEFLAGS` into `o`.
///
/// GNU make applies the environment's flags first and the command line second,
/// so a sub-make invoked as `$(MAKE) -k` sees both. The value looks like `kns`
/// or `s -j32 --jobserver-auth=fifo:/tmp/x`: a leading cluster of letters with
/// no dash, then words. Words after the cluster are options rsmake does not
/// implement (there is no jobserver) and are ignored rather than refused —
/// refusing would make rsmake unable to run under a GNU make parent at all.
/// `j` is ignored for the reason documented at `child_env`.
///
/// GNU exits 2 on a letter that is not one of its options. rsmake cannot do
/// that faithfully without enumerating every GNU flag it does not implement, so
/// it splits the difference: letters in [`UNMODELLED_FLAG_LETTERS`] pass in
/// silence, and anything else is named in a warning on stderr and dropped. A
/// warning rather than an exit, because the value comes from a parent process
/// rather than from this invocation's user, and killing the build over it would
/// make rsmake unusable under a GNU parent using a flag rsmake has not listed.
pub fn apply_makeflags(makeflags: &str, o: &mut Opts) {
    let Some(cluster) = makeflags.split_whitespace().next() else {
        return;
    };
    if cluster.starts_with('-') {
        return;
    }
    for c in cluster.chars() {
        if apply_flag_letter(c, o) || UNMODELLED_FLAG_LETTERS.contains(&c) || c == 'j' {
            continue;
        }
        eprintln!("rsmake: warning: unknown flag `{c}` in MAKEFLAGS, ignored");
    }
}

/// Parse a command line, taking the inherited `MAKEFLAGS` as an argument so
/// the parser stays a pure function of its inputs.
///
/// Hand-written because make's command line mixes clustered short flags with
/// positional `VAR=value` assignments and bare goals, and `-j`'s argument is
/// optional-but-numeric. Every argument-parsing crate models one of those
/// three badly.
pub fn parse_args_with(makeflags: &str, args: &[String]) -> std::result::Result<Request, String> {
    let mut o = Opts::default();
    apply_makeflags(makeflags, &mut o);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--version" {
            return Ok(Request::Version);
        }
        if a == "--help" {
            return Ok(Request::Help);
        }
        if a == "--" {
            for rest in &args[i + 1..] {
                push_positional(&mut o, rest);
            }
            break;
        }
        if !a.starts_with('-') || a == "-" {
            push_positional(&mut o, a);
            i += 1;
            continue;
        }

        // A cluster like `-knf Makefile`: flags consume the rest of the
        // cluster as their argument when they take one, else the next word.
        // The cluster is walked by byte offset rather than by cloning the
        // remainder per character, which was quadratic in the cluster length.
        let cluster = &a[1..];
        let mut pos = 0usize;
        while pos < cluster.len() {
            let c = cluster[pos..].chars().next().expect("in-bounds char");
            pos += c.len_utf8();
            let rest = &cluster[pos..];
            let mut take_arg = |need: bool| -> std::result::Result<Option<String>, String> {
                if !rest.is_empty() {
                    pos = cluster.len();
                    return Ok(Some(rest.to_string()));
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
                'f' => {
                    if let Some(f) = take_arg(true)? {
                        o.makefile.push(PathBuf::from(f));
                    }
                }
                'C' => {
                    if let Some(d) = take_arg(true)? {
                        o.directory.push(PathBuf::from(d));
                    }
                }
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
                // `-q` is CLI-only: it asks a question about *this*
                // invocation and is deliberately not propagated to sub-makes,
                // so it has no place in the shared letter table.
                'q' => o.question = true,
                other => {
                    if !apply_flag_letter(other, &mut o) {
                        return Err(format!("unknown option -{other}"));
                    }
                }
            }
        }
        i += 1;
    }

    // `-n` output is the differential oracle's input, so it must be
    // reproducible; concurrent workers would interleave it.
    if o.dry_run {
        o.jobs = 1;
    }
    Ok(Request::Run(o))
}

/// A non-flag word: `VAR=value` is an override, anything else is a goal.
fn push_positional(o: &mut Opts, a: &str) {
    match a.find('=') {
        Some(eq) if eq > 0 => o
            .overrides
            .push((a[..eq].to_string(), a[eq + 1..].to_string())),
        _ => o.goals.push(a.to_string()),
    }
}

/// Parse the real command line, reading `MAKEFLAGS` from the environment.
pub fn parse_args(args: &[String]) -> std::result::Result<Request, String> {
    let flags = std::env::var("MAKEFLAGS").unwrap_or_default();
    parse_args_with(&flags, args)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_string()).collect()
    }

    /// Parse with no inherited `MAKEFLAGS`, which is what a top-level
    /// invocation sees, and unwrap the `Run` case.
    #[track_caller]
    fn opts(words: &[&str]) -> Opts {
        match parse_args_with("", &args(words)) {
            Ok(Request::Run(o)) => o,
            other => panic!("expected a run request, got {other:?}"),
        }
    }

    #[track_caller]
    fn err(words: &[&str]) -> String {
        match parse_args_with("", &args(words)) {
            Err(e) => e,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn clustered_flags_are_all_applied() {
        let o = opts(&["-kn"]);
        assert!(o.keep_going);
        assert!(o.dry_run);
        // `-n` forces serial: the dry-run stream is the oracle's input and
        // concurrent workers would interleave it.
        assert_eq!(o.jobs, 1);

        let o = opts(&["-knf", "Other.mk", "goal"]);
        assert!(o.keep_going && o.dry_run);
        assert_eq!(o.makefile, vec![PathBuf::from("Other.mk")]);
        assert_eq!(o.goals, vec!["goal".to_string()]);
    }

    #[test]
    fn job_counts_attached_and_detached_and_bare() {
        assert_eq!(opts(&["-j4"]).jobs, 4);
        assert_eq!(opts(&["-j", "4"]).jobs, 4);
        assert!(err(&["-j"]).contains("job count"));
        assert!(err(&["-j0"]).contains("at least 1"));
        assert!(err(&["-j", "x"]).contains("job count"));
    }

    #[test]
    fn attached_argument_forms_and_positionals() {
        let o = opts(&["-fMakefile.dev", "CC=clang", "all", "-Csub"]);
        assert_eq!(o.makefile, vec![PathBuf::from("Makefile.dev")]);
        assert_eq!(o.directory, vec![PathBuf::from("sub")]);
        assert_eq!(o.overrides, vec![("CC".to_string(), "clang".to_string())]);
        assert_eq!(o.goals, vec!["all".to_string()]);
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        // Everything after `--` is a goal or an override, even if it looks
        // like a flag. Without this, a target literally named `-n` is
        // unbuildable.
        let o = opts(&["--", "-n", "V=1"]);
        assert!(!o.dry_run, "`-n` after `--` is a goal, not a flag");
        assert_eq!(o.goals, vec!["-n".to_string()]);
        assert_eq!(o.overrides, vec![("V".to_string(), "1".to_string())]);
    }

    #[test]
    fn unknown_options_are_refused_by_name() {
        assert!(err(&["-Z"]).contains("-Z"));
        assert!(err(&["-nZ"]).contains("-Z"));
    }

    #[test]
    fn help_and_version_are_requests_rather_than_exits() {
        assert!(matches!(
            parse_args_with("", &args(&["--help"])),
            Ok(Request::Help)
        ));
        assert!(matches!(
            parse_args_with("", &args(&["-n", "--version"])),
            Ok(Request::Version)
        ));
    }

    #[test]
    fn repeated_f_and_c_accumulate() {
        // GNU make reads every `-f` in order and chains every `-C`.
        // Last-wins would silently build the wrong thing.
        let o = opts(&["-f", "a.mk", "-f", "b.mk", "-C", "x", "-C", "y"]);
        assert_eq!(
            o.makefile,
            vec![PathBuf::from("a.mk"), PathBuf::from("b.mk")]
        );
        assert_eq!(o.directory, vec![PathBuf::from("x"), PathBuf::from("y")]);
    }

    #[test]
    fn capital_r_implies_lowercase_r() {
        // Otherwise the built-in `%.o: %.c` rule survives with an empty
        // `COMPILE.c`, and the recipe collapses to the bare source file name.
        let o = opts(&["-R"]);
        assert!(o.no_builtin_vars);
        assert!(o.no_builtin_rules, "-R must imply -r");
        assert!(!opts(&["-r"]).no_builtin_vars, "-r must not imply -R");
    }

    #[test]
    fn every_simple_flag_is_reachable() {
        let o = opts(&["-k", "-B", "-e", "-s", "-i", "-q", "-r"]);
        assert!(o.keep_going && o.always_make && o.env_overrides);
        assert!(o.silent && o.ignore_errors && o.question && o.no_builtin_rules);
    }

    #[test]
    fn makeflags_is_applied_before_the_command_line() {
        // GNU order: the environment first, argv second, both cumulative.
        let o = match parse_args_with("ks", &args(&["-n"])) {
            Ok(Request::Run(o)) => o,
            other => panic!("{other:?}"),
        };
        assert!(o.keep_going && o.silent && o.dry_run);

        // The real thing from GNU make 4.4.1 looks like this. The words after
        // the leading cluster are options rsmake does not implement, and `j`
        // is deliberately not honoured -- there is no jobserver to join.
        let o = match parse_args_with("s -j32 --jobserver-auth=fifo:/tmp/x", &args(&[])) {
            Ok(Request::Run(o)) => o,
            other => panic!("{other:?}"),
        };
        assert!(o.silent);
        assert_eq!(o.jobs, 1, "-j must not come back in through MAKEFLAGS");

        // A leading dash means the whole value is long-option words: there is
        // no letter cluster to fold in.
        let o = match parse_args_with("--warn-undefined-variables", &args(&[])) {
            Ok(Request::Run(o)) => o,
            other => panic!("{other:?}"),
        };
        assert!(!o.silent && !o.keep_going);
    }

    #[test]
    fn makeflags_capital_r_implies_lowercase_r_too() {
        let mut o = Opts::default();
        apply_makeflags("R", &mut o);
        assert!(o.no_builtin_vars && o.no_builtin_rules);
    }

    /// `set`'s return value documents a real decision, and until this test
    /// existed every one of its nine call sites discarded it -- so the
    /// contract was written down and never observed.
    #[test]
    fn set_reports_that_a_lower_ranked_assignment_was_dropped() {
        let mut v = Vars::default();
        let cmdline = Var {
            value: "clang".to_string(),
            flavor: Flavor::Recursive,
            origin: Origin::CommandLine,
            exported: false,
        };
        assert!(v.set("CC", cmdline), "the first assignment always takes");
        let file = Var {
            value: "cc".to_string(),
            flavor: Flavor::Recursive,
            origin: Origin::File,
            exported: false,
        };
        assert!(
            !v.set("CC", file),
            "a makefile assignment must not beat `make CC=clang`"
        );
        assert_eq!(v.value("CC"), "clang");
        assert_eq!(v.get("CC").expect("still set").origin, Origin::CommandLine);
    }

    #[test]
    fn origin_becomes_environment_override_only_once_a_file_assignment_loses() {
        // Measured against GNU make 4.4.1: under `-e`, an environment
        // variable reports `environment override` only after a makefile
        // assignment to it has actually been dropped. One the makefile never
        // mentions stays plain `environment`.
        let mut v = Vars {
            env_overrides: true,
            ..Vars::default()
        };
        let env = |value: &str| Var {
            value: value.to_string(),
            flavor: Flavor::Recursive,
            origin: Origin::Environment,
            exported: true,
        };
        v.set("TOUCHED", env("from-env"));
        v.set("UNTOUCHED", env("from-env"));
        assert_eq!(
            v.get("UNTOUCHED").expect("set").origin.name(),
            "environment"
        );

        assert!(!v.set(
            "TOUCHED",
            Var {
                value: "from-file".to_string(),
                flavor: Flavor::Recursive,
                origin: Origin::File,
                exported: false,
            }
        ));
        assert_eq!(v.value("TOUCHED"), "from-env");
        assert_eq!(
            v.get("TOUCHED").expect("set").origin.name(),
            "environment override"
        );
        assert_eq!(
            v.get("UNTOUCHED").expect("set").origin.name(),
            "environment",
            "an untouched environment variable must not be promoted"
        );
    }

    #[test]
    fn without_dash_e_a_file_assignment_beats_the_environment() {
        let mut v = Vars::default();
        v.set(
            "CC",
            Var {
                value: "from-env".to_string(),
                flavor: Flavor::Recursive,
                origin: Origin::Environment,
                exported: true,
            },
        );
        assert!(v.set(
            "CC",
            Var {
                value: "from-file".to_string(),
                flavor: Flavor::Recursive,
                origin: Origin::File,
                exported: false,
            }
        ));
        assert_eq!(v.value("CC"), "from-file");
        assert_eq!(v.get("CC").expect("set").origin, Origin::File);
    }

    #[test]
    fn shell_is_never_taken_from_the_environment() {
        // GNU make never imports SHELL: the user's interactive shell has
        // nothing to do with the shell recipes run under, and importing it
        // makes a build depend on who started it.
        // Restored afterwards: the test process is shared with every other
        // test in this binary, and leaving SHELL=/bin/echo behind would make
        // any later test that reads it fail for reasons nothing points at.
        let prior = std::env::var_os("SHELL");
        unsafe { std::env::set_var("SHELL", "/bin/echo") };
        let e = Engine::new(Opts::default());
        let shell = e.vars.value("SHELL").to_string();
        unsafe {
            match prior {
                Some(v) => std::env::set_var("SHELL", v),
                None => std::env::remove_var("SHELL"),
            }
        }
        assert_eq!(
            shell, "/bin/sh",
            "SHELL must come from the built-in defaults, not the environment"
        );
    }

    #[test]
    fn makeflags_letters_rsmake_does_not_model_are_tolerated() {
        // GNU exits 2 on a letter that is not one of its options, but the
        // value arrives from a parent process: a real GNU flag rsmake does not
        // implement must not kill the build.
        let mut o = Opts::default();
        apply_makeflags("kqtw", &mut o);
        assert!(o.keep_going, "modelled letters still apply");
        assert!(!o.question, "`q` is not propagated into a sub-make");

        // Garbage is dropped too, but by name on stderr rather than silently.
        let mut o = Opts::default();
        apply_makeflags("Zs", &mut o);
        assert!(o.silent, "an unknown letter must not abort the cluster");
    }
}
