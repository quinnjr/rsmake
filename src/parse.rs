//! Makefile text to rules and variables.
//!
//! Parsing is not a separate pass. Conditionals and `include` are evaluated as
//! the file is read, so a variable assigned on line 3 governs the `ifeq` on
//! line 4, and `$(eval)` re-enters this parser from inside an expansion. That
//! is why the variable table, the rule table and the parser share one owner.

use crate::{AutoVars, Engine, Error, Flavor, Loc, Origin, Result, Rule, Rules, TargetVar, Var};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Recursive,
    Simple,
    Append,
    Cond,
}

enum Split {
    Assign {
        name_end: usize,
        val_start: usize,
        op: Op,
    },
    RuleColon {
        at: usize,
        double: bool,
    },
    Bang,
    None,
}

/// Find whichever comes first at paren depth zero: an assignment operator or a
/// rule colon. `foo: bar=baz` is a rule with a target-specific variable, and
/// `foo := bar: baz` is an assignment, so first-hit is the whole rule.
fn split_line(s: &str) -> Split {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth -= 1,
            b':' if depth == 0 => {
                if b[i + 1..].starts_with(b":=") {
                    return Split::Assign {
                        name_end: i,
                        val_start: i + 3,
                        op: Op::Simple,
                    };
                }
                if b[i + 1..].starts_with(b"=") {
                    return Split::Assign {
                        name_end: i,
                        val_start: i + 2,
                        op: Op::Simple,
                    };
                }
                let double = b[i + 1..].starts_with(b":");
                return Split::RuleColon { at: i, double };
            }
            b'=' if depth == 0 => {
                return Split::Assign {
                    name_end: i,
                    val_start: i + 1,
                    op: Op::Recursive,
                };
            }
            b'+' if depth == 0 && b[i + 1..].starts_with(b"=") => {
                return Split::Assign {
                    name_end: i,
                    val_start: i + 2,
                    op: Op::Append,
                };
            }
            b'?' if depth == 0 && b[i + 1..].starts_with(b"=") => {
                return Split::Assign {
                    name_end: i,
                    val_start: i + 2,
                    op: Op::Cond,
                };
            }
            b'!' if depth == 0 && b[i + 1..].starts_with(b"=") => return Split::Bang,
            _ => {}
        }
        i += 1;
    }
    Split::None
}

/// Strip an unescaped `#` comment.
///
/// The run of backslashes in front of a `#` decides: an odd run escapes it, so
/// the `#` is literal and the run collapses to half its length; an even run
/// leaves the `#` starting a comment and still collapses. `V = a\\# c` is
/// therefore `a\`, not `a\# c` — a rule that only looked at the single
/// character before the hash would keep the whole line.
fn strip_comment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut slashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '#' => {
                for _ in 0..slashes / 2 {
                    out.push('\\');
                }
                if slashes % 2 == 0 {
                    return out;
                }
                slashes = 0;
                out.push('#');
            }
            other => {
                for _ in 0..slashes {
                    out.push('\\');
                }
                slashes = 0;
                out.push(other);
            }
        }
    }
    for _ in 0..slashes {
        out.push('\\');
    }
    out
}

fn trailing_backslashes(s: &str) -> usize {
    s.chars().rev().take_while(|&c| c == '\\').count()
}

fn continues(s: &str) -> bool {
    trailing_backslashes(s) % 2 == 1
}

/// State of one `ifeq`/`ifdef` nesting level.
struct Cond {
    /// Some branch at this level has already been taken.
    taken: bool,
    /// This branch's lines execute, ignoring enclosing levels.
    active: bool,
    seen_else: bool,
}

/// Directives and special targets outside the dialect. Each is rejected by
/// name: a build that quietly does the wrong thing is worse than one that
/// stops, and every one of these changes semantics if ignored.
const REJECTED_DIRECTIVES: &[(&str, &str)] = &[
    (
        "override",
        "variable overriding is not supported; command-line assignments already win",
    ),
    (
        "undefine",
        "`undefine` is not supported; assign an empty value instead",
    ),
    ("private", "private variables are not supported"),
    ("load", "dynamic objects are not supported"),
    ("unload", "dynamic objects are not supported"),
];

