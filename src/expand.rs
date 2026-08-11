//! Variable expansion and the function set.
//!
//! This is the part where a mistake surfaces three packages downstream looking
//! like an unrelated bug, so the four rules that govern it are stated here and
//! tested directly rather than discovered:
//!
//! 1. `:=` expands its right-hand side once, at assignment. `=` stores the
//!    text and expands at every reference, so `$(shell ...)` on the right of a
//!    `=` runs once per reference, not once per makefile.
//! 2. Recipe lines expand when the recipe runs, which is why `$$` in a recipe
//!    reaches the shell as `$`. Everywhere else `$$` yields a literal `$` at
//!    that expansion.
//! 3. Whitespace inside function arguments is significant. `$(if $(X), yes)`
//!    yields a leading space, and GNU make does not strip it either. The one
//!    exception is the *condition* text GNU strips before testing it: the
//!    first argument of `$(if)` and every argument of `$(and)` and `$(or)`.
//!    Their branches are still passed through untouched.
//! 4. A variable that expands to itself is a cycle, reported with its chain,
//!    never truncated into a plausible-looking empty string.

use crate::{AutoVars, Engine, Error, Flavor, MAX_EXPAND_DEPTH, Origin, Result, Var};
use std::path::Path;
use std::process::Command;

/// Arity of each supported function. `usize::MAX` means variadic; a fixed
/// arity means the final argument absorbs any remaining commas, so
/// `$(subst a,b,c,d)` substitutes within the text `c,d`.
fn arity(name: &str) -> Option<usize> {
    Some(match name {
        "strip" | "sort" | "words" | "firstword" | "lastword" | "dir" | "notdir" | "suffix"
        | "basename" | "wildcard" | "realpath" | "abspath" | "eval" | "origin" | "shell"
        | "error" | "warning" | "info" | "flavor" => 1,
        "findstring" | "filter" | "filter-out" | "word" | "addsuffix" | "addprefix" | "join" => 2,
        "subst" | "patsubst" | "wordlist" | "foreach" | "if" => 3,
        "or" | "and" | "call" => usize::MAX,
        _ => return None,
    })
}

/// Real GNU make functions that rsmake does not implement. Falling through to
/// a variable lookup would expand them to the empty string, which is exactly
/// the silent wrong answer the closed dialect exists to prevent: a name that
/// GNU make would have evaluated must not quietly evaluate to nothing.
///
/// A name that is *not* a GNU function still falls through, because
/// `$(nosuchfn a,b)` is an ordinary reference to a variable whose name happens
/// to contain a space, and GNU make expands it to nothing.
const REJECTED_FUNCTIONS: &[(&str, &str)] = &[
    (
        "value",
        "the unexpanded value of a variable is not reachable; use a `:=` copy",
    ),
    ("file", "writing files from an expansion is not supported"),
    ("intcmp", "integer comparison is not supported"),
    ("let", "`let` bindings are not supported; use `$(foreach)`"),
    ("guile", "the Guile extension is not supported"),
];

fn words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Index of the first `needle` at the depth of the enclosing reference, so a
/// `:` or `=` inside a nested `$(...)` does not split a substitution
/// reference.
///
/// Only the delimiter pair that opened the current reference is counted, and
/// the depth is clamped at zero: `$(subst },X,a}b)` contains an unmatched `}`
/// that is ordinary text, and letting it drive the counter negative would
/// suppress every later split.
fn find_top(s: &str, needle: char, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        if c == needle && depth == 0 {
            return Some(i);
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
        }
    }
    None
}

/// Byte index of the closer matching the opener at index 0.
fn match_close(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Split on commas at depth zero, producing at most `max` pieces. The
/// parenthesised form; callers that know which delimiter opened the reference
/// use [`split_args_in`].
pub fn split_args(s: &str, max: usize) -> Vec<&str> {
    split_args_in(s, max, '(', ')')
}

/// Split on commas at the depth of the enclosing reference.
///
/// Only `open`/`close` are counted, and the depth never goes below zero, so an
/// unmatched closer is ordinary text rather than something that silently
/// swallows the rest of the argument list: `$(subst },X,a}b)` splits into
/// three arguments, as in GNU make.
pub fn split_args_in(s: &str, max: usize, open: char, close: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == ',' && depth == 0 && out.len() + 1 < max {
            out.push(&s[start..i]);
            start = i + 1;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
        }
    }
    out.push(&s[start..]);
    out
}

