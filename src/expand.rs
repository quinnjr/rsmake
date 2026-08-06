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
//!    yields a leading space, and GNU make does not strip it either.
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

fn words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Index of the first `needle` at paren/brace depth zero, so a `:` or `=`
/// inside a nested `$(...)` does not split a substitution reference.
fn find_top(s: &str, needle: char) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            c if c == needle && depth == 0 => return Some(i),
            _ => {}
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

/// Split on commas at depth zero, producing at most `max` pieces.
pub fn split_args(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            ',' if depth == 0 && out.len() + 1 < max => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Match `word` against a pattern containing at most one `%`, returning the
/// stem. A pattern with no `%` matches only itself, with an empty stem.
pub fn pattern_match<'a>(pat: &str, word: &'a str) -> Option<&'a str> {
    match pat.find('%') {
        None => (pat == word).then_some(""),
        Some(p) => {
            let (pre, post) = (&pat[..p], &pat[p + 1..]);
            if word.len() >= pre.len() + post.len() && word.starts_with(pre) && word.ends_with(post)
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
        Some(stem) => match repl.find('%') {
            None => repl.to_string(),
            Some(r) => format!("{}{}{}", &repl[..r], stem, &repl[r + 1..]),
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
fn glob_match(pat: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().position(|&c| c == ']') else {
                    return !n.is_empty() && n[0] == '[' && go(&p[1..], &n[1..]);
                };
                if n.is_empty() || close < 2 {
                    return false;
                }
                let (negate, set) = match p[1] {
                    '!' | '^' => (true, &p[2..close]),
                    _ => (false, &p[1..close]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        if n[0] >= set[i] && n[0] <= set[i + 2] {
                            hit = true;
                        }
                        i += 3;
                    } else {
                        if n[0] == set[i] {
                            hit = true;
                        }
                        i += 1;
                    }
                }
                hit != negate && go(&p[close + 1..], &n[1..])
            }
            Some(&c) => !n.is_empty() && n[0] == c && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
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
            return Err(Error::at(
                &self.loc,
                format!(
                    "variable expansion exceeded {MAX_EXPAND_DEPTH} levels, \
                     which means a variable references itself; chain: {chain}"
                ),
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
                    let body = after[1..end].to_string();
                    let v = self.expand_ref(&body, auto)?;
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
    fn expand_ref(&mut self, body: &str, auto: &AutoVars) -> Result<String> {
        let trimmed = body.trim_start();
        let (name, rest) = match trimmed.find(char::is_whitespace) {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => (trimmed, ""),
        };
        if arity(name).is_some() {
            return self.call_function(name, rest, auto);
        }

        if let Some(ci) = find_top(body, ':')
            && let Some(ei) = find_top(&body[ci + 1..], '=')
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

    fn call_function(&mut self, name: &str, raw: &str, auto: &AutoVars) -> Result<String> {
        let n = arity(name).expect("caller checked");
        let args = split_args(raw, n);
        let arg = |i: usize| -> &str { args.get(i).copied().unwrap_or("") };

        // Lazily-expanded functions come first: expanding their arguments up
        // front would run the untaken branch of `$(if)`, defeat the
        // short-circuit in `$(and)`, and expand `$(foreach)`'s body once with
        // the loop variable unbound.
        match name {
            "if" => {
                let cond = self.expand(arg(0), auto)?;
                let branch = if cond.is_empty() { arg(2) } else { arg(1) };
                return self.expand(branch, auto);
            }
            "or" => {
                for a in &args {
                    let v = self.expand(a, auto)?;
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
                return Ok(String::new());
            }
            "and" => {
                let mut last = String::new();
                for a in &args {
                    last = self.expand(a, auto)?;
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
                let mut saved = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let v = self.expand(a, auto)?;
                    let key = i.to_string();
                    saved.push((key.clone(), self.vars.remove(&key)));
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
            "subst" => g(2).replace(g(0), g(1)),
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
                let n = self.parse_index(g(0), "word")?;
                words(g(1))
                    .get(n.wrapping_sub(1))
                    .copied()
                    .unwrap_or("")
                    .to_string()
            }
            "words" => words(g(0)).len().to_string(),
            "wordlist" => {
                let s = self.parse_index(g(0), "wordlist")?;
                let e = self.parse_index(g(1), "wordlist")?;
                let ws = words(g(2));
                if s == 0 || s > ws.len() || e < s {
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
                self.warnings += 1;
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

    fn parse_index(&self, s: &str, func: &str) -> Result<usize> {
        s.trim().parse::<usize>().map_err(|_| {
            Error::at(
                &self.loc,
                format!("non-numeric first argument to '{func}' function: '{s}'"),
            )
        })
    }

    /// `$(shell ...)`: stdout with trailing newlines dropped and interior
    /// newlines turned into spaces. `.SHELLSTATUS` is set so a makefile can
    /// tell an empty success from a failure, which is otherwise invisible.
    fn run_shell_function(&mut self, cmd: &str) -> Result<String> {
        let shell = match self.vars.value("SHELL") {
            "" => "/bin/sh".to_string(),
            s => s.to_string(),
        };
        let out = Command::new(&shell).arg("-c").arg(cmd).output();
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
/// the build output — nondeterministic.
fn expand_wildcard(pat: &str) -> Vec<String> {
    if !pat.contains(GLOB_CHARS) {
        return if Path::new(pat).exists() {
            vec![pat.to_string()]
        } else {
            Vec::new()
        };
    }
    let rooted = pat.starts_with('/');
    let mut cur = vec![if rooted {
        "/".to_string()
    } else {
        String::new()
    }];

    for comp in pat.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &cur {
            if !comp.contains(GLOB_CHARS) {
                let joined = glob_join(base, comp);
                if Path::new(&joined).exists() {
                    next.push(joined);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut hits: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                // A leading dot matches only an explicit leading dot, as in glob(3).
                .filter(|n| glob_match(comp, n) && (!n.starts_with('.') || comp.starts_with('.')))
                .map(|n| glob_join(base, &n))
                .collect();
            hits.sort();
            next.extend(hits);
        }
        cur = next;
    }
    cur
}
