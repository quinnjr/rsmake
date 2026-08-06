//! Rule selection, dependency discovery, and staleness.
//!
//! Discovery and execution are two phases. Discovery walks the rules once,
//! single-threaded, and produces a DAG with every node resolved; execution
//! then runs that DAG serially or across `-j` workers. Splitting them is what
//! makes parallel execution a scheduling problem rather than a re-entrancy
//! problem, and it means cycle detection happens before any recipe runs.

use crate::expand::pattern_match;
use crate::{AutoVars, Engine, Error, Flavor, Loc, Origin, Result, TargetVar, Var};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One rule instance attached to a target. A single-colon target has exactly
/// one; a double-colon target has one per rule, each with its own
/// prerequisites and recipe, evaluated for staleness independently.
#[derive(Debug)]
pub struct NodeEntry {
    pub deps: Vec<usize>,
    pub order_only: Vec<usize>,
    pub prereq_names: Vec<String>,
    pub recipe: Vec<String>,
    pub stem: String,
    pub loc: Loc,
}

#[derive(Debug)]
pub struct Node {
    pub target: String,
    /// Path the target resolved to, which differs from `target` when `vpath`
    /// found it elsewhere.
    pub path: PathBuf,
    pub entries: Vec<NodeEntry>,
    pub phony: bool,
    pub mtime: Option<SystemTime>,
    /// Target-specific variables, including those inherited from the parent
    /// that pulled this node in.
    pub tvars: Vec<TargetVar>,
    /// Set by the executor when a recipe actually ran, so a dependent knows to
    /// rebuild even if timestamps did not move (a same-second rebuild would
    /// otherwise look up to date).
    pub rebuilt: bool,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub index: HashMap<String, usize>,
}

impl Graph {
    pub fn get(&self, name: &str) -> Option<&Node> {
        self.index.get(name).map(|&i| &self.nodes[i])
    }
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

impl Engine {
    /// Resolve a name to the file it refers to, consulting `vpath` and `VPATH`.
    ///
    /// A name with an explicit rule keeps its own spelling: the rule says where
    /// the file will be, and searching would find a stale copy elsewhere.
    fn resolve_path(&mut self, name: &str) -> PathBuf {
        let local = PathBuf::from(name);
        if local.exists() || self.rules.explicit.contains_key(name) {
            return local;
        }
        let vpath_var = self
            .var_value("VPATH", &AutoVars::default())
            .unwrap_or_default();
        for d in self.rules.search_dirs(name, &vpath_var) {
            let cand = Path::new(&d).join(name);
            if cand.exists() {
                return cand;
            }
        }
        local
    }

    /// Can `name` be supplied, without committing to building it? Used to
    /// decide whether a pattern rule's prerequisites are satisfiable.
    fn can_supply(&mut self, name: &str) -> bool {
        self.rules.explicit.contains_key(name) || self.resolve_path(name).exists()
    }

    /// Choose a pattern rule for `target`. Declaration order wins, and the
    /// built-ins are appended last so a user's own `%.o: %.c` always takes
    /// precedence over the built-in of the same shape.
    fn match_pattern(&mut self, target: &str) -> Option<(crate::Rule, String)> {
        let candidates: Vec<(usize, String)> = self
            .rules
            .patterns
            .iter()
            .enumerate()
            .filter_map(|(i, r)| pattern_match(&r.targets[0], target).map(|s| (i, s.to_string())))
            .collect();
        for (i, stem) in candidates {
            let rule = self.rules.patterns[i].clone();
            // The stem goes into the `%`, so `%.c` with stem `a` is `a.c`.
            let fill = |p: &String| p.replacen('%', &stem, 1);
            let prereqs: Vec<String> = rule.prereqs.iter().map(fill).collect();
            if prereqs.iter().all(|p| self.can_supply(p)) {
                let mut r = rule;
                r.prereqs = prereqs;
                r.order_only = r.order_only.iter().map(fill).collect();
                return Some((r, stem));
            }
        }
        None
    }