/// Split a pattern at its wildcard `%`, returning the literal text either side.
/// `None` for the second half means the pattern has no wildcard and matches
/// only itself.
///
/// The wildcard is the first `%` preceded by an even number of backslashes;
/// `\%` is a literal percent and `\\%` is a backslash followed by the
/// wildcard, as GNU make documents. Each pair of backslashes that quotes a `%`
/// collapses to one; backslashes that quote nothing are left alone, so a
/// Windows-style path in a pattern is not silently rewritten.
fn split_pattern(pat: &str) -> (String, Option<String>) {
    let mut pre = String::new();
    let mut slashes = 0usize;
    for (i, c) in pat.char_indices() {
        match c {
            '\\' => slashes += 1,
            '%' => {
                for _ in 0..slashes / 2 {
                    pre.push('\\');
                }
                if slashes % 2 == 1 {
                    pre.push('%');
                    slashes = 0;
                } else {
                    return (pre, Some(unquote_percent(&pat[i + 1..])));
                }
            }
            other => {
                for _ in 0..slashes {
                    pre.push('\\');
                }
                slashes = 0;
                pre.push(other);
            }
        }
    }
    for _ in 0..slashes {
        pre.push('\\');
    }
    (pre, None)
}

/// Drop the quoting backslashes in front of any `%`, which past the wildcard
/// is every `%` there is.
fn unquote_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut slashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '%' => {
                for _ in 0..slashes / 2 {
                    out.push('\\');
                }
                slashes = 0;
                out.push('%');
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

/// Match `word` against a pattern containing at most one wildcard `%`,
/// returning the stem. A pattern with no wildcard matches only itself, with an
/// empty stem.
pub fn pattern_match<'a>(pat: &str, word: &'a str) -> Option<&'a str> {
    match split_pattern(pat) {
        (lit, None) => (lit == word).then_some(""),
        (pre, Some(post)) => {
            if word.len() >= pre.len() + post.len()
                && word.starts_with(&pre)
                && word.ends_with(&post)
            {
                Some(&word[pre.len()..word.len() - post.len()])
            } else {
                None
            }
        }
    }
}

/// Substitute `%` in `repl` with the stem `word` yields against `pat`.
/// A word that does not match is passed through unchanged.
pub fn pattern_subst(pat: &str, repl: &str, word: &str) -> String {
    match pattern_match(pat, word) {
        None => word.to_string(),
        Some(stem) => match split_pattern(repl) {
            (lit, None) => lit,
            (pre, Some(post)) => format!("{pre}{stem}{post}"),
        },
    }
}