const REJECTED_TARGETS: &[(&str, &str)] = &[
    (
        ".ONESHELL",
        "each recipe line runs in its own shell; join lines with `;` or `&&`",
    ),
    (".SECONDARY", "intermediate-file retention is not modelled"),
    (
        ".INTERMEDIATE",
        "intermediate-file deletion is not modelled",
    ),
    (".SECONDEXPANSION", "secondary expansion is not supported"),
    (".EXPORT_ALL_VARIABLES", "use `export` with no arguments"),
    (
        ".DEFAULT_GOAL",
        "the default goal is the first explicit non-special target",
    ),
];

/// Backstop for the include cycle guard. The guard identifies files by their
/// canonical path; if `canonicalize` fails — a path that momentarily does not
/// resolve, a filesystem that refuses it — two names for one file stop looking
/// equal and the ring is no longer detected. This cap turns that miss into a
/// named error instead of a stack overflow, which names nothing.
const MAX_INCLUDE_DEPTH: usize = 200;

impl Engine {
    pub fn parse_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(at) = self.including.iter().position(|p| *p == key) {
            let mut chain: Vec<String> = self.including[at..]
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            chain.push(key.display().to_string());
            return Err(Error::at(
                &self.loc,
                format!("include cycle: {}", chain.join(" -> ")),
            ));
        }
        if self.including.len() >= MAX_INCLUDE_DEPTH {
            return Err(Error::at(
                &self.loc,
                format!("include nesting deeper than {MAX_INCLUDE_DEPTH} levels"),
            ));
        }
        self.including.push(key);
        // Popped before the `?`, so an error inside the include unwinds the
        // stack the same way a success does; leaving the entry behind would
        // make a later, legitimate include of the same file look like a cycle.
        let r = self.parse_text(&text, &path.display().to_string());
        self.including.pop();
        r
    }

    /// Parse makefile text. Re-entrant: `include` and `$(eval)` both land here,
    /// and each nesting level keeps its own conditional stack and pending rule
    /// so an unbalanced `ifeq` inside an include cannot corrupt its parent.
    pub fn parse_text(&mut self, text: &str, file: &str) -> Result<()> {
        let raw: Vec<&str> = text.split('\n').collect();
        let mut conds: Vec<Cond> = Vec::new();
        let mut pending: Option<Rule> = None;
        // The `bool` is whether the branch holding this `define` is live. A
        // dead one still has to be captured: its body is arbitrary text, and
        // letting a line that reads `endif` reach the conditional dispatcher
        // unbalances the enclosing stack.
        let mut define: Option<(String, Op, Vec<String>, Loc, bool)> = None;
        let mut i = 0usize;

        while i < raw.len() {
            let lineno = i + 1;
            let first = raw[i];
            self.loc = Loc {
                file: file.to_string(),
                line: lineno,
            };

            // A `define` body is captured verbatim; only `endef` ends it.
            if let Some((name, op, body, loc, live)) = define.as_mut() {
                if first.trim() == "endef" {
                    let joined = body.join("\n");
                    let (n, o, l, alive) = (name.clone(), *op, loc.clone(), *live);
                    define = None;
                    if alive {
                        self.loc = l;
                        self.assign(&n, o, &joined)?;
                    }
                } else {
                    body.push(first.to_string());
                }
                i += 1;
                continue;
            }

            let executing = conds.iter().all(|c| c.active);

            // Recipe lines are tab-introduced and belong to the rule above.
            // The tab is load-bearing: a space-indented command is a syntax
            // error, not a recipe, and silently treating it as one is how a
            // makefile ends up running nothing.
            if let Some(body) = first.strip_prefix('\t') {
                let mut cmd = body.to_string();
                while continues(raw.get(i).copied().unwrap_or("")) && i + 1 < raw.len() {
                    i += 1;
                    // The backslash-newline is passed through to the shell;
                    // only the continuation line's own leading tab is dropped.
                    cmd.push('\n');
                    cmd.push_str(raw[i].strip_prefix('\t').unwrap_or(raw[i]));
                }
                if executing {
                    match pending.as_mut() {
                        Some(r) => r.recipe.push(cmd),
                        None if cmd.trim().is_empty() => {}
                        None => {
                            return Err(Error::at(
                                &self.loc,
                                "recipe commences before first target".to_string(),
                            ));
                        }
                    }
                }
                i += 1;
                continue;
            }

            // Assemble a logical line from backslash continuations, then strip
            // the comment from the whole of it. The order matters both ways:
            // a backslash-newline inside a comment continues the comment, and
            // a backslash left behind by comment stripping (`a\\#`) is text,
            // not a request to swallow the next line.
            let mut logical = first.to_string();
            while continues(&logical) && i + 1 < raw.len() {
                let keep = logical.len() - 1;
                logical.truncate(keep);
                // Backslash-newline and the whitespace around it collapse to a
                // single space. Trimming only one side leaves a double space
                // that survives into `$(words)` and command lines.
                let trimmed = logical.trim_end().to_string();
                logical = trimmed;
                i += 1;
                logical.push(' ');
                logical.push_str(raw[i].trim_start());
            }
            let logical = strip_comment(&logical);
            let line = logical.trim();

            if line.is_empty() {
                i += 1;
                continue;
            }

            let word = line.split_whitespace().next().unwrap_or("");

            // Conditionals are tracked even inside a skipped branch, so
            // nesting stays balanced.
            match word {
                "ifeq" | "ifneq" | "ifdef" | "ifndef" => {
                    let active = if executing {
                        self.eval_conditional(word, line)?
                    } else {
                        false
                    };
                    conds.push(Cond {
                        taken: active,
                        active,
                        seen_else: false,
                    });
                    i += 1;
                    continue;
                }
                "else" => {
                    let Some(c) = conds.last_mut() else {
                        return Err(Error::at(&self.loc, "`else` without `if`".to_string()));
                    };
                    if c.seen_else {
                        return Err(Error::at(&self.loc, "`else` after `else`".to_string()));
                    }
                    let rest = line["else".len()..].trim().to_string();
                    if rest.is_empty() {
                        c.seen_else = true;
                        c.active = !c.taken;
                        c.taken = true;
                    } else {
                        // `else ifeq (...)`: only evaluate when no earlier
                        // branch was taken and the enclosing levels are live.
                        let outer = conds[..conds.len() - 1].iter().all(|c| c.active);
                        let c = conds.last_mut().expect("checked above");
                        if c.taken || !outer {
                            c.active = false;
                        } else {
                            let w = rest.split_whitespace().next().unwrap_or("");
                            if !matches!(w, "ifeq" | "ifneq" | "ifdef" | "ifndef") {
                                return Err(Error::at(
                                    &self.loc,
                                    format!("expected a conditional after `else`, found `{w}`"),
                                ));
                            }
                            let active = self.eval_conditional(w, &rest)?;
                            let c = conds.last_mut().expect("checked above");
                            c.active = active;
                            c.taken = active;
                        }
                    }
                    i += 1;
                    continue;
                }
                "endif" => {
                    if conds.pop().is_none() {
                        return Err(Error::at(&self.loc, "`endif` without `if`".to_string()));
                    }
                    i += 1;
                    continue;
                }
                _ => {}
            }

            if !executing {
                // A `define` in a dead branch still captures, so its body
                // never reaches the dispatcher above. The name is left
                // unexpanded and the body is discarded at `endef`.
                if word == "define" {
                    let rest = line["define".len()..].trim().to_string();
                    define = Some((rest, Op::Recursive, Vec::new(), self.loc.clone(), false));
                }
                i += 1;
                continue;
            }

            if let Some((_, why)) = REJECTED_DIRECTIVES.iter().find(|(d, _)| *d == word) {
                return Err(Error::at(
                    &self.loc,
                    format!("`{word}` is outside the rsmake dialect: {why}"),
                ));
            }

            // A new non-recipe line closes the rule above it.
            if let Some(r) = pending.take() {
                self.install_rule(r)?;
            }

            match word {
                "define" => {
                    let rest = line["define".len()..].trim();
                    let (name, op) = match split_line(rest) {
                        Split::Assign {
                            name_end,
                            val_start,
                            op,
                        } => {
                            let _ = val_start;
                            (rest[..name_end].trim().to_string(), op)
                        }
                        _ => (rest.to_string(), Op::Recursive),
                    };
                    let name = self.expand(&name, &AutoVars::default())?.trim().to_string();
                    define = Some((name, op, Vec::new(), self.loc.clone(), true));
                    i += 1;
                    continue;
                }
                "include" | "-include" | "sinclude" => {
                    let optional = word != "include";
                    let rest = line[word.len()..].to_string();
                    let expanded = self.expand(&rest, &AutoVars::default())?;
                    for f in expanded.split_whitespace() {
                        let p = PathBuf::from(f);
                        if !p.exists() {
                            if optional {
                                continue;
                            }
                            return Err(Error::at(
                                &self.loc,
                                format!("{f}: no such file or directory"),
                            ));
                        }
                        let saved = self.loc.clone();
                        self.parse_file(&p)?;
                        self.loc = saved;
                    }
                    i += 1;
                    continue;
                }
                "export" | "unexport" => {
                    let rest = line[word.len()..].trim().to_string();
                    if rest.is_empty() {
                        self.vars.export_all = word == "export";
                        i += 1;
                        continue;
                    }
                    // `export NAME = value` assigns and exports in one line.
                    if let Split::Assign {
                        name_end,
                        val_start,
                        op,
                    } = split_line(&rest)
                    {
                        let name = self.expand(rest[..name_end].trim(), &AutoVars::default())?;
                        let name = name.trim().to_string();
                        self.assign(&name, op, rest[val_start..].trim())?;
                        self.vars.set_exported(&name, word == "export");
                        i += 1;
                        continue;
                    }
                    let names = self.expand(&rest, &AutoVars::default())?;
                    for n in names.split_whitespace() {
                        self.vars.set_exported(n, word == "export");
                    }
                    i += 1;
                    continue;
                }
                "vpath" => {
                    let rest = self.expand(&line["vpath".len()..], &AutoVars::default())?;
                    let mut it = rest.split_whitespace();
                    match (it.next(), it.collect::<Vec<_>>()) {
                        (None, _) => self.rules.vpath.clear(),
                        (Some(pat), dirs) if dirs.is_empty() => {
                            self.rules.vpath.retain(|(p, _)| p != pat);
                        }
                        (Some(pat), dirs) => {
                            let dirs: Vec<String> = dirs
                                .iter()
                                .flat_map(|d| d.split(':'))
                                .filter(|d| !d.is_empty())
                                .map(str::to_string)
                                .collect();
                            self.rules.vpath.push((pat.to_string(), dirs));
                        }
                    }
                    i += 1;
                    continue;
                }
                _ => {}
            }

            match split_line(line) {
                Split::Bang => {
                    return Err(Error::at(
                        &self.loc,
                        "`!=` shell assignment is outside the rsmake dialect; \
                         use `:=` with `$(shell ...)`"
                            .to_string(),
                    ));
                }
                Split::Assign {
                    name_end,
                    val_start,
                    op,
                } => {
                    let name = self.expand(line[..name_end].trim(), &AutoVars::default())?;
                    let name = name.trim().to_string();
                    if name.is_empty() {
                        return Err(Error::at(&self.loc, "empty variable name".to_string()));
                    }
                    self.assign(&name, op, line[val_start..].trim())?;
                }
                Split::RuleColon { at, double } => {
                    let targets_text = line[..at].to_string();
                    let rest = line[at + if double { 2 } else { 1 }..].to_string();
                    pending = self.begin_rule(&targets_text, &rest, double)?;
                }
                Split::None => {
                    // A bare line with no separator is legal when it is a
                    // function call that produces nothing -- `$(eval ...)` on
                    // a line of its own is the whole reason `eval` is usable.
                    let produced = self.expand(line, &AutoVars::default())?;
                    if !produced.trim().is_empty() {
                        return Err(Error::at(
                            &self.loc,
                            format!("missing separator in `{line}`"),
                        ));
                    }
                }
            }
            i += 1;
        }

        if let Some((name, _, _, loc, _)) = define {
            return Err(Error::at(
                &loc,
                format!("missing `endef` for `define {name}`"),
            ));
        }
        if let Some(c) = conds.pop() {
            let _ = c;
            return Err(Error::at(
                &Loc {
                    file: file.to_string(),
                    line: raw.len(),
                },
                "missing `endif`".to_string(),
            ));
        }
        if let Some(r) = pending.take() {
            self.install_rule(r)?;
        }
        Ok(())
    }

    fn eval_conditional(&mut self, word: &str, line: &str) -> Result<bool> {
        let rest = line[word.len()..].trim().to_string();
        let auto = AutoVars::default();
        match word {
            "ifdef" | "ifndef" => {
                let name = self.expand(&rest, &auto)?.trim().to_string();
                // `ifdef` tests for a non-empty value, not for definedness —
                // a variable assigned the empty string is not "defined" here.
                let defined = !self.var_value(&name, &auto)?.is_empty();
                Ok(defined == (word == "ifdef"))
            }
            "ifeq" | "ifneq" => {
                let (a, b) = self.conditional_operands(&rest)?;
                let a = self.expand(&a, &auto)?;
                let b = self.expand(&b, &auto)?;
                Ok((a == b) == (word == "ifeq"))
            }
            _ => unreachable!("caller matched the directive"),
        }
    }

    /// `ifeq (a,b)` and `ifeq "a" "b"` / `ifeq 'a' 'b'`, the three forms GNU
    /// accepts. The parenthesised form splits on the first comma at depth zero
    /// so `ifeq ($(f a,b),x)` works.
    fn conditional_operands(&self, rest: &str) -> Result<(String, String)> {
        let rest = rest.trim();
        if let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            let parts = crate::expand::split_args(inner, 2);
            if parts.len() != 2 {
                return Err(Error::at(
                    &self.loc,
                    format!("malformed conditional: {rest}"),
                ));
            }
            return Ok((parts[0].trim().to_string(), parts[1].trim().to_string()));
        }
        let mut operands = Vec::new();
        let mut it = rest.chars().peekable();
        while let Some(&c) = it.peek() {
            if c.is_whitespace() {
                it.next();
                continue;
            }
            if c != '"' && c != '\'' {
                return Err(Error::at(
                    &self.loc,
                    format!("malformed conditional: {rest}"),
                ));
            }
            it.next();
            let mut s = String::new();
            for ch in it.by_ref() {
                if ch == c {
                    break;
                }
                s.push(ch);
            }
            operands.push(s);
        }
        if operands.len() != 2 {
            return Err(Error::at(
                &self.loc,
                format!("malformed conditional: {rest}"),
            ));
        }
        Ok((operands.remove(0), operands.remove(0)))
    }

    fn assign(&mut self, name: &str, op: Op, raw_value: &str) -> Result<()> {
        let auto = AutoVars::default();
        let origin = Origin::File;
        match op {
            Op::Cond => {
                if self.vars.get(name).is_some() {
                    return Ok(());
                }
                self.vars.set(
                    name,
                    Var {
                        value: raw_value.to_string(),
                        flavor: Flavor::Recursive,
                        origin,
                        exported: self.vars.export_all,
                    },
                );
            }
            Op::Recursive => {
                self.vars.set(
                    name,
                    Var {
                        value: raw_value.to_string(),
                        flavor: Flavor::Recursive,
                        origin,
                        exported: self.vars.export_all,
                    },
                );
            }
            Op::Simple => {
                let v = self.expand(raw_value, &auto)?;
                self.vars.set(
                    name,
                    Var {
                        value: v,
                        flavor: Flavor::Simple,
                        origin,
                        exported: self.vars.export_all,
                    },
                );
            }
            Op::Append => {
                // Appending to a simple variable expands now; appending to a
                // recursive one appends the text unexpanded, so a `$(shell)`
                // added with `+=` still runs at reference time.
                let (old, flavor) = match self.vars.get(name) {
                    Some(v) => (v.value.clone(), v.flavor),
                    None => (String::new(), Flavor::Recursive),
                };
                let piece = if flavor == Flavor::Simple {
                    self.expand(raw_value, &auto)?
                } else {
                    raw_value.to_string()
                };
                let value = if old.is_empty() {
                    piece
                } else {
                    format!("{old} {piece}")
                };
                self.vars.set(
                    name,
                    Var {
                        value,
                        flavor,
                        origin,
                        exported: self.vars.export_all,
                    },
                );
            }
        }
        Ok(())
    }

    /// Start a rule: expand the targets, decide whether the right-hand side is
    /// prerequisites or a target-specific assignment, and split order-only.
    fn begin_rule(&mut self, targets_text: &str, rest: &str, double: bool) -> Result<Option<Rule>> {
        let auto = AutoVars::default();
        let targets_text = self.expand(targets_text, &auto)?;
        let targets: Vec<String> = targets_text
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if targets.is_empty() {
            return Err(Error::at(&self.loc, "rule has no targets".to_string()));
        }
        for t in &targets {
            if t.contains('(') && t.ends_with(')') {
                return Err(Error::at(
                    &self.loc,
                    format!("archive member syntax `{t}` is outside the rsmake dialect"),
                ));
            }
        }

        // An inline recipe (`target: prereqs ; command`) is split off first, so
        // the assignment probe below only ever looks at the prerequisite text.
        // Probing the whole line instead made `all: ; @echo "FOO?=bar"` look
        // like a target-specific `?=` and `all: ; @echo V=x` look like an
        // assignment with no targets — the recipe's own text is not part of the
        // rule's grammar.
        let (prereq_text, inline_recipe) = match rest.find(';') {
            Some(i) => (&rest[..i], Some(rest[i + 1..].to_string())),
            None => (rest, None),
        };

        // A target-specific variable: `target: CFLAGS += -g`.
        if let Split::Assign {
            name_end,
            val_start,
            op,
        } = split_line(prereq_text)
        {
            let name = self.expand(rest[..name_end].trim(), &auto)?;
            // GNU keeps the existing value for a target-specific `?=`. rsmake's
            // overlay has no way to express "only if unset" — it is applied
            // when the target is built, long after the test would have to be
            // made — so the construct is refused by name rather than silently
            // behaving like `=`, which would overwrite what GNU preserves.
            if op == Op::Cond {
                return Err(Error::at(
                    &self.loc,
                    format!(
                        "target-specific `?=` (`{}: {} ?= ...`) is outside the rsmake dialect; \
                         guard the assignment with `ifndef` instead",
                        targets.join(" "),
                        name.trim()
                    ),
                ));
            }
            // Deliberately `rest`, not `prereq_text`: once the line is known to
            // be an assignment there is no inline recipe on it, and GNU keeps a
            // `;` in the value — `all: V = a;b` gives `V` the value `a;b`.
            let value = rest[val_start..].trim().to_string();
            let (flavor, append) = match op {
                Op::Simple => (Flavor::Simple, false),
                Op::Append => (Flavor::Recursive, true),
                _ => (Flavor::Recursive, false),
            };
            let value = if op == Op::Simple {
                self.expand(&value, &auto)?
            } else {
                value
            };
            for t in &targets {
                self.rules
                    .target_vars
                    .entry(t.clone())
                    .or_default()
                    .push(TargetVar {
                        name: name.trim().to_string(),
                        value: value.clone(),
                        flavor,
                        append,
                    });
            }
            return Ok(None);
        }

        // `targets: pattern: prereqs` is a static pattern rule.
        if matches!(split_line(prereq_text), Split::RuleColon { .. }) {
            return Err(Error::at(
                &self.loc,
                "static pattern rules are outside the rsmake dialect; \
                 write a pattern rule or list the targets explicitly"
                    .to_string(),
            ));
        }

        let prereq_text = self.expand(prereq_text, &auto)?;
        let (normal, order_only) = match prereq_text.find('|') {
            Some(i) => (&prereq_text[..i], &prereq_text[i + 1..]),
            None => (prereq_text.as_str(), ""),
        };

        Ok(Some(Rule {
            targets,
            prereqs: normal.split_whitespace().map(str::to_string).collect(),
            order_only: order_only.split_whitespace().map(str::to_string).collect(),
            // A whitespace-only inline recipe is kept. `foo: ;` is an *empty*
            // recipe, which is how a makefile says "this target is made by
            // doing nothing" and suppresses the implicit-rule search;
            // discarding the line would make it indistinguishable from a rule
            // with no recipe at all, and the built-in `%.o: %.c` would fire.
            // The empty line expands to no command, so nothing is run for it.
            recipe: inline_recipe.into_iter().collect(),
            double_colon: double,
            loc: self.loc.clone(),
        }))
    }

    /// File the finished rule: special target, suffix rule, pattern rule, or
    /// ordinary explicit rule.
    fn install_rule(&mut self, rule: Rule) -> Result<()> {
        for t in rule.targets.clone() {
            if let Some((_, why)) = REJECTED_TARGETS.iter().find(|(n, _)| *n == t) {
                return Err(Error::at(
                    &rule.loc,
                    format!("`{t}` is outside the rsmake dialect: {why}"),
                ));
            }
            if let Some(handled) = self.install_special(&t, &rule) {
                handled?;
                continue;
            }
            if t.contains('%') {
                let mut r = rule.clone();
                r.targets = vec![t];
                self.rules.patterns.push(r);
                continue;
            }
            if let Some(pat) = self.as_suffix_rule(&t) {
                // A suffix rule's prerequisites come from its name, so any
                // written on the line are dropped. GNU make warns and carries
                // on; silently dropping them would leave a header dependency
                // that looks declared and is not.
                if !rule.prereqs.is_empty() || !rule.order_only.is_empty() {
                    eprintln!(
                        "rsmake: {}: warning: ignoring prerequisites on suffix rule definition",
                        rule.loc
                    );
                }
                let mut r = rule.clone();
                r.targets = vec![pat.0];
                r.prereqs = vec![pat.1];
                r.order_only = Vec::new();
                self.rules.patterns.push(r);
                continue;
            }
            let mut r = rule.clone();
            r.targets = vec![t.clone()];
            let slot = self.rules.explicit.entry(t.clone()).or_insert_with(|| {
                self.rules.order.push(t.clone());
                Vec::new()
            });
            // Single-colon targets accumulate prerequisites across rules but
            // keep only one recipe; a second recipe is an error rather than a
            // silent replacement, because the discarded one is invisible.
            if !r.double_colon
                && let Some(existing) = slot.iter_mut().find(|e| !e.double_colon)
            {
                existing.prereqs.extend(r.prereqs.clone());
                existing.order_only.extend(r.order_only.clone());
                if !r.recipe.is_empty() {
                    if !existing.recipe.is_empty() {
                        return Err(Error::at(
                            &r.loc,
                            format!(
                                "target `{t}` already has a recipe, defined at {}",
                                existing.loc
                            ),
                        ));
                    }
                    existing.recipe = r.recipe.clone();
                    existing.loc = r.loc.clone();
                }
                continue;
            }
            slot.push(r);
        }
        Ok(())
    }

    /// Returns `Some` when the target is one rsmake handles specially, so the
    /// caller does not also file it as an ordinary rule.
    ///
    /// The `Result` is the reporting channel for the one thing that can go
    /// wrong here: these targets carry settings, not commands, so a recipe
    /// attached to one is dropped. `.DEFAULT` is the exception — a recipe is
    /// the whole content of that rule.
    fn install_special(&mut self, t: &str, rule: &Rule) -> Option<Result<()>> {
        let p = &rule.prereqs;
        if t != ".DEFAULT" && !rule.recipe.iter().all(|l| l.trim().is_empty()) {
            const SETTINGS: &[&str] = &[
                ".PHONY",
                ".PRECIOUS",
                ".SILENT",
                ".IGNORE",
                ".DELETE_ON_ERROR",
                ".NOTPARALLEL",
                ".POSIX",
                ".SUFFIXES",
            ];
            if SETTINGS.contains(&t) {
                return Some(Err(Error::at(
                    &rule.loc,
                    format!(
                        "`{t}` declares a setting and takes no recipe; \
                         the recipe here would never run"
                    ),
                )));
            }
        }
        match t {
            ".PHONY" => self.rules.phony.extend(p.iter().cloned()),
            ".PRECIOUS" => self.rules.precious.extend(p.iter().cloned()),
            ".SILENT" => {
                if p.is_empty() {
                    // Not `opts.silent`: that is the `-s` flag, and it rides
                    // down to sub-makes in `MAKEFLAGS`. `.SILENT:` does not.
                    self.rules.silent_all = true;
                } else {
                    self.rules.silent_targets.extend(p.iter().cloned());
                }
            }
            ".IGNORE" => {
                if p.is_empty() {
                    self.opts.ignore_errors = true;
                } else {
                    self.rules.ignore_targets.extend(p.iter().cloned());
                }
            }
            ".DELETE_ON_ERROR" => self.rules.delete_on_error = true,
            ".NOTPARALLEL" => self.rules.not_parallel = true,
            ".POSIX" => self.rules.posix = true,
            ".SUFFIXES" => {
                if p.is_empty() {
                    self.rules.suffixes.clear();
                } else {
                    self.rules.suffixes.extend(p.iter().cloned());
                }
            }
            ".DEFAULT" => self.rules.default_rule = Some(rule.clone()),
            _ => return None,
        }
        Some(Ok(()))
    }

    /// `.c.o` becomes `%.o: %.c`, and `.c` becomes `%: %.c`, but only when the
    /// suffixes involved are known. Without that check `.PHONY`-style targets
    /// and ordinary dotfiles would be silently reinterpreted as rules.
    fn as_suffix_rule(&self, t: &str) -> Option<(String, String)> {
        if !t.starts_with('.') || t.contains('/') {
            return None;
        }
        let known = |s: &str| self.rules.suffixes.iter().any(|k| k == s);
        for (i, _) in t.char_indices().skip(1) {
            if t[i..].starts_with('.') {
                let (src, tgt) = (&t[..i], &t[i..]);
                if known(src) && known(tgt) {
                    return Some((format!("%{tgt}"), format!("%{src}")));
                }
            }
        }
        known(t).then(|| ("%".to_string(), format!("%{t}")))
    }

    /// Install the built-in pattern rules after the makefile is read, so a
    /// user-written pattern rule for the same target always wins.
    pub fn install_builtin_rules(&mut self) {
        if self.opts.no_builtin_rules {
            return;
        }
        for (target, prereq, recipe) in crate::BUILTIN_RULES {
            self.rules.patterns.push(Rule {
                targets: vec![(*target).to_string()],
                prereqs: vec![(*prereq).to_string()],
                order_only: Vec::new(),
                recipe: vec![(*recipe).to_string()],
                double_colon: false,
                loc: Loc {
                    file: "<builtin>".to_string(),
                    line: 0,
                },
            });
        }
    }
}

impl Rules {
    /// Directories to search for `name`, most specific first. `vpath`
    /// directives are consulted before the blanket `VPATH`.
    pub fn search_dirs(&self, name: &str, vpath_var: &str) -> Vec<String> {
        let mut dirs: Vec<String> = self
            .vpath
            .iter()
            .filter(|(pat, _)| crate::expand::pattern_match(pat, name).is_some())
            .flat_map(|(_, d)| d.iter().cloned())
            .collect();
        dirs.extend(
            vpath_var
                .split([':', ' ', '\t'])
                .filter(|d| !d.is_empty())
                .map(str::to_string),
        );
        dirs
    }
}