    /// Build the DAG rooted at `goal`.
    ///
    /// `stack` carries the in-progress chain so a cycle is reported with the
    /// full path that closed it. rsmake does not break cycles by dropping an
    /// edge: the resulting build order would be arbitrary and the failure
    /// would move to a different target on the next run.
    pub fn discover(
        &mut self,
        goal: &str,
        graph: &mut Graph,
        stack: &mut Vec<String>,
        inherited: &[TargetVar],
    ) -> Result<usize> {
        if let Some(&i) = graph.index.get(goal) {
            if stack.iter().any(|s| s == goal) {
                let mut chain = stack.clone();
                chain.push(goal.to_string());
                return Err(Error::new(format!(
                    "dependency cycle: {}",
                    chain.join(" -> ")
                )));
            }
            return Ok(i);
        }
        if stack.iter().any(|s| s == goal) {
            let mut chain = stack.clone();
            chain.push(goal.to_string());
            return Err(Error::new(format!(
                "dependency cycle: {}",
                chain.join(" -> ")
            )));
        }

        let path = self.resolve_path(goal);
        let phony = self.rules.is_phony(goal);
        let mut tvars = inherited.to_vec();
        if let Some(own) = self.rules.target_vars.get(goal) {
            tvars.extend(own.iter().cloned());
        }

        // Reserve the slot before recursing so a self-referential rule is
        // caught by the stack check rather than looping.
        let idx = graph.nodes.len();
        graph.nodes.push(Node {
            target: goal.to_string(),
            path: path.clone(),
            entries: Vec::new(),
            phony,
            mtime: if phony { None } else { mtime_of(&path) },
            tvars: tvars.clone(),
            rebuilt: false,
        });
        graph.index.insert(goal.to_string(), idx);

        let explicit = self.rules.explicit.get(goal).cloned().unwrap_or_default();
        let mut instances: Vec<(crate::Rule, String)> = Vec::new();

        if explicit.is_empty() {
            if let Some(hit) = self.match_pattern(goal) {
                instances.push(hit);
            }
        } else {
            for r in explicit {
                // A target with prerequisites but no recipe may still take its
                // recipe from a pattern rule; without this, `main.o: defs.h`
                // alongside the built-in `%.o: %.c` builds nothing.
                if r.recipe.is_empty()
                    && let Some((pat, stem)) = self.match_pattern(goal)
                {
                    let mut merged = r.clone();
                    merged.recipe = pat.recipe;
                    for p in pat.prereqs {
                        if !merged.prereqs.contains(&p) {
                            merged.prereqs.push(p);
                        }
                    }
                    instances.push((merged, stem));
                    continue;
                }
                instances.push((r, String::new()));
            }
        }

        if instances.is_empty() {
            if let Some(d) = self.rules.default_rule.clone() {
                instances.push((d, String::new()));
            } else if path.exists() {
                // A source file with no rule is a leaf, not an error.
                return Ok(idx);
            } else {
                return Err(Error::new(format!(
                    "no rule to make target `{goal}`{}",
                    stack
                        .last()
                        .map(|p| format!(", needed by `{p}`"))
                        .unwrap_or_default()
                )));
            }
        }

        stack.push(goal.to_string());
        let mut entries = Vec::new();
        for (rule, stem) in instances {
            let mut deps = Vec::new();
            let mut names = Vec::new();
            for p in &rule.prereqs {
                let resolved = self.resolve_path(p).to_string_lossy().into_owned();
                let name = if resolved.is_empty() {
                    p.clone()
                } else {
                    resolved
                };
                deps.push(self.discover(&name, graph, stack, &tvars)?);
                names.push(name);
            }
            let mut order_only = Vec::new();
            for p in &rule.order_only {
                let resolved = self.resolve_path(p).to_string_lossy().into_owned();
                order_only.push(self.discover(&resolved, graph, stack, &tvars)?);
            }
            entries.push(NodeEntry {
                deps,
                order_only,
                prereq_names: names,
                recipe: rule.recipe.clone(),
                stem,
                loc: rule.loc.clone(),
            });
        }
        stack.pop();
        graph.nodes[idx].entries = entries;
        Ok(idx)
    }