/// `$(VAR:from=to)`. A `%` in `from` makes it a pattern substitution;
/// otherwise it is a suffix replacement, and an empty `from` appends.
fn subst_ref(from: &str, to: &str, value: &str) -> String {
    if from.contains('%') {
        return words(value)
            .iter()
            .map(|w| pattern_subst(from, to, w))
            .collect::<Vec<_>>()
            .join(" ");
    }
    words(value)
        .iter()
        .map(|w| {
            if from.is_empty() {
                format!("{w}{to}")
            } else if let Some(head) = w.strip_suffix(from) {
                format!("{head}{to}")
            } else {
                (*w).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `*`, `?` and `[...]` over a single path component.
///
/// Memoised on `(pattern pos, name pos)`, and `*` recurses two ways rather
/// than trying every split of the remaining name: `*` either consumes nothing
/// or consumes one more character. There are |pattern| · |name| subproblems
/// and each does O(1) work on top of its recursive calls, so matching really
/// is O(|pattern| · |name|) — the `any`-over-all-splits form memoises the same
/// subproblems but pays a second |name| factor to reach them. Every branch,
/// including an unterminated `[`, records its result, so no path escapes the
/// bound.
fn go(
    p: &[char],
    n: &[char],
    pi: usize,
    ni: usize,
    memo: &mut std::collections::HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(&hit) = memo.get(&(pi, ni)) {
        return hit;
    }
    let hit = match p.get(pi) {
        None => ni == n.len(),
        Some('*') => go(p, n, pi + 1, ni, memo) || (ni < n.len() && go(p, n, pi, ni + 1, memo)),
        Some('?') => ni < n.len() && go(p, n, pi + 1, ni + 1, memo),
        Some('[') => match p[pi + 1..].iter().position(|&c| c == ']') {
            // An unterminated `[` is a literal bracket, as in glob(3).
            None => ni < n.len() && n[ni] == '[' && go(p, n, pi + 1, ni + 1, memo),
            Some(rel_close) => {
                let close = pi + 1 + rel_close;
                if ni >= n.len() || close < pi + 2 {
                    false
                } else {
                    let start = pi + 1;
                    let (negate, set) = match p[start] {
                        '!' | '^' => (true, &p[start + 1..close]),
                        _ => (false, &p[start..close]),
                    };
                    let mut seen = false;
                    let mut i = 0;
                    while i < set.len() {
                        if i + 2 < set.len() && set[i + 1] == '-' {
                            if n[ni] >= set[i] && n[ni] <= set[i + 2] {
                                seen = true;
                            }
                            i += 3;
                        } else {
                            if n[ni] == set[i] {
                                seen = true;
                            }
                            i += 1;
                        }
                    }
                    seen != negate && go(p, n, close + 1, ni + 1, memo)
                }
            }
        },
        Some(&c) => ni < n.len() && n[ni] == c && go(p, n, pi + 1, ni + 1, memo),
    };
    memo.insert((pi, ni), hit);
    hit
}

/// One compiled glob component, reused across a directory's entries so the
/// pattern is decoded and the memo table allocated once per component rather
/// than once per file.
struct Glob {
    pat: Vec<char>,
    name: Vec<char>,
    memo: std::collections::HashMap<(usize, usize), bool>,
}

impl Glob {
    fn new(pat: &str) -> Self {
        Glob {
            pat: pat.chars().collect(),
            name: Vec::new(),
            memo: std::collections::HashMap::new(),
        }
    }

    fn matches(&mut self, name: &str) -> bool {
        self.name.clear();
        self.name.extend(name.chars());
        self.memo.clear();
        go(&self.pat, &self.name, 0, 0, &mut self.memo)
    }
}

fn dir_of(name: &str) -> String {
    match name.rfind('/') {
        Some(i) => name[..=i].to_string(),
        None => "./".to_string(),
    }
}

fn notdir_of(name: &str) -> &str {
    match name.rfind('/') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// The suffix is the last `.` in the final path component, so `dir.d/x` has
/// none. Getting this wrong makes `$(basename)` eat directory names.
fn suffix_of(name: &str) -> &str {
    let base = notdir_of(name);
    match base.rfind('.') {
        Some(i) => &base[i..],
        None => "",
    }
}

impl Engine {
    /// Expand `text` in the context of `auto`.
    pub fn expand(&mut self, text: &str, auto: &AutoVars) -> Result<String> {
        let mut out = String::new();
        self.expand_into(text, auto, &mut out)?;
        Ok(out)
    }

    fn expand_into(&mut self, s: &str, auto: &AutoVars, out: &mut String) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_EXPAND_DEPTH {
            self.depth -= 1;
            let chain = if self.expanding.is_empty() {
                "<no named variable>".to_string()
            } else {
                self.expanding.join(" -> ")
            };
            // A repeat in the chain is a genuine cycle; without one the
            // makefile is merely nested deeper than the cap, and calling that
            // a self-reference sends the reader hunting for a loop that is
            // not there.
            let repeat = self
                .expanding
                .iter()
                .enumerate()
                .find(|(i, name)| self.expanding[..*i].contains(name))
                .map(|(_, name)| name.clone());
            return Err(Error::at(
                &self.loc,
                match repeat {
                    Some(name) => format!(
                        "variable expansion exceeded {MAX_EXPAND_DEPTH} levels, \
                         which means a variable references itself: `{name}` is \
                         expanded while it is already being expanded; chain: {chain}"
                    ),
                    None => format!(
                        "variable expansion nested deeper than the {MAX_EXPAND_DEPTH}-level \
                         limit; no variable repeats, so this is depth rather than a cycle; \
                         chain: {chain}"
                    ),
                },
            ));
        }
        let r = self.expand_scan(s, auto, out);
        self.depth -= 1;
        r
    }

    fn expand_scan(&mut self, s: &str, auto: &AutoVars, out: &mut String) -> Result<()> {
        let mut rest = s;
        loop {
            let Some(p) = rest.find('$') else {
                out.push_str(rest);
                return Ok(());
            };
            out.push_str(&rest[..p]);
            let after = &rest[p + 1..];
            let Some(c) = after.chars().next() else {
                // A trailing `$` is a literal `$`, as in GNU make.
                out.push('$');
                return Ok(());
            };
            match c {
                '$' => {
                    out.push('$');
                    rest = &after[1..];
                }
                '(' | '{' => {
                    let close = if c == '(' { ')' } else { '}' };
                    let Some(end) = match_close(after, c, close) else {
                        return Err(Error::at(
                            &self.loc,
                            format!("unterminated variable reference: {}", &rest[p..]),
                        ));
                    };
                    let v = self.expand_ref(&after[1..end], auto, c, close)?;
                    out.push_str(&v);
                    rest = &after[end + 1..];
                }
                _ => {
                    let name = c.to_string();
                    let v = self.var_value(&name, auto)?;
                    out.push_str(&v);
                    rest = &after[c.len_utf8()..];
                }
            }
        }
    }

    /// The body of a `$(...)`: a function call, a substitution reference, or a
    /// variable name (itself expanded, so `$($(X))` works).
    fn expand_ref(
        &mut self,
        body: &str,
        auto: &AutoVars,
        open: char,
        close: char,
    ) -> Result<String> {
        // The function name is read from the body as written, and must be
        // followed by whitespace. Both halves matter in GNU make:
        // `$( subst a,b,aa)` has leading space and is a variable name, and
        // `$(value)` has no argument list and is a reference to a variable
        // called `value` rather than a call of the function.
        //
        // The whole run of whitespace after the name is skipped, not one byte:
        // `$(subst  a,b,a a)` (two spaces) is `b b` in GNU make, and slicing
        // `i + 1` would leave a stray space at the front of the first argument.
        //
        // ASCII whitespace only, matching GNU make 4.4.1: `$(subst\u{a0}a,...)`
        // is a reference to a variable whose name contains a non-breaking space
        // — undefined, so empty — not a call of `subst`. Rust's Unicode
        // `is_whitespace` would both call the function GNU does not and, with
        // a `+ 1` slice, panic on the character boundary.
        let (name, rest) = match body.find(|c: char| c.is_ascii_whitespace()) {
            Some(i) => (&body[..i], body[i..].trim_ascii_start()),
            None => ("", ""),
        };
        if arity(name).is_some() {
            return self.call_function(name, rest, auto, open, close);
        }
        if let Some((_, why)) = REJECTED_FUNCTIONS.iter().find(|(f, _)| *f == name) {
            return Err(Error::at(
                &self.loc,
                format!("`$({name} ...)` is outside the rsmake dialect: {why}"),
            ));
        }

        if let Some(ci) = find_top(body, ':', open, close)
            && let Some(ei) = find_top(&body[ci + 1..], '=', open, close)
        {
            let tail = &body[ci + 1..];
            let vname = self.expand(&body[..ci], auto)?;
            let from = self.expand(&tail[..ei], auto)?;
            let to = self.expand(&tail[ei + 1..], auto)?;
            let value = self.var_value(&vname, auto)?;
            return Ok(subst_ref(&from, &to, &value));
        }

        let vname = self.expand(body, auto)?;
        self.var_value(&vname, auto)
    }

    /// Automatic variables, including the `D` and `F` forms. These are not
    /// merely convenient: without them `$(@D)` would expand to the empty
    /// string, which is a silently wrong answer rather than an error.
    fn auto_lookup(&self, name: &str, auto: &AutoVars) -> Option<String> {
        let base = |c: char| -> Option<String> {
            Some(match c {
                '@' => auto.target.clone(),
                '<' => auto.first_prereq.clone(),
                '^' => auto.prereqs.join(" "),
                '+' => auto.prereqs_all.join(" "),
                '?' => auto.newer.join(" "),
                '*' => auto.stem.clone(),
                '%' => auto.member.clone(),
                _ => return None,
            })
        };
        let mut cs = name.chars();
        let first = cs.next()?;
        match (cs.next(), cs.next()) {
            (None, _) => base(first),
            (Some('D'), None) => Some(
                base(first)?
                    .split_whitespace()
                    .map(|w| {
                        let d = dir_of(w).trim_end_matches('/').to_string();
                        if d.is_empty() { "/".to_string() } else { d }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            (Some('F'), None) => Some(
                base(first)?
                    .split_whitespace()
                    .map(notdir_of)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        }
    }

    /// Resolve a name to its value, expanding recursive flavours at the point
    /// of reference. An undefined variable is the empty string, as in make.
    pub fn var_value(&mut self, name: &str, auto: &AutoVars) -> Result<String> {
        if let Some(v) = self.auto_lookup(name, auto) {
            return Ok(v);
        }
        let Some(var) = self.vars.get(name) else {
            return Ok(String::new());
        };
        if var.flavor == Flavor::Simple {
            return Ok(var.value.clone());
        }
        let text = var.value.clone();
        self.expanding.push(name.to_string());
        let r = self.expand(&text, auto);
        self.expanding.pop();
        r
    }

    fn call_function(
        &mut self,
        name: &str,
        raw: &str,
        auto: &AutoVars,
        open: char,
        close: char,
    ) -> Result<String> {
        let n = arity(name).expect("caller checked");
        let args = split_args_in(raw, n, open, close);
        let arg = |i: usize| -> &str { args.get(i).copied().unwrap_or("") };

        // Lazily-expanded functions come first: expanding their arguments up
        // front would run the untaken branch of `$(if)`, defeat the
        // short-circuit in `$(and)`, and expand `$(foreach)`'s body once with
        // the loop variable unbound.
        match name {
            // GNU make strips the *condition* text before expanding it — the
            // first argument of `if`, and every argument of `and`/`or`, since
            // each of those is both a condition and the result. The branches
            // of `if` are not stripped: `$(if $(X), yes)` keeps its space.
            "if" => {
                let cond = self.expand(arg(0).trim(), auto)?;
                let branch = if cond.is_empty() { arg(2) } else { arg(1) };
                return self.expand(branch, auto);
            }
            "or" => {
                for a in &args {
                    let v = self.expand(a.trim(), auto)?;
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
                return Ok(String::new());
            }
            "and" => {
                let mut last = String::new();
                for a in &args {
                    last = self.expand(a.trim(), auto)?;
                    if last.is_empty() {
                        return Ok(String::new());
                    }
                }
                return Ok(last);
            }
            "foreach" => {
                let var = self.expand(arg(0), auto)?.trim().to_string();
                let list = self.expand(arg(1), auto)?;
                let body = arg(2).to_string();
                let saved = self.vars.remove(&var);
                let mut out: Vec<String> = Vec::new();
                for w in list.split_whitespace() {
                    self.vars.force(&var, w.to_string(), Origin::Automatic);
                    out.push(self.expand(&body, auto)?);
                }
                self.vars.restore(&var, saved);
                return Ok(out.join(" "));
            }
            "call" => {
                let fname = self.expand(arg(0), auto)?.trim().to_string();
                let Some(var) = self.vars.get(&fname) else {
                    return Ok(String::new());
                };
                let body = var.value.clone();
                // Arguments are expanded in the *caller's* scope, before the
                // callee's positionals replace them.
                let mut vals = Vec::with_capacity(args.len());
                for a in &args {
                    vals.push(self.expand(a, auto)?);
                }
                // GNU make pushes a fresh scope, so a positional this call does
                // not supply is empty in the callee rather than leaking the
                // enclosing call's. Saving only the ones bound here would let
                // `$(call g,X,Y)` where `g = $(call f,inner)` show `$(2)` as
                // `Y` inside `f`, which GNU leaves empty.
                //
                // Only *positional* bindings are shadowed. A makefile is free
                // to define an ordinary variable named `2`, and GNU keeps it
                // visible inside a call that does not supply a second argument:
                // with `2 = globaltwo`, `$(call f,onearg)` where
                // `f = [$(1)][$(2)]` is `[onearg][globaltwo]`. Shadowing every
                // digit-named variable would blank it.
                let numeric: Vec<String> = self
                    .vars
                    .iter()
                    .filter(|(_, v)| v.origin == Origin::Automatic)
                    .map(|(k, _)| k.clone())
                    .filter(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
                    .collect();
                let mut saved: Vec<(String, Option<Var>)> = Vec::new();
                for k in numeric {
                    let old = self.vars.remove(&k);
                    saved.push((k, old));
                }
                for (i, v) in vals.into_iter().enumerate() {
                    let key = i.to_string();
                    if !saved.iter().any(|(k, _)| *k == key) {
                        // The prior value has to be *taken*, not assumed
                        // absent: a makefile may define an ordinary `1 = x`,
                        // and pushing `None` here would restore "undefined"
                        // after the call and destroy it. GNU restores it.
                        saved.push((key.clone(), self.vars.remove(&key)));
                    }
                    self.vars.force(&key, v, Origin::Automatic);
                }
                self.expanding.push(format!("call {fname}"));
                let r = self.expand(&body, auto);
                self.expanding.pop();
                for (k, v) in saved {
                    self.vars.restore(&k, v);
                }
                return r;
            }
            "eval" => {
                let text = self.expand(arg(0), auto)?;
                let loc = self.loc.clone();
                self.parse_text(&text, &loc.file)?;
                self.loc = loc;
                return Ok(String::new());
            }
            "origin" => {
                let vname = self.expand(arg(0), auto)?;
                let vname = vname.trim();
                if self.auto_lookup(vname, auto).is_some() {
                    return Ok("automatic".to_string());
                }
                return Ok(match self.vars.get(vname) {
                    None => "undefined".to_string(),
                    Some(v) => v.origin.name().to_string(),
                });
            }
            "flavor" => {
                let vname = self.expand(arg(0), auto)?;
                return Ok(match self.vars.get(vname.trim()) {
                    None => "undefined".to_string(),
                    Some(v) if v.flavor == Flavor::Simple => "simple".to_string(),
                    Some(_) => "recursive".to_string(),
                });
            }
            _ => {}
        }

        // Everything below takes fully expanded arguments.
        let mut a: Vec<String> = Vec::with_capacity(args.len());
        for x in &args {
            a.push(self.expand(x, auto)?);
        }
        let g = |i: usize| -> &str { a.get(i).map(String::as_str).unwrap_or("") };
        let joined = |v: Vec<String>| v.join(" ");

        Ok(match name {
            "subst" => {
                if g(0).is_empty() {
                    // GNU: an empty search appends the replacement once.
                    format!("{}{}", g(2), g(1))
                } else {
                    g(2).replace(g(0), g(1))
                }
            }
            "patsubst" => joined(
                words(g(2))
                    .iter()
                    .map(|w| pattern_subst(g(0), g(1), w))
                    .collect(),
            ),
            "strip" => words(g(0)).join(" "),
            "findstring" => {
                if g(1).contains(g(0)) {
                    g(0).to_string()
                } else {
                    String::new()
                }
            }
            "filter" | "filter-out" => {
                let keep = name == "filter";
                let pats = words(g(0));
                joined(
                    words(g(1))
                        .iter()
                        .filter(|w| pats.iter().any(|p| pattern_match(p, w).is_some()) == keep)
                        .map(|w| (*w).to_string())
                        .collect(),
                )
            }
            "sort" => {
                let mut ws: Vec<&str> = words(g(0));
                ws.sort_unstable();
                ws.dedup();
                ws.join(" ")
            }
            "word" => {
                let n = self.parse_index(g(0), "word", "first")?;
                if n == 0 {
                    return Err(Error::at(
                        &self.loc,
                        "first argument to 'word' function must be greater than 0".to_string(),
                    ));
                }
                words(g(1))
                    .get(n - 1)
                    .copied()
                    .unwrap_or("")
                    .to_string()
            }
            "words" => words(g(0)).len().to_string(),
            "wordlist" => {
                let s = self.parse_index(g(0), "wordlist", "first")?;
                let e = self.parse_index(g(1), "wordlist", "second")?;
                if s == 0 {
                    return Err(Error::at(
                        &self.loc,
                        format!("invalid first argument to 'wordlist' function: '{s}'"),
                    ));
                }
                let ws = words(g(2));
                if s > ws.len() || e < s {
                    String::new()
                } else {
                    ws[s - 1..e.min(ws.len())].join(" ")
                }
            }
            "firstword" => words(g(0)).first().copied().unwrap_or("").to_string(),
            "lastword" => words(g(0)).last().copied().unwrap_or("").to_string(),
            "dir" => joined(words(g(0)).iter().map(|w| dir_of(w)).collect()),
            "notdir" => joined(
                words(g(0))
                    .iter()
                    .map(|w| notdir_of(w).to_string())
                    .collect(),
            ),
            "suffix" => joined(
                words(g(0))
                    .iter()
                    .map(|w| suffix_of(w).to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            "basename" => joined(
                words(g(0))
                    .iter()
                    .map(|w| {
                        let s = suffix_of(w);
                        w[..w.len() - s.len()].to_string()
                    })
                    .collect(),
            ),
            "addsuffix" => joined(words(g(1)).iter().map(|w| format!("{w}{}", g(0))).collect()),
            "addprefix" => joined(words(g(1)).iter().map(|w| format!("{}{w}", g(0))).collect()),
            "join" => {
                let (l, r) = (words(g(0)), words(g(1)));
                joined(
                    (0..l.len().max(r.len()))
                        .map(|i| {
                            format!(
                                "{}{}",
                                l.get(i).copied().unwrap_or(""),
                                r.get(i).copied().unwrap_or("")
                            )
                        })
                        .collect(),
                )
            }
            "wildcard" => joined(
                words(g(0))
                    .iter()
                    .flat_map(|p| expand_wildcard(p))
                    .collect(),
            ),
            "realpath" => joined(
                words(g(0))
                    .iter()
                    .filter_map(|w| std::fs::canonicalize(w).ok())
                    .filter_map(|p| p.to_str().map(str::to_string))
                    .collect(),
            ),
            "abspath" => joined(
                words(g(0))
                    .iter()
                    .filter_map(|w| absolute(Path::new(w)))
                    .collect(),
            ),
            "shell" => self.run_shell_function(g(0))?,
            "error" => return Err(Error::at(&self.loc, g(0).to_string())),
            "warning" => {
                eprintln!("{}: {}", self.loc, g(0));
                self.warnings += 1;
                String::new()
            }
            "info" => {
                println!("{}", g(0));
                // Counted separately from `$(warning)`: the two are different
                // diagnostics on different streams, and a single field could
                // not tell a test which of them actually fired.
                self.infos += 1;
                String::new()
            }
            other => {
                return Err(Error::at(
                    &self.loc,
                    format!("unimplemented function '{other}'"),
                ));
            }
        })
    }

    /// `ordinal` names which argument is at fault. Reporting a bad second
    /// argument as the first sends the reader to the wrong end of the call,
    /// and GNU make names it correctly.
    fn parse_index(&self, s: &str, func: &str, ordinal: &str) -> Result<usize> {
        s.trim().parse::<usize>().map_err(|_| {
            Error::at(
                &self.loc,
                format!("invalid {ordinal} argument to '{func}' function: '{s}'"),
            )
        })
    }

    /// `$(shell ...)`: stdout with trailing newlines dropped and interior
    /// newlines turned into spaces. `.SHELLSTATUS` is set so a makefile can
    /// tell an empty success from a failure, which is otherwise invisible.
    ///
    /// Only stdout is captured. The child's stderr is inherited, as in GNU
    /// make: a compiler probe that fails must say why on the terminal rather
    /// than have its diagnostic swallowed by the expansion that ran it.
    fn run_shell_function(&mut self, cmd: &str) -> Result<String> {
        // `SHELL` is expanded like any other variable; `SHELL = $(BASH)` names
        // a program, not a literal `$(BASH)` to hand to exec.
        let raw = self.vars.value("SHELL").to_string();
        let shell = match self.expand(&raw, &AutoVars::default())?.trim() {
            "" => "/bin/sh".to_string(),
            s => s.to_string(),
        };
        let out = Command::new(&shell)
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output();
        let (status, stdout) = match out {
            Ok(o) => (o.status.code().unwrap_or(1), o.stdout),
            Err(e) => {
                return Err(Error::at(
                    &self.loc,
                    format!("could not run shell '{shell}': {e}"),
                ));
            }
        };
        self.vars.set(
            ".SHELLSTATUS",
            Var {
                value: status.to_string(),
                flavor: Flavor::Simple,
                origin: Origin::File,
                exported: false,
            },
        );
        Ok(String::from_utf8_lossy(&stdout)
            .trim_end_matches('\n')
            .replace('\n', " "))
    }
}

/// `$(abspath)` resolves `.` and `..` textually without touching the
/// filesystem — unlike `$(realpath)`, it must succeed for paths that do not
/// exist — so it is done by hand rather than through `std::fs::canonicalize`.
fn absolute(p: &Path) -> Option<String> {
    let base = if p.is_absolute() {
        std::path::PathBuf::new()
    } else {
        std::env::current_dir().ok()?
    };
    let mut parts: Vec<String> = base
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .filter(|c| c != "/")
        .collect();
    for c in p.to_str()?.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

const GLOB_CHARS: [char; 3] = ['*', '?', '['];

fn glob_join(base: &str, name: &str) -> String {
    match base {
        "" => name.to_string(),
        "/" => format!("/{name}"),
        _ => format!("{base}/{name}"),
    }
}

/// `$(wildcard)`. Every path component is expanded, not just the last, because
/// `src/*/*.c` and `s?c/*.c` are both things makefiles write.
///
/// Results are sorted at each level: readdir order is not stable across
/// filesystems, and an unstable `$(wildcard)` makes link order — and therefore
/// the build output — nondeterministic. The sort is byte order, not glob(3)'s
/// locale collation — ordering diverges from GNU on locale-sensitive names.
///
/// A trailing `/` restricts the last level to directories and is kept on the
/// results, so `$(wildcard */)` names directories as `src/`. Returning plain
/// files for it would make `$(wildcard */)` a list nobody can `cd` into.
fn expand_wildcard(pat: &str) -> Vec<String> {
    let dir_only = pat.len() > 1 && pat.ends_with('/');
    let core = if dir_only {
        pat.trim_end_matches('/')
    } else {
        pat
    };
    if !core.contains(GLOB_CHARS) {
        // A name with no wildcards is an existence test. The trailing slash
        // comes back only if the name really is a directory, which is what
        // glob(3) — and therefore GNU make — does with it.
        if !Path::new(core).exists() {
            return Vec::new();
        }
        return match dir_only && Path::new(core).is_dir() {
            true => vec![format!("{core}/")],
            false => vec![core.to_string()],
        };
    }
    let rooted = core.starts_with('/');
    let mut cur = vec![if rooted {
        "/".to_string()
    } else {
        String::new()
    }];

    for comp in core.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        // Compiled once per component rather than once per directory entry.
        let mut glob = comp.contains(GLOB_CHARS).then(|| Glob::new(comp));
        for base in &cur {
            let Some(glob) = glob.as_mut() else {
                let joined = glob_join(base, comp);
                if Path::new(&joined).exists() {
                    next.push(joined);
                }
                continue;
            };
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut hits: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                // A leading dot matches only an explicit leading dot, as in glob(3).
                .filter(|n| (!n.starts_with('.') || comp.starts_with('.')) && glob.matches(n))
                .map(|n| glob_join(base, &n))
                .collect();
            hits.sort();
            next.extend(hits);
        }
        cur = next;
    }
    if dir_only {
        return cur
            .into_iter()
            .filter(|p| Path::new(p).is_dir())
            .map(|p| format!("{p}/"))
            .collect();
    }
    cur
}