    /// Which prerequisites of `entry` are newer than the target, and whether
    /// the target must be rebuilt at all.
    ///
    /// Equal timestamps mean up to date: a target is stale only when strictly
    /// older than a prerequisite. A prerequisite dated in the future is a
    /// clock-skew warning and forces a rebuild, because silently treating it
    /// as up to date is indistinguishable from a correct build until much later.
    pub fn staleness(&self, graph: &Graph, idx: usize, entry: usize) -> (bool, Vec<String>) {
        let node = &graph.nodes[idx];
        let e = &node.entries[entry];
        let mut newer = Vec::new();
        let mut stale = self.opts.always_make || node.phony || node.mtime.is_none();

        for (di, dep) in e.deps.iter().enumerate() {
            let d = &graph.nodes[*dep];
            let name = e
                .prereq_names
                .get(di)
                .cloned()
                .unwrap_or_else(|| d.target.clone());
            let counts = match (node.mtime, d.mtime) {
                // A target that does not exist is older than everything, so
                // every prerequisite is "newer" and lands in `$?`. Leaving
                // `$?` empty here is the difference between a link line that
                // names its objects and one that names nothing.
                (None, _) => true,
                _ if d.rebuilt || d.phony => true,
                (Some(t), Some(p)) => p > t,
                (Some(_), None) => false,
            };
            if counts {
                stale = true;
                if !newer.contains(&name) {
                    newer.push(name);
                }
            }
        }

        if let Some(t) = node.mtime
            && t > SystemTime::now()
        {
            eprintln!(
                "rsmake: warning: `{}` has a modification time in the future; rebuilding",
                node.target
            );
            stale = true;
        }
        (stale, newer)
    }

    /// Bind the automatic variables for one rule instance.
    pub fn auto_vars(&self, graph: &Graph, idx: usize, entry: usize, newer: &[String]) -> AutoVars {
        let node = &graph.nodes[idx];
        let e = &node.entries[entry];
        let mut dedup: Vec<String> = Vec::new();
        for p in &e.prereq_names {
            if !dedup.contains(p) {
                dedup.push(p.clone());
            }
        }
        AutoVars {
            target: node.target.clone(),
            stem: e.stem.clone(),
            member: String::new(),
            first_prereq: e.prereq_names.first().cloned().unwrap_or_default(),
            prereqs: dedup,
            prereqs_all: e.prereq_names.clone(),
            newer: newer.to_vec(),
        }
    }

    /// Apply a node's target-specific variables, returning the previous values
    /// so the caller can restore them. Target-specific scope is a stack, not a
    /// global assignment: leaking one into a sibling target is a bug that only
    /// shows up when build order changes.
    pub fn push_target_vars(&mut self, tvars: &[TargetVar]) -> Vec<(String, Option<Var>)> {
        let mut saved = Vec::new();
        for tv in tvars {
            saved.push((tv.name.clone(), self.vars.get(&tv.name).cloned()));
            let value = if tv.append {
                let old = self
                    .vars
                    .get(&tv.name)
                    .map(|v| v.value.clone())
                    .unwrap_or_default();
                if old.is_empty() {
                    tv.value.clone()
                } else {
                    format!("{old} {}", tv.value)
                }
            } else {
                tv.value.clone()
            };
            self.vars.force(&tv.name, value, Origin::File);
            if tv.flavor == Flavor::Recursive
                && let Some(v) = self.vars.get(&tv.name).cloned()
            {
                self.vars.force(&tv.name, v.value, Origin::File);
            }
        }
        saved
    }

    pub fn pop_target_vars(&mut self, saved: Vec<(String, Option<Var>)>) {
        for (name, old) in saved.into_iter().rev() {
            self.vars.restore(&name, old);
        }
    }

    /// The default goal: the first explicit target in file order that is
    /// neither special nor a pattern. Taken from insertion order rather than
    /// the hash map, which would make the default goal an allocation accident.
    pub fn default_goal(&self) -> Option<String> {
        self.rules
            .order
            .iter()
            .find(|t| !t.starts_with('.') && !t.contains('%'))
            .cloned()
    }
}
